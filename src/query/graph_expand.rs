use std::collections::{HashMap, HashSet};

use anyhow::Result;
use serde::Deserialize;
use surrealdb::Surreal;
use surrealdb::engine::local::Db;
use tracing::warn;

use crate::query::find_db_for_file;
use crate::query::merger::MergeChunk;

/// An expanded chunk produced by BFS graph traversal.
pub struct ExpandedChunk {
    pub file: String,
    pub line_start: u32,
    pub line_end: u32,
    pub score: f32,
    pub content: String,
    pub symbol: Option<String>,
}

// ─── DB row types ──────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct SymbolRow {
    file: String,
    name: String,
    line_start: i64,
    line_end: i64,
}

#[derive(Deserialize)]
struct ChunkRow {
    file: String,
    line_start: i64,
    line_end: i64,
    content: String,
}

// ─── Graph expansion ──────────────────────────────────────────────────────

const CALLER_SCORE_FACTOR: f32 = 0.6;
const CALLEE_SCORE_FACTOR: f32 = 0.5;
const SCORE_FLOOR: f32 = 0.15;
const MAX_DEPTH: usize = 2;
const MAX_BONUS_CHUNKS: usize = 30;

/// Expand base search results via BFS over the call graph.
///
/// For each chunk in `base_chunks`, finds overlapping symbols, then BFS-expands
/// callers (score × 0.6) and callees (score × 0.5) up to 2 levels deep.
/// Returns up to MAX_BONUS_CHUNKS additional chunks.
///
/// `db_for_file` maps repo path → DB handle so we can look up chunks by file.
pub async fn graph_expand(
    base_chunks: &[MergeChunk],
    db_map: &HashMap<String, Surreal<Db>>,
) -> Vec<ExpandedChunk> {
    // We need any db — pick the first available. For multi-repo, we check all.
    if db_map.is_empty() {
        return vec![];
    }

    let mut all_expanded: Vec<ExpandedChunk> = Vec::new();
    let mut global_seen: HashSet<String> = HashSet::new(); // FQN strings already visited

    // Track files already present in base_chunks so we don't re-fetch identical ranges.
    let base_keys: HashSet<(String, u32, u32)> = base_chunks
        .iter()
        .map(|c| (c.file.clone(), c.line_start, c.line_end))
        .collect();

    'outer: for base_chunk in base_chunks {
        // Find overlapping symbols in the DB that owns this file.
        let db = match find_db_for_file(db_map, &base_chunk.file) {
            Some(db) => db,
            None => continue,
        };

        let overlapping = match query_overlapping_symbols(
            &db,
            &base_chunk.file,
            base_chunk.line_start,
            base_chunk.line_end,
        )
        .await
        {
            Ok(syms) => syms,
            Err(e) => {
                warn!(error = %e, file = %base_chunk.file, "failed to query overlapping symbols");
                continue;
            }
        };

        if overlapping.is_empty() {
            continue;
        }

        // BFS queue: (fqn_string, score, depth)
        let mut queue: Vec<(String, f32, usize)> = overlapping
            .iter()
            .map(|s| (build_fqn(&s.file, &s.name), base_chunk.score, 0))
            .collect();

        while let Some((fqn, score, depth)) = queue.pop() {
            if depth >= MAX_DEPTH {
                continue;
            }
            if all_expanded.len() >= MAX_BONUS_CHUNKS {
                break 'outer;
            }

            // Expand callers.
            let caller_score = score * CALLER_SCORE_FACTOR;
            if caller_score >= SCORE_FLOOR {
                let callers = query_callers(&db, &fqn).await.unwrap_or_default();
                for caller_fqn in callers {
                    if global_seen.contains(&caller_fqn) {
                        continue;
                    }
                    global_seen.insert(caller_fqn.clone());
                    if let Some(chunk) =
                        fetch_chunk_for_fqn(&db, &caller_fqn, caller_score, &base_keys).await
                    {
                        if all_expanded.len() < MAX_BONUS_CHUNKS {
                            all_expanded.push(chunk);
                        }
                        queue.push((caller_fqn, caller_score, depth + 1));
                    }
                }
            }

            // Expand callees.
            let callee_score = score * CALLEE_SCORE_FACTOR;
            if callee_score >= SCORE_FLOOR {
                let callees = query_callees(&db, &fqn).await.unwrap_or_default();
                for callee_fqn in callees {
                    if global_seen.contains(&callee_fqn) {
                        continue;
                    }
                    global_seen.insert(callee_fqn.clone());
                    if let Some(chunk) =
                        fetch_chunk_for_fqn(&db, &callee_fqn, callee_score, &base_keys).await
                    {
                        if all_expanded.len() < MAX_BONUS_CHUNKS {
                            all_expanded.push(chunk);
                        }
                        queue.push((callee_fqn, callee_score, depth + 1));
                    }
                }
            }
        }
    }

    all_expanded
}

