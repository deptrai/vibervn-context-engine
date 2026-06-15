//! Bounded in-memory session store for the MCP streamable-HTTP transport.
//!
//! ## Why this exists (the bug it fixes)
//!
//! rmcp's `LocalSessionManager` keeps live sessions in a `HashMap` with a
//! `keep_alive` idle timeout (default 5 min). When a client goes quiet for
//! longer than that, the session worker exits with `IdleTimeout` and
//! `close_session` removes the entry from that map. With a *store-less*
//! manager, the very next request carrying the now-stale `mcp-session-id`
//! finds `has_session == false`, there is nothing to restore from, and the
//! client receives `404 "Session not found"`. Clients (e.g. Claude Code) hold
//! a session id across long idle gaps, so the failure is intermittent — it
//! only fires when the gap exceeds the idle window. For a server that runs for
//! weeks as an always-on service, that is not acceptable: MCP must not break.
//!
//! ## How a store fixes it (root cause, not a longer timeout)
//!
//! rmcp's transport supports a pluggable [`SessionStore`]. When one is
//! configured, the lifecycle becomes self-healing:
//!   - on `initialize`, the transport persists the client's `initialize_params`
//!     here (keyed by the unique session id);
//!   - an idle timeout still drops the *live worker* (cheap — bounds the number
//!     of resident channels/tasks), but it does **not** delete the store entry
//!     (only an explicit client `DELETE` does);
//!   - the next request with the stale id misses the live map, then
//!     `try_restore_from_store` loads the params from here and transparently
//!     re-creates the worker and replays the handshake. The client never sees
//!     an error.
//!
//! This lets us keep rmcp's short default `keep_alive` (so the count of *live*
//! workers stays bounded) while still never returning "Session not found".
//!
//! ## Bounded memory (project invariant)
//!
//! This server indexes Linux-kernel / Chromium-scale repos and must keep memory
//! bounded regardless of load. Each entry is tiny (just `initialize_params`),
//! but an always-on server serving many short-lived clients over weeks would
//! grow this map without limit. So it is an LRU bounded at [`MAX_SESSIONS`],
//! mirroring the per-repo vector-shard LRU: on insert past the cap we evict the
//! least-recently-used entry. An evicted client simply re-initializes on its
//! next request (a fresh session id), which is correct, not an error.
//!
//! ## Multi-client safety
//!
//! Session ids are globally unique (rmcp's `session_id()`), so every client —
//! and every concurrent connection — has an independent entry. A single shared
//! store instance backs both the global `/mcp` endpoint and the per-repo
//! services; there is no cross-client interference. All state lives behind a
//! single `RwLock`, so concurrent `load`/`store`/`delete` are serialized.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::RwLock;

use rmcp::transport::streamable_http_server::session::store::{
    SessionState, SessionStore, SessionStoreError,
};

/// Upper bound on retained sessions. Each entry is only the client's
/// `initialize_params` (a few hundred bytes), so 8192 is generous yet keeps
/// worst-case memory in the low single-digit megabytes. Far more than the
/// number of *live* workers a single-user (or small-team) install will ever
/// hold concurrently; the cap exists purely to bound an always-on server.
pub const MAX_SESSIONS: usize = 8192;

/// A single stored session plus its recency stamp (for LRU eviction).
#[derive(Debug)]
struct Entry {
    state: SessionState,
    /// Monotonic stamp bumped on insert/access. Lowest = least recently used.
    last_used: u64,
}

/// Bounded, in-memory, LRU [`SessionStore`].
#[derive(Debug, Default)]
pub struct BoundedSessionStore {
    inner: RwLock<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    sessions: HashMap<String, Entry>,
    /// Monotonic recency counter. Never reset; u64 won't wrap in any realistic
    /// server lifetime (would need ~10^19 store ops).
    clock: u64,
}

impl Inner {
    fn tick(&mut self) -> u64 {
        self.clock = self.clock.wrapping_add(1);
        self.clock
    }

    /// Evict least-recently-used entries until strictly below `MAX_SESSIONS`,
    /// leaving room for one new insert. O(n) per eviction, but evictions only
    /// happen on `store` (session creation), which is rare relative to
    /// `load` (every restore) — never on the hot query path.
    fn evict_to_cap(&mut self) {
        while self.sessions.len() >= MAX_SESSIONS {
            let Some(victim) = self
                .sessions
                .iter()
                .min_by_key(|(_, e)| e.last_used)
                .map(|(id, _)| id.clone())
            else {
                break;
            };
            self.sessions.remove(&victim);
        }
    }
}

