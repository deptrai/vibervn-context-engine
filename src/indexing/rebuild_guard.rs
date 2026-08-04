//! Single-flight guard for automatic embedding-identity rebuild triggers.
//!
//! A burst of queries can all discover the same stale shard concurrently. This
//! guard ensures at most one `rebuild: true` trigger is queued per repo until the
//! consumer finishes (success or failure). The durable `needs_rebuild` DB marker
//! remains the cross-restart source of truth; this set only coalesces work within
//! one process lifetime.

use std::collections::HashSet;
use std::sync::Mutex;

#[derive(Default)]
pub struct IdentityRebuildGuard {
    pending: Mutex<HashSet<String>>,
}

impl IdentityRebuildGuard {
    /// Reserve `repo` for one automatic rebuild trigger.
    ///
    /// Returns `true` only to the first caller; later callers coalesce until
    /// [`release`](Self::release) is called by the index consumer.
    pub fn reserve(&self, repo: &str) -> bool {
        let mut pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
        pending.insert(repo.to_owned())
    }

    /// Release the single-flight reservation after a run completes/fails, or when
    /// channel send fails during shutdown so a future query/restart can retry.
    pub fn release(&self, repo: &str) {
        let mut pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
        pending.remove(repo);
    }
}
