//! Router-side WORKER REGISTRY: the source of truth for "which repos have a live
//! worker, on what port, and is it still alive?" plus single-flight spawn.
//!
//! ## State machine (per repo)
//!
//! ```text
//!   (absent) ──spawn──▶ Spawning ──ready line──▶ Ready{port,child}
//!        ▲                  │                          │
//!        │                  │ spawn/handshake failed   │ child exited / killed
//!        └──────────────────┴──────────────────────────┘
//! ```
//!
//! - **Spawning**: a worker process has been launched; we are awaiting its
//!   readiness handshake (stdout line). Concurrent requests for the same repo
//!   DO NOT spawn a second process — they await the same `ready` notify
//!   (single-flight, mirroring `IndexEngine::warm_locks` / `store::OPEN_GATES`
//!   lifted to the process level).
//! - **Ready**: the worker accepted on `port`; the router proxies to it. We hold
//!   the `Child` handle so we can `try_wait` (reuse-safe liveness — never PID
//!   lookup, which is vulnerable to PID reuse and the `STILL_ACTIVE`=259 trap)
//!   and so the OS parent/child relationship + Job Object keep it from orphaning.
//! - **absent/Dead**: no live worker; the next request spawns one.
//!
//! ## Why we keep the `Child`
//!
//! `Child::try_wait()` checks liveness via the retained OS handle, not the PID,
//! so it is immune to PID reuse and never misreads exit code 259. Before
//! respawning we confirm the prior child is truly dead with `try_wait`, so we
//! never race the old worker's still-draining RocksDB LOCK.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{Mutex, Notify, RwLock};

/// A live, ready worker: the port it accepted on + the owned child process
/// handle (for reuse-safe liveness checks and lifetime ownership).
pub struct ReadyWorker {
    pub port: u16,
    pub pid: u32,
    /// Owned child handle. `try_wait` on this is the reuse-safe liveness check.
    /// Behind a `Mutex` because `try_wait`/`kill` take `&mut Child` but the
    /// registry entry is shared.
    pub child: Arc<Mutex<std::process::Child>>,
}