// ─── Helpers ──────────────────────────────────────────────────────────────

/// Build a FQN string that matches what's stored as record IDs.
/// Format: "{file}::{name}" — used only for deduplication, not as a SurrealDB record ID.
fn build_fqn(file: &str, name: &str) -> String {
    format!("{}::{}", file, name)
}

async fn query_overlapping_symbols(
    db: &Surreal<Db>,
    file: &str,
    chunk_start: u32,
    chunk_end: u32,
) -> Result<Vec<SymbolRow>> {
    let rows: Vec<SymbolRow> = db
        .query(
            "SELECT file, name, line_start, line_end FROM symbol \
             WHERE file = $file AND line_start <= $chunk_end AND line_end >= $chunk_start",
        )
        .bind(("file", file.to_string()))
        .bind(("chunk_end", chunk_end as i64))
        .bind(("chunk_start", chunk_start as i64))
        .await?
        .take(0)?;
    Ok(rows)
}

async fn query_callers(db: &Surreal<Db>, fqn: &str) -> Result<Vec<String>> {
    // Extract name from fqn (format: "file::name").
    let name = fqn.rsplit("::").next().unwrap_or(fqn);

    #[derive(Deserialize)]
    struct Row {
        in_file: String,
        // The caller is the `in` side of the calls relation.
    }

    // We query by looking for calls edges where the out (callee) has the given name.
    // Since record IDs use the FQN, we need to find by symbol name across files.
    let rows: Vec<Row> = db
        .query(
            "SELECT in_file FROM calls WHERE out.name = $name LIMIT 20",
        )
        .bind(("name", name.to_string()))
        .await?
        .take(0)?;

    // Return unique file::name pairs from caller side.
    let callers: Vec<String> = rows
        .into_iter()
        .map(|r| format!("{}::{}", r.in_file, name))
        .collect();
    Ok(callers)
}

async fn query_callees(db: &Surreal<Db>, fqn: &str) -> Result<Vec<String>> {
    let name = fqn.rsplit("::").next().unwrap_or(fqn);

    #[derive(Deserialize)]
    struct Row {
        out_file: String,
    }

    let rows: Vec<Row> = db
        .query(
            "SELECT out_file FROM calls WHERE in.name = $name LIMIT 20",
        )
        .bind(("name", name.to_string()))
        .await?
        .take(0)?;

    let callees: Vec<String> = rows
        .into_iter()
        .map(|r| {
            let callee_name = fqn.rsplit("::").next().unwrap_or(name);
            format!("{}::{}", r.out_file, callee_name)
        })
        .collect();
    Ok(callees)
}

async fn fetch_chunk_for_fqn(
    db: &Surreal<Db>,
    fqn: &str,
    score: f32,
    base_keys: &HashSet<(String, u32, u32)>,
) -> Option<ExpandedChunk> {
    // First find the symbol to get its location.
    let name = fqn.rsplit("::").next().unwrap_or(fqn);
    let file_prefix = &fqn[..fqn.rfind("::").unwrap_or(fqn.len())];

    let sym_rows: Vec<SymbolRow> = db
        .query(
            "SELECT file, name, line_start, line_end FROM symbol \
             WHERE name = $name AND file = $file LIMIT 1",
        )
        .bind(("name", name.to_string()))
        .bind(("file", file_prefix.to_string()))
        .await
        .ok()?
        .take(0)
        .ok()?;

    let sym = sym_rows.into_iter().next()?;

    // Find the overlapping chunk.
    let chunk_rows: Vec<ChunkRow> = db
        .query(
            "SELECT file, line_start, line_end, content FROM chunk \
             WHERE file = $file AND line_start <= $sym_end AND line_end >= $sym_start \
             ORDER BY line_start LIMIT 1",
        )
        .bind(("file", sym.file.clone()))
        .bind(("sym_end", sym.line_end))
        .bind(("sym_start", sym.line_start))
        .await
        .ok()?
        .take(0)
        .ok()?;

    let row = chunk_rows.into_iter().next()?;
    let ls = row.line_start as u32;
    let le = row.line_end as u32;

    // Skip if this range was already in the base results.
    if base_keys.contains(&(row.file.clone(), ls, le)) {
        return None;
    }

    Some(ExpandedChunk {
        file: row.file,
        line_start: ls,
        line_end: le,
        score,
        content: row.content,
        symbol: Some(sym.name),
    })
}
