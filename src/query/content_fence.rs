//! Content fence — the query-side guarantee that a returned result always has
//! real code behind it.
//!
//! ## Why this exists
//! A resident vector shard and the `chunk` rows it points at are two separate
//! stores that are mutated at different instants during an index run:
//!
//! * full rebuild: `delete_all_data` clears `chunk` (store::ops), and the new
//!   shard is only published later, after the whole repo has been re-embedded.
//! * incremental: `delete_files_data_incremental` clears the affected files'
//!   `chunk` rows, and the shard delta is only applied after streaming finishes.
//!
//! In both windows a query can hit a shard whose vectors reference `chunk` rows
//! that no longer exist. `EmbeddingIdentity` does NOT catch this: the identity is
//! unchanged (same model), so the shard validates as a `Hit`. The DB lookup then
//! returns nothing and the pipeline happily emits a result block with an empty
//! body — a confident answer containing no code.
//!
//! This module is the fail-closed backstop: a candidate whose stored content did
//! not resolve is DROPPED, and a query whose candidates were *all* dropped is
//! reported as `warming` (retry) rather than as a genuine "no results". Dropping
//! is preferred over erroring so a partially re-indexed repo still returns the
//! chunks that ARE durable (the chosen "partial results, never empty" semantics).
//!
//! The logic is a pure function over already-fetched chunks so it is unit-tested
//! without a live DB, an embedding client, or a running index.

use crate::query::merger::MergeChunk;

/// Minimum number of non-whitespace characters a chunk's stored content must have
/// to count as resolved. A chunk row that is absent (or present but blank) yields
/// an empty string from `fetch_chunk_content`, which is indistinguishable from —
/// and just as useless as — a missing row, so both are fenced by the same rule.
pub const MIN_RESOLVED_CONTENT_CHARS: usize = 1;

/// Outcome of fencing a candidate set.
#[derive(Debug, Default)]
pub struct ContentFence {
    /// Candidates whose stored content resolved. Original order is preserved.
    pub kept: Vec<MergeChunk>,
    /// How many candidates were dropped because their content did not resolve.
    pub dropped: usize,
}

impl ContentFence {
    /// True when there were candidates but every one of them was dropped.
    ///
    /// This is the signal that the vector shard is out of sync with the `chunk`
    /// table (an index run is mid-flight), NOT that the repo has no match. The
    /// caller must translate it into `warming = true`.
    pub fn stale_shard_detected(&self) -> bool {
        self.kept.is_empty() && self.dropped > 0
    }
}

/// True if `content` failed to resolve to real stored code.
pub fn is_unresolved_content(content: &str) -> bool {
    content.trim().len() < MIN_RESOLVED_CONTENT_CHARS
}

/// Drop every candidate whose stored content did not resolve.
pub fn apply(chunks: Vec<MergeChunk>) -> ContentFence {
    let total = chunks.len();
    let kept: Vec<MergeChunk> = chunks
        .into_iter()
        .filter(|c| !is_unresolved_content(&c.content))
        .collect();
    ContentFence {
        dropped: total - kept.len(),
        kept,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(file: &str, content: &str) -> MergeChunk {
        MergeChunk {
            file: file.to_owned(),
            line_start: 1,
            line_end: 2,
            score: 1.0,
            content: content.to_owned(),
            symbol: None,
            symbol_fqn: None,
            symbol_kind: None,
        }
    }

    #[test]
    fn drops_unresolved_and_keeps_resolved_in_order() {
        let fence = apply(vec![
            chunk("a.rs", "fn a() {}"),
            chunk("gone.rs", ""),
            chunk("b.rs", "fn b() {}"),
        ]);
        assert_eq!(fence.dropped, 1, "the missing chunk row must be dropped");
        assert_eq!(
            fence
                .kept
                .iter()
                .map(|c| c.file.as_str())
                .collect::<Vec<_>>(),
            vec!["a.rs", "b.rs"],
            "surviving candidates keep their original order"
        );
        assert!(
            !fence.stale_shard_detected(),
            "a partial drop is partial results, not a stale shard"
        );
    }

    /// Whitespace-only content is as useless as an absent row — same fence.
    #[test]
    fn whitespace_only_content_is_unresolved() {
        assert!(is_unresolved_content(""));
        assert!(is_unresolved_content("   \n\t "));
        assert!(!is_unresolved_content("x"));
    }

    /// Every candidate dropped => the shard is ahead of the chunk table.
    #[test]
    fn all_dropped_is_reported_as_stale_shard() {
        let fence = apply(vec![chunk("gone1.rs", ""), chunk("gone2.rs", "")]);
        assert_eq!(fence.dropped, 2);
        assert!(fence.kept.is_empty());
        assert!(
            fence.stale_shard_detected(),
            "all-dropped must be a retryable warming signal, never 'no results'"
        );
    }

    /// A genuinely empty candidate set is NOT a stale shard: the search simply
    /// matched nothing, which must keep reporting a real empty.
    #[test]
    fn no_candidates_is_not_stale_shard() {
        let fence = apply(vec![]);
        assert_eq!(fence.dropped, 0);
        assert!(
            !fence.stale_shard_detected(),
            "empty input must stay a genuine empty, not a warming retry"
        );
    }
}
