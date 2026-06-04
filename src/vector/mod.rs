use anyhow::{Context, Result};
use surrealdb::Surreal;
use surrealdb::engine::local::Db;
use tracing::info;

// ─── Public types ─────────────────────────────────────────────────────────

/// Identifies a chunk by its location in the source tree.
/// Used to map VectorIndex results back to SurrealDB rows.
#[derive(Debug, Clone)]
pub struct ChunkId {
    pub file: String,
    pub line_start: u32,
    pub line_end: u32,
}

/// A single result returned by [`VectorIndex::search`].
#[derive(Debug, Clone)]
pub struct SearchResult {
    pub chunk_id: ChunkId,
    /// Cosine similarity in [0, 1] (vectors are pre-normalized).
    pub score: f32,
}

// ─── VectorIndex ─────────────────────────────────────────────────────────

/// In-memory flat cosine-similarity index.
///
/// All vectors are L2-normalized at insert time, so cosine similarity reduces
/// to a plain dot product at query time (no division per candidate).
///
/// At 500 K chunks × 1024 dims, LLVM auto-vectorizes the inner loop to SIMD,
/// giving ~50-100 ms per query on a modern CPU — acceptable until an HNSW
/// implementation is available.
pub struct VectorIndex {
    /// Row-major storage: entry i holds the normalized embedding for chunk i.
    embeddings: Vec<Vec<f32>>,
    /// Parallel array: chunk_ids[i] corresponds to embeddings[i].
    chunk_ids: Vec<ChunkId>,
    /// Dimensionality of the first inserted vector; all subsequent inserts
    /// must match. `None` until the first insert.
    dimension: Option<usize>,
}

impl VectorIndex {
    /// Create an empty index.
    pub fn new() -> Self {
        Self {
            embeddings: Vec::new(),
            chunk_ids: Vec::new(),
            dimension: None,
        }
    }

    /// Insert a batch of (ChunkId, embedding) pairs.
    ///
    /// Each embedding is L2-normalized before storage. Zero-length or
    /// zero-magnitude vectors are stored as-is (they will score 0 against
    /// everything, which is correct).
    pub fn insert(&mut self, chunks: &[(ChunkId, Vec<f32>)]) {
        for (id, raw_emb) in chunks {
            if raw_emb.is_empty() {
                // Skip zero-length embeddings — they carry no information.
                continue;
            }
            // Record dimension on first insert; skip mismatches.
            match self.dimension {
                None => self.dimension = Some(raw_emb.len()),
                Some(d) if d != raw_emb.len() => {
                    tracing::warn!(
                        expected = d,
                        got = raw_emb.len(),
                        file = %id.file,
                        "embedding dimension mismatch — skipping chunk"
                    );
                    continue;
                }
                _ => {}
            }

            let normalized = l2_normalize(raw_emb);
            self.embeddings.push(normalized);
            self.chunk_ids.push(id.clone());
        }
    }

    /// Remove all embeddings whose `file` field matches `file`.
    ///
    /// Uses swap-remove to avoid O(n) shifts; rebuilds both parallel arrays.
    pub fn remove_file(&mut self, file: &str) {
        let mut i = 0;
        while i < self.chunk_ids.len() {
            if self.chunk_ids[i].file == file {
                self.chunk_ids.swap_remove(i);
                self.embeddings.swap_remove(i);
                // Don't advance i — the swapped element now lives at i.
            } else {
                i += 1;
            }
        }
    }

    /// Search for the top-k most similar chunks to `query`.
    ///
    /// `query` is normalized internally so the caller need not pre-normalize.
    /// Returns results sorted by descending score, capped at `top_k`.
    pub fn search(&self, query: &[f32], top_k: usize) -> Vec<SearchResult> {
        if self.embeddings.is_empty() || query.is_empty() || top_k == 0 {
            return vec![];
        }

        let q_norm = l2_normalize(query);

        // Score every vector.
        let mut scored: Vec<(usize, f32)> = self
            .embeddings
            .iter()
            .enumerate()
            .map(|(i, emb)| (i, dot_product(&q_norm, emb)))
            .collect();

        // Partial sort: bring the top-k largest scores to the front.
        let k = top_k.min(scored.len());
        scored.select_nth_unstable_by(k - 1, |a, b| {
            b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal)
        });
        scored.truncate(k);
        scored.sort_unstable_by(|a, b| {
            b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal)
        });

        scored
            .into_iter()
            .map(|(i, score)| SearchResult {
                chunk_id: self.chunk_ids[i].clone(),
                score,
            })
            .collect()
    }

    /// Load all embeddings from SurrealDB on startup.
    ///
    /// Only loads rows that have a non-empty embedding vector.
    pub async fn load_from_db(db: &Surreal<Db>) -> Result<Self> {
        use serde::Deserialize;

        #[derive(Deserialize)]
        struct Row {
            file: String,
            line_start: i64,
            line_end: i64,
            embedding: Vec<f32>,
        }

        let rows: Vec<Row> = db
            .query("SELECT file, line_start, line_end, embedding FROM chunk WHERE embedding != []")
            .await
            .context("load embeddings from chunk table")?
            .take(0)?;

        let mut index = VectorIndex::new();
        let pairs: Vec<(ChunkId, Vec<f32>)> = rows
            .into_iter()
            .map(|r| {
                (
                    ChunkId {
                        file: r.file,
                        line_start: r.line_start as u32,
                        line_end: r.line_end as u32,
                    },
                    r.embedding,
                )
            })
            .collect();

        let count = pairs.len();
        index.insert(&pairs);
        info!(count, "loaded embeddings into VectorIndex");

        Ok(index)
    }

    /// Remove all entries from the index.
    pub fn clear(&mut self) {
        self.embeddings.clear();
        self.chunk_ids.clear();
        self.dimension = None;
    }

    /// Number of indexed vectors.
    pub fn len(&self) -> usize {
        self.embeddings.len()
    }

    /// Returns `true` if the index contains no vectors.
    pub fn is_empty(&self) -> bool {
        self.embeddings.is_empty()
    }
}

impl Default for VectorIndex {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Math helpers ─────────────────────────────────────────────────────────

/// Compute the dot product of two equal-length slices.
#[inline]
fn dot_product(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Return a copy of `v` normalized to unit L2 length.
/// Returns `v` unchanged if its magnitude is zero (avoids NaN).
fn l2_normalize(v: &[f32]) -> Vec<f32> {
    let mag: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if mag == 0.0 {
        return v.to_vec();
    }
    v.iter().map(|x| x / mag).collect()
}