impl BoundedSessionStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Test/diagnostic helper: number of currently retained sessions.
    #[cfg(test)]
    pub async fn session_count(&self) -> usize {
        self.inner.read().await.sessions.len()
    }
}

#[async_trait::async_trait]
impl SessionStore for BoundedSessionStore {
    async fn load(&self, session_id: &str) -> Result<Option<SessionState>, SessionStoreError> {
        let mut inner = self.inner.write().await;
        let stamp = inner.tick();
        if let Some(entry) = inner.sessions.get_mut(session_id) {
            entry.last_used = stamp;
            Ok(Some(entry.state.clone()))
        } else {
            Ok(None)
        }
    }

    async fn store(&self, session_id: &str, state: &SessionState) -> Result<(), SessionStoreError> {
        let mut inner = self.inner.write().await;
        // Only evict when inserting a genuinely new id; re-storing an existing
        // session must not evict a peer.
        if !inner.sessions.contains_key(session_id) {
            inner.evict_to_cap();
        }
        let stamp = inner.tick();
        inner.sessions.insert(
            session_id.to_owned(),
            Entry {
                state: state.clone(),
                last_used: stamp,
            },
        );
        Ok(())
    }

    async fn delete(&self, session_id: &str) -> Result<(), SessionStoreError> {
        self.inner.write().await.sessions.remove(session_id);
        Ok(())
    }
}

/// Shared handle type used by the server.
pub type SharedSessionStore = Arc<BoundedSessionStore>;

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::InitializeRequestParams;

    fn state() -> SessionState {
        SessionState::new(InitializeRequestParams::default())
    }

    #[tokio::test]
    async fn store_then_load_roundtrips() {
        let s = BoundedSessionStore::new();
        assert!(s.load("missing").await.unwrap().is_none());
        s.store("a", &state()).await.unwrap();
        assert!(s.load("a").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn delete_removes_entry() {
        let s = BoundedSessionStore::new();
        s.store("a", &state()).await.unwrap();
        s.delete("a").await.unwrap();
        assert!(s.load("a").await.unwrap().is_none());
        // Deleting a missing id is a no-op, not an error.
        s.delete("a").await.unwrap();
    }

    #[tokio::test]
    async fn restore_returns_none_after_idle_close_does_not_delete() {
        // Models the real flow: idle timeout calls close_session (in-memory map
        // only) but never touches the store, so the entry survives for restore.
        let s = BoundedSessionStore::new();
        s.store("sess", &state()).await.unwrap();
        // No delete happened (idle close path) -> still restorable.
        assert!(s.load("sess").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn evicts_lru_when_over_cap_and_keeps_bounded() {
        let s = BoundedSessionStore::new();
        // Fill exactly to cap.
        for i in 0..MAX_SESSIONS {
            s.store(&format!("k{i}"), &state()).await.unwrap();
        }
        assert_eq!(s.session_count().await, MAX_SESSIONS);

        // Touch "k0" so it is most-recently-used; "k1" is now the LRU victim.
        assert!(s.load("k0").await.unwrap().is_some());

        // One more insert must evict the LRU (k1), not k0, and stay bounded.
        s.store("overflow", &state()).await.unwrap();
        assert_eq!(s.session_count().await, MAX_SESSIONS);
        assert!(s.load("k0").await.unwrap().is_some(), "recently-used kept");
        assert!(s.load("overflow").await.unwrap().is_some(), "new kept");
        assert!(s.load("k1").await.unwrap().is_none(), "LRU evicted");
    }

    #[tokio::test]
    async fn re_store_existing_does_not_evict_peer() {
        let s = BoundedSessionStore::new();
        for i in 0..MAX_SESSIONS {
            s.store(&format!("k{i}"), &state()).await.unwrap();
        }
        // Re-storing an existing id must not push us over cap / evict anyone.
        s.store("k0", &state()).await.unwrap();
        assert_eq!(s.session_count().await, MAX_SESSIONS);
        assert!(s.load("k1").await.unwrap().is_some());
    }
}