impl ReadyWorker {
    /// Kill this worker and block until it is reaped, releasing its RocksDB
    /// exclusive LOCK (the OS frees the file handle on process exit). Used when a
    /// freshly-spawned worker turns out to be ORPHANED — i.e. `publish_ready`
    /// reports it was superseded (the registry entry this driver owned was
    /// removed by the watchdog and possibly re-elected to a new spawn). A
    /// superseded worker is routed to by nobody yet still holds the LOCK, so it
    /// MUST be killed or it would deadlock the re-elected spawn's `open_db`.
    /// Mirrors `Registry::kill`'s child teardown.
    pub async fn kill_and_wait(&self) {
        let mut child = self.child.lock().await;
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// Per-repo worker state.
pub enum WorkerState {
    /// A spawn is in flight; awaiters block on `ready` until it flips to Ready
    /// (or the entry is removed on failure).
    ///
    /// `ready` doubles as the per-spawn IDENTITY token: it is a fresh `Arc` each
    /// time `acquire` elects a spawner, so `Arc::ptr_eq` tells whether the
    /// `Spawning` currently in the map is still the one a given driver owns. This
    /// is what makes `publish_ready` / `abandon_spawn` ABA-safe — see those.
    ///
    /// `since` stamps when this spawn was elected, so the orphan watchdog
    /// (`reap`) can drop a `Spawning` whose driver vanished WITHOUT killing a
    /// legitimately-slow one: `spawn_worker` self-times-out at
    /// `SPAWN_READY_TIMEOUT`, so any valid driver resolves its entry (publish or
    /// abandon) well before the much larger `SPAWN_ORPHAN_AFTER` cutoff.
    Spawning { ready: Arc<Notify>, since: Instant },
    /// Worker is accepting on `port`.
    Ready(Arc<ReadyWorker>),
}

/// The router's live worker table, keyed by NORMALIZED repo path.
#[derive(Clone, Default)]
pub struct Registry {
    inner: Arc<RwLock<HashMap<String, WorkerState>>>,
}

/// Outcome of [`Registry::acquire`]: either a ready worker to proxy to, or this
/// caller has been elected to perform the spawn (and must call
/// [`Registry::publish_ready`] / [`Registry::abandon_spawn`] when done).
pub enum Acquire {
    /// A live worker already exists (or another caller's spawn completed) —
    /// proxy here.
    Ready(Arc<ReadyWorker>),
    /// This caller won the single-flight election and must spawn the worker.
    /// `ready` is the notify to signal awaiters once published.
    SpawnElected { ready: Arc<Notify> },
    /// A spawn is already in flight by another caller; await `ready` then
    /// re-acquire.
    AwaitSpawn { ready: Arc<Notify> },
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Single-flight acquire. Returns:
    /// - `Ready` if a live worker exists,
    /// - `SpawnElected` if this caller should spawn (state set to `Spawning`),
    /// - `AwaitSpawn` if another caller is mid-spawn.
    ///
    /// Liveness: a `Ready` entry whose child has exited is treated as absent
    /// (the caller is elected to respawn). The dead-child check uses `try_wait`
    /// on the owned handle (reuse-safe).
    pub async fn acquire(&self, repo: &str) -> Acquire {
        // Fast path: read lock, return Ready if present + alive.
        {
            let map = self.inner.read().await;
            match map.get(repo) {
                Some(WorkerState::Ready(w)) => {
                    if is_child_alive(w).await {
                        return Acquire::Ready(w.clone());
                    }
                    // dead — fall through to write path to respawn
                }
                Some(WorkerState::Spawning { ready, .. }) => {
                    return Acquire::AwaitSpawn {
                        ready: ready.clone(),
                    };
                }
                None => {}
            }
        }

        // Slow path: write lock, re-check (another task may have changed it),
        // then either elect-to-spawn or await.
        let mut map = self.inner.write().await;
        match map.get(repo) {
            Some(WorkerState::Ready(w)) => {
                if is_child_alive(w).await {
                    return Acquire::Ready(w.clone());
                }
                // Confirmed dead under the write lock → reap + elect to respawn.
                // (try_wait already reaped the zombie; just replace the entry.)
            }
            Some(WorkerState::Spawning { ready, .. }) => {
                return Acquire::AwaitSpawn {
                    ready: ready.clone(),
                };
            }
            None => {}
        }
        let ready = Arc::new(Notify::new());
        map.insert(
            repo.to_string(),
            WorkerState::Spawning {
                ready: ready.clone(),
                since: Instant::now(),
            },
        );
        Acquire::SpawnElected { ready }
    }

    /// NON-MUTATING liveness check for read-only (spawn-blocked) routes:
    /// status/graph/files/index-events etc. Returns the live worker if one is
    /// resident and its child is still running, else `None`.
    ///
    /// Why this exists separately from `acquire`: `acquire` is the SPAWN path —
    /// on a dead `Ready` entry it transitions the entry to `Spawning` and returns
    /// `SpawnElected`, EXPECTING the caller to actually spawn. A read-only route
    /// that called `acquire` and then discarded a `SpawnElected` (because it must
    /// never spawn) would leave the entry stuck in `Spawning` forever — an orphan
    /// with no driver, which `get_index_status` then reports as
    /// `indexing/starting` indefinitely (the "stuck Indexing… after idle" bug:
    /// the worker idle-exited, its stale `Ready` entry was probed by a status
    /// poll, and `acquire` orphaned it into `Spawning`).
    ///
    /// `peek_ready` instead REAPS a dead `Ready` entry to ABSENT (so the next
    /// status poll falls through to the durable sidecar, the truth) and NEVER
    /// transitions to `Spawning`. A `Spawning`/absent entry is left untouched and
    /// reported as `None` (the caller serves cold without waking a worker).
    pub async fn peek_ready(&self, repo: &str) -> Option<Arc<ReadyWorker>> {
        // Fast path: read lock. Alive Ready → hit. Spawning/absent → miss. A dead
        // Ready falls through to the write path to be reaped.
        {
            let map = self.inner.read().await;
            match map.get(repo) {
                Some(WorkerState::Ready(w)) => {
                    if is_child_alive(w).await {
                        return Some(w.clone());
                    }
                    // dead — fall through to reap under the write lock
                }
                Some(WorkerState::Spawning { .. }) | None => return None,
            }
        }

        // Slow path: write lock, re-check. Only reap if it is STILL a dead Ready
        // (another task may have replaced it with a Spawning/Ready in the gap —
        // leave that alone). Reap removes the entry entirely (→ absent); it does
        // NOT elect a spawn.
        let mut map = self.inner.write().await;
        match map.get(repo) {
            Some(WorkerState::Ready(w)) => {
                if is_child_alive(w).await {
                    return Some(w.clone());
                }
                // Confirmed dead under the write lock → reap to absent.
                map.remove(repo);
                None
            }
            // Re-elected to Spawning, or already removed → don't touch.
            Some(WorkerState::Spawning { .. }) | None => None,
        }
    }

    /// Background self-heal sweep: drop registry entries that no live driver
    /// backs, so the registry converges to the truth without depending on a
    /// future request to probe each entry.
    ///
    /// Two orphan sources are reaped:
    /// - A `Ready` whose child has EXITED (the worker idle-exited / crashed but
    ///   nothing has probed its entry since). `peek_ready` reaps these on access;
    ///   this catches any that no route happens to touch.
    /// - A `Spawning` older than `spawn_orphan_after` whose driver vanished
    ///   without publishing or abandoning (e.g. the spawning request was dropped
    ///   mid-flight). Removing it + waking its awaiters lets a waiter re-elect a
    ///   fresh spawn instead of blocking on a `Notify` that will never fire.
    ///
    /// FALSE-POSITIVE SAFETY for the `Spawning` cutoff: a *legitimate* driver
    /// resolves its entry (publish_ready/abandon_spawn) before
    /// `spawn::SPAWN_READY_TIMEOUT` because `spawn_worker` self-times-out there
    /// and the caller immediately abandons. With `spawn_orphan_after` set to a
    /// multiple of that timeout (`SPAWN_ORPHAN_AFTER` = 3×), no in-flight valid
    /// spawn is ever old enough to be reaped — only a truly abandoned one is.
    ///
    /// Lock hygiene: the dead-`Ready` check uses `try_lock` + `try_wait`, both
    /// non-blocking and SYNCHRONOUS, so the closure never `.await`s while the
    /// registry write lock is held. A child whose lock is momentarily contended
    /// (`try_lock` fails — e.g. another task is `try_wait`-ing it) is KEPT and
    /// re-checked next tick rather than blocking the sweep.
    pub async fn reap(&self, spawn_orphan_after: Duration) {
        // Collect the identity tokens of removed Spawning entries so we can wake
        // their awaiters AFTER dropping the registry lock.
        let mut woke: Vec<Arc<Notify>> = Vec::new();
        {
            let mut map = self.inner.write().await;
            map.retain(|_repo, state| match state {
                WorkerState::Ready(w) => {
                    // Non-blocking liveness: if we can't grab the child lock right
                    // now, keep it (some task holds it — it's in use); next tick
                    // re-checks. If we can, try_wait tells us if it exited.
                    match w.child.try_lock() {
                        Ok(mut child) => match child.try_wait() {
                            Ok(Some(_status)) => false, // exited → drop (reap)
                            Ok(None) => true,           // still running → keep
                            Err(_) => false,            // undeterminable → drop
                        },
                        Err(_) => true, // busy → keep, re-check next tick
                    }
                }
                WorkerState::Spawning { ready, since } => {
                    if since.elapsed() >= spawn_orphan_after {
                        woke.push(ready.clone());
                        false // orphaned spawn → drop
                    } else {
                        true // still within the spawn budget → keep
                    }
                }
            });
        }
        // Wake awaiters of the dropped Spawning entries (lock released) so they
        // re-acquire and one re-elects a fresh spawn.
        for ready in woke {
            ready.notify_waiters();
        }
    }

    /// Publish a freshly-spawned ready worker and wake all awaiters.
    ///
    /// Returns `true` if the worker was installed, `false` if this spawn was
    /// SUPERSEDED — i.e. the `Spawning` entry this driver owned is no longer in
    /// the map (or has been replaced by a *different* spawn's `Spawning`). That
    /// happens when the orphan watchdog (`reap`) removed a slow-to-publish entry
    /// and a waiter re-elected a fresh spawn. We must NOT blindly `insert` Ready
    /// in that case: it would clobber the re-elected `Spawning` (a second live
    /// worker is mid-spawn for the same repo) and leave THIS worker reachable
    /// while the other keeps booting → two workers racing one RocksDB LOCK. So we
    /// install ONLY when the current entry is still our own `Spawning`
    /// (`Arc::ptr_eq` on the identity token); otherwise we report `false` so the
    /// caller kills this now-orphaned worker. Awaiters are woken either way so a
    /// genuine publish wakes them and a superseded one lets them re-acquire the
    /// new state.
    pub async fn publish_ready(
        &self,
        repo: &str,
        worker: Arc<ReadyWorker>,
        ready: &Arc<Notify>,
    ) -> bool {
        let installed = {
            let mut map = self.inner.write().await;
            match map.get(repo) {
                // Still our spawn → install Ready.
                Some(WorkerState::Spawning { ready: cur, .. }) if Arc::ptr_eq(cur, ready) => {
                    map.insert(repo.to_string(), WorkerState::Ready(worker));
                    true
                }
                // Absent / a different spawn's Spawning / already Ready → our
                // spawn was superseded; do NOT clobber. Caller kills the worker.
                _ => false,
            }
        };
        ready.notify_waiters();
        installed
    }

    /// Abandon a failed spawn: remove the `Spawning` entry and wake awaiters so
    /// they re-acquire (and one of them re-elects to spawn).
    ///
    /// ABA-safe: removes ONLY if the current `Spawning` is still the one this
    /// driver created (`Arc::ptr_eq` on the identity token). If the watchdog
    /// already removed our entry and a waiter re-elected a NEW spawn, the map now
    /// holds a different `Spawning` — we must not remove that (it belongs to
    /// another driver) or a Ready another path installed.
    pub async fn abandon_spawn(&self, repo: &str, ready: &Arc<Notify>) {
        {
            let mut map = self.inner.write().await;
            if matches!(map.get(repo), Some(WorkerState::Spawning { ready: cur, .. }) if Arc::ptr_eq(cur, ready))
            {
                map.remove(repo);
            }
        }
        ready.notify_waiters();
    }

    /// Kill + drop a repo's worker (used on explicit teardown, e.g. repo
    /// removal). Idempotent.
    pub async fn kill(&self, repo: &str) {
        let entry = {
            let mut map = self.inner.write().await;
            map.remove(repo)
        };
        if let Some(WorkerState::Ready(w)) = entry {
            let mut child = w.child.lock().await;
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    /// Snapshot of repos that currently have a Ready worker (for diagnostics /
    /// the router's "which repos are warm" view).
    pub async fn ready_repos(&self) -> Vec<String> {
        self.inner
            .read()
            .await
            .iter()
            .filter(|(_, v)| matches!(v, WorkerState::Ready(_)))
            .map(|(k, _)| k.clone())
            .collect()
    }

    /// Snapshot of repos whose worker is mid-SPAWN (an action triggered a worker
    /// that hasn't finished binding yet). `ready_repos` excludes these — but the
    /// status view needs them so the badge shows "indexing/starting" during the
    /// ~1-2s spawn window instead of falling through to the cold sidecar and
    /// flashing "not indexed" right after the user triggered an action.
    pub async fn spawning_repos(&self) -> Vec<String> {
        self.inner
            .read()
            .await
            .iter()
            .filter(|(_, v)| matches!(v, WorkerState::Spawning { .. }))
            .map(|(k, _)| k.clone())
            .collect()
    }
}

/// Reuse-safe liveness check: `try_wait` on the OWNED child handle. Returns true
/// if the child is still running. A child that has exited is reaped here (so it
/// does not linger as a zombie on Unix) and reported dead.
async fn is_child_alive(w: &ReadyWorker) -> bool {
    let mut child = w.child.lock().await;
    match child.try_wait() {
        Ok(Some(_status)) => false, // exited — reaped
        Ok(None) => true,           // still running
        Err(_) => false,            // can't determine → treat as dead, respawn
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SINGLE-FLIGHT: the first acquire on an absent repo is elected to spawn;
    /// a concurrent acquire for the SAME repo must NOT also be elected (that
    /// would spawn a second worker racing the RocksDB LOCK). It awaits instead.
    #[tokio::test]
    async fn concurrent_acquire_elects_exactly_one_spawner() {
        let reg = Registry::new();
        let a = reg.acquire("repoX").await;
        let b = reg.acquire("repoX").await;

        let a_elected = matches!(a, Acquire::SpawnElected { .. });
        let b_awaits = matches!(b, Acquire::AwaitSpawn { .. });
        assert!(a_elected, "first caller must be elected to spawn");
        assert!(
            b_awaits,
            "second concurrent caller must AWAIT, never spawn a second worker (LOCK race)"
        );
    }

    /// A repo mid-SPAWN (acquire elected a spawner but it hasn't published Ready)
    /// must appear in `spawning_repos()` and NOT in `ready_repos()`. This is what
    /// lets `get_index_status` report "indexing/starting" during the ~1-2s spawn
    /// window instead of falling through to the cold sidecar and flashing
    /// "not_indexed" right after the user triggered an action (the reported bug).
    #[tokio::test]
    async fn spawning_repo_is_reported_spawning_not_ready() {
        let reg = Registry::new();
        // Elect a spawner → the repo's state is now `Spawning`.
        assert!(matches!(
            reg.acquire("repoS").await,
            Acquire::SpawnElected { .. }
        ));
        assert!(
            reg.spawning_repos().await.contains(&"repoS".to_string()),
            "a mid-spawn repo must be reported by spawning_repos()"
        );
        assert!(
            !reg.ready_repos().await.contains(&"repoS".to_string()),
            "a mid-spawn repo must NOT be in ready_repos() (it isn't accepting yet)"
        );
    }

    /// A different repo acquired concurrently IS independently elected (distinct
    /// repos open distinct DBs — no shared LOCK, so they spawn in parallel).
    #[tokio::test]
    async fn distinct_repos_each_elect() {
        let reg = Registry::new();
        let a = reg.acquire("repoA").await;
        let b = reg.acquire("repoB").await;
        assert!(matches!(a, Acquire::SpawnElected { .. }));
        assert!(matches!(b, Acquire::SpawnElected { .. }));
    }

    /// After a failed spawn is abandoned, the entry is cleared so the next
    /// acquire re-elects (retry), rather than getting stuck in Spawning forever.
    #[tokio::test]
    async fn abandon_allows_reelection() {
        let reg = Registry::new();
        let ready = match reg.acquire("repoY").await {
            Acquire::SpawnElected { ready } => ready,
            _ => panic!("first must be elected"),
        };
        reg.abandon_spawn("repoY", &ready).await;
        // Next acquire should be elected again (not stuck awaiting a dead spawn).
        assert!(
            matches!(reg.acquire("repoY").await, Acquire::SpawnElected { .. }),
            "after abandon, the repo must be re-electable for a fresh spawn"
        );
    }

    // ── Self-heal helpers + tests ────────────────────────────────────────────

    /// Build a `ReadyWorker` wrapping a child process that has ALREADY EXITED, so
    /// `try_wait` reports it dead. Cross-platform: an instant-exit command. We
    /// `wait()` first to guarantee termination + cache the status, so the later
    /// `try_wait` in `is_child_alive`/`reap` deterministically returns
    /// `Ok(Some(_))` (dead).
    fn dead_ready_worker() -> Arc<ReadyWorker> {
        #[cfg(windows)]
        let mut child = std::process::Command::new("cmd")
            .args(["/C", "exit", "0"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn instant-exit process");
        #[cfg(not(windows))]
        let mut child = std::process::Command::new("true")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn instant-exit process");
        // Block until it's truly dead so liveness checks are deterministic.
        let _ = child.wait();
        Arc::new(ReadyWorker {
            port: 0,
            pid: child.id(),
            child: Arc::new(Mutex::new(child)),
        })
    }

    /// Build a `ReadyWorker` wrapping a LONG-LIVED child so `try_wait` reports it
    /// alive. The caller is responsible for not leaking it (tests drop the
    /// registry, and the child is killed on drop in practice via the OS once the
    /// handle is gone; we also explicitly kill where it matters).
    fn live_ready_worker() -> Arc<ReadyWorker> {
        #[cfg(windows)]
        // `ping -n 60 -t 127.0.0.1` would also work; `timeout` needs a TTY, so
        // use a long ping which exits on its own and needs no console.
        let child = std::process::Command::new("ping")
            .args(["-n", "60", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn long-lived process");
        #[cfg(not(windows))]
        let child = std::process::Command::new("sleep")
            .arg("60")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn long-lived process");
        Arc::new(ReadyWorker {
            port: 0,
            pid: child.id(),
            child: Arc::new(Mutex::new(child)),
        })
    }

    /// Directly install a `Ready` entry (bypassing the spawn handshake) for
    /// liveness/reap tests.
    async fn install_ready(reg: &Registry, repo: &str, w: Arc<ReadyWorker>) {
        reg.inner
            .write()
            .await
            .insert(repo.to_string(), WorkerState::Ready(w));
    }

    /// (a) REGRESSION for the "stuck Indexing… after idle" bug: `peek_ready` on a
    /// DEAD `Ready` entry must return `None`, REMOVE the entry entirely (reap to
    /// absent — not transition to `Spawning`), and leave `spawning_repos()` empty.
    /// The old code path (`ready_repos()` + `acquire`) orphaned it into `Spawning`,
    /// which `get_index_status` then reported as `indexing/starting` forever.
    #[tokio::test]
    async fn peek_ready_reaps_dead_ready_to_absent() {
        let reg = Registry::new();
        install_ready(&reg, "repoD", dead_ready_worker()).await;

        let got = reg.peek_ready("repoD").await;
        assert!(got.is_none(), "dead Ready must peek as None");
        assert!(
            !reg.ready_repos().await.contains(&"repoD".to_string()),
            "dead Ready must be reaped out of ready_repos()"
        );
        assert!(
            reg.spawning_repos().await.is_empty(),
            "peek_ready must NEVER transition a dead Ready into Spawning (the orphan bug)"
        );
        // Entry is fully absent → a fresh acquire is elected to spawn.
        assert!(
            matches!(reg.acquire("repoD").await, Acquire::SpawnElected { .. }),
            "after reap the repo is absent → next acquire elects a fresh spawn"
        );
    }

    /// (a') A LIVE `Ready` entry peeks as `Some` and is left intact.
    #[tokio::test]
    async fn peek_ready_returns_live_worker() {
        let reg = Registry::new();
        let w = live_ready_worker();
        install_ready(&reg, "repoL", w.clone()).await;

        let got = reg.peek_ready("repoL").await;
        assert!(got.is_some(), "live Ready must peek as Some");
        assert!(
            reg.ready_repos().await.contains(&"repoL".to_string()),
            "live Ready must remain resident after peek"
        );
        w.kill_and_wait().await; // cleanup
    }

    /// (b) Two concurrent `peek_ready` on the SAME dead repo: both return `None`,
    /// the entry is removed exactly once, and nothing panics (idempotent reap).
    #[tokio::test]
    async fn concurrent_peek_ready_dead_repo_no_panic_single_reap() {
        let reg = Registry::new();
        install_ready(&reg, "repoC", dead_ready_worker()).await;

        let r1 = reg.clone();
        let r2 = reg.clone();
        let (a, b) = tokio::join!(async move { r1.peek_ready("repoC").await }, async move {
            r2.peek_ready("repoC").await
        },);
        assert!(a.is_none() && b.is_none(), "both peeks miss on a dead repo");
        assert!(
            reg.ready_repos().await.is_empty() && reg.spawning_repos().await.is_empty(),
            "entry removed exactly once, no orphan Spawning left behind"
        );
    }

    /// (c) `reap(Duration::ZERO)` drops a `Spawning` entry and WAKES its awaiter.
    #[tokio::test]
    async fn reap_drops_orphan_spawning_and_wakes_awaiter() {
        let reg = Registry::new();
        let ready = match reg.acquire("repoSp").await {
            Acquire::SpawnElected { ready } => ready,
            _ => panic!("first acquire must elect"),
        };
        // Register an awaiter BEFORE reaping so notify_waiters reaches it.
        let notified = ready.clone();
        let waiter = tokio::spawn(async move { notified.notified().await });
        // Give the waiter a moment to park on the Notify.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        reg.reap(std::time::Duration::ZERO).await;

        assert!(
            reg.spawning_repos().await.is_empty(),
            "reap(ZERO) must drop the orphan Spawning entry"
        );
        // The awaiter must have been woken (not hang).
        tokio::time::timeout(std::time::Duration::from_secs(2), waiter)
            .await
            .expect("awaiter must be woken by reap")
            .expect("waiter task joins");
    }

    /// (d) FALSE-POSITIVE GUARD: `reap` with the real `SPAWN_ORPHAN_AFTER` cutoff
    /// must NOT touch a freshly-created `Spawning` (a legitimately in-flight, just
    /// possibly slow, spawn). A live driver always resolves its entry before this
    /// cutoff, so the watchdog must never kill it.
    #[tokio::test]
    async fn reap_keeps_recent_spawning() {
        let reg = Registry::new();
        assert!(matches!(
            reg.acquire("repoFresh").await,
            Acquire::SpawnElected { .. }
        ));
        reg.reap(super::super::spawn::SPAWN_ORPHAN_AFTER).await;
        assert!(
            reg.spawning_repos()
                .await
                .contains(&"repoFresh".to_string()),
            "a fresh Spawning must survive reap at the real cutoff (don't kill a slow-but-live spawn)"
        );
    }

    /// (e) `reap` drops a dead `Ready` even if no request ever probes it (the
    /// backstop for entries `peek_ready` never gets to touch).
    #[tokio::test]
    async fn reap_drops_dead_ready() {
        let reg = Registry::new();
        install_ready(&reg, "repoDR", dead_ready_worker()).await;
        reg.reap(std::time::Duration::ZERO).await;
        assert!(
            reg.ready_repos().await.is_empty(),
            "reap must drop a dead Ready entry"
        );
    }

    /// (e') `reap` KEEPS a live `Ready` entry.
    #[tokio::test]
    async fn reap_keeps_live_ready() {
        let reg = Registry::new();
        let w = live_ready_worker();
        install_ready(&reg, "repoLR", w.clone()).await;
        reg.reap(std::time::Duration::ZERO).await;
        assert!(
            reg.ready_repos().await.contains(&"repoLR".to_string()),
            "reap must keep a live Ready entry"
        );
        w.kill_and_wait().await; // cleanup
    }

    /// (f) ABA SAFETY: the watchdog reaps a slow spawn's `Spawning(ready_A)`, a
    /// waiter re-elects a fresh `Spawning(ready_B)`, then the original slow driver
    /// finally completes. `publish_ready(ready_A)` must report SUPERSEDED (false)
    /// and must NOT clobber `ready_B` into `Ready` (that would leave two workers
    /// racing one LOCK). The re-elected `Spawning(ready_B)` stays intact.
    #[tokio::test]
    async fn aba_publish_ready_does_not_clobber_reelected_spawn() {
        let reg = Registry::new();
        // Driver A elected.
        let ready_a = match reg.acquire("repoABA").await {
            Acquire::SpawnElected { ready } => ready,
            _ => panic!("A must elect"),
        };
        // Watchdog reaps A's slow entry.
        reg.reap(std::time::Duration::ZERO).await;
        assert!(reg.spawning_repos().await.is_empty());
        // A waiter re-acquires → fresh election (driver B).
        let _ready_b = match reg.acquire("repoABA").await {
            Acquire::SpawnElected { ready } => ready,
            _ => panic!("re-acquire after reap must elect a fresh spawn"),
        };
        // Driver A finally finishes and tries to publish onto the STALE token.
        let installed = reg
            .publish_ready("repoABA", dead_ready_worker(), &ready_a)
            .await;
        assert!(
            !installed,
            "superseded publish_ready(ready_A) must report false, not install"
        );
        assert!(
            reg.ready_repos().await.is_empty(),
            "ready_A's worker must NOT be installed (no two-worker LOCK race)"
        );
        assert!(
            reg.spawning_repos().await.contains(&"repoABA".to_string()),
            "the re-elected Spawning(ready_B) must remain intact"
        );
    }

    /// (f') ABA SAFETY for `abandon_spawn`: a stale driver A abandoning on
    /// `ready_A` after reap + re-election must be a NO-OP — it must NOT remove the
    /// re-elected `Spawning(ready_B)`.
    #[tokio::test]
    async fn aba_abandon_spawn_does_not_remove_reelected_spawn() {
        let reg = Registry::new();
        let ready_a = match reg.acquire("repoABA2").await {
            Acquire::SpawnElected { ready } => ready,
            _ => panic!("A must elect"),
        };
        reg.reap(std::time::Duration::ZERO).await;
        let _ready_b = match reg.acquire("repoABA2").await {
            Acquire::SpawnElected { ready } => ready,
            _ => panic!("re-acquire must elect"),
        };
        // Stale abandon on A's token.
        reg.abandon_spawn("repoABA2", &ready_a).await;
        assert!(
            reg.spawning_repos().await.contains(&"repoABA2".to_string()),
            "stale abandon_spawn(ready_A) must NOT remove the re-elected Spawning(ready_B)"
        );
    }
}
