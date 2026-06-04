use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use surrealdb::Surreal;
use surrealdb::engine::local::Db;

use crate::parsing::symbols::{QualifiedSymbol, Symbol, SymbolKind};
use crate::parsing::relations::EdgeKind;
use crate::parsing::chunker::Chunk;

// ─── FileMeta ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileMeta {
    pub path: String,
    pub mtime: i64,
    pub size: i64,
    pub repo: String,
}

// ─── IndexMeta ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexMeta {
    pub key: String,
    pub value: String,
}

// ─── DB row types for queries ─────────────────────────────────────────────

pub fn kind_to_str(k: &SymbolKind) -> &'static str {
    match k {
        SymbolKind::Function => "function",
        SymbolKind::Method => "method",
        SymbolKind::Struct => "struct",
        SymbolKind::Trait => "trait",
        SymbolKind::Impl => "impl",
        SymbolKind::Class => "class",
        SymbolKind::Module => "module",
        SymbolKind::Interface => "interface",
    }
}

// ─── Delete operations (used in transactions) ────────────────────────────

/// Delete all edges, symbols, chunks, and file_meta for a given file path.
/// Edge deletion happens first (while symbol IDs still exist for traversal).
pub async fn delete_file_data(db: &Surreal<Db>, file_path: &str) -> Result<()> {
    // 1. Delete edges first (all relation tables by in_file or out_file).
    let path = file_path.to_string();

    db.query("DELETE FROM calls WHERE in_file = $path OR out_file = $path")
        .bind(("path", path.clone()))
        .await
        .context("delete calls")?;

    db.query("DELETE FROM uses WHERE in_file = $path OR out_file = $path")
        .bind(("path", path.clone()))
        .await
        .context("delete uses")?;

    db.query("DELETE FROM imports WHERE in_file = $path OR out_file = $path")
        .bind(("path", path.clone()))
        .await
        .context("delete imports")?;

    db.query("DELETE FROM contains WHERE in_file = $path OR out_file = $path")
        .bind(("path", path.clone()))
        .await
        .context("delete contains")?;

    db.query("DELETE FROM implements WHERE in_file = $path OR out_file = $path")
        .bind(("path", path.clone()))
        .await
        .context("delete implements")?;

    // 2. Delete symbols.
    db.query("DELETE FROM symbol WHERE file = $path")
        .bind(("path", path.clone()))
        .await
        .context("delete symbols")?;

    // 3. Delete chunks.
    db.query("DELETE FROM chunk WHERE file = $path")
        .bind(("path", path.clone()))
        .await
        .context("delete chunks")?;

    // 4. Delete file_meta.
    db.query("DELETE FROM file_meta WHERE path = $path")
        .bind(("path", path.clone()))
        .await
        .context("delete file_meta")?;

    Ok(())
}

/// Delete ALL data — used for full rebuild.
pub async fn delete_all_data(db: &Surreal<Db>) -> Result<()> {
    // Edges first.
    db.query("DELETE FROM calls").await.context("delete all calls")?;
    db.query("DELETE FROM uses").await.context("delete all uses")?;
    db.query("DELETE FROM imports").await.context("delete all imports")?;
    db.query("DELETE FROM contains").await.context("delete all contains")?;
    db.query("DELETE FROM implements").await.context("delete all implements")?;
    // Then symbols, chunks, file_meta.
    db.query("DELETE FROM symbol").await.context("delete all symbols")?;
    db.query("DELETE FROM chunk").await.context("delete all chunks")?;
    db.query("DELETE FROM file_meta").await.context("delete all file_meta")?;
    Ok(())
}

// ─── Insert operations ────────────────────────────────────────────────────

/// Upsert a symbol using its deterministic record ID.
pub async fn upsert_symbol(db: &Surreal<Db>, sym: &Symbol) -> Result<()> {
    let record_id = sym.qualified.record_id();
    let kind_str = kind_to_str(&sym.kind);
    let parent_id = sym.parent_fqn.as_ref().map(|fqn| {
        format!("symbol:⟨{}⟩", fqn)
    });

    db.query(
        "UPSERT type::thing($id) SET \
         name = $name, kind = $kind, file = $file, \
         line_start = $line_start, line_end = $line_end, \
         signature = $signature, parent = $parent",
    )
    .bind(("id", record_id))
    .bind(("name", sym.qualified.name.clone()))
    .bind(("kind", kind_str.to_string()))
    .bind(("file", sym.qualified.file.clone()))
    .bind(("line_start", sym.line_start as i64))
    .bind(("line_end", sym.line_end as i64))
    .bind(("signature", sym.signature.clone()))
    .bind(("parent", parent_id))
    .await
    .context("upsert symbol")?;

    Ok(())
}

/// Insert a resolved edge using RELATE.
pub async fn insert_edge(
    db: &Surreal<Db>,
    from: &QualifiedSymbol,
    to: &QualifiedSymbol,
    kind: &EdgeKind,
    line: u32,
) -> Result<()> {
    let from_id = from.record_id();
    let to_id = to.record_id();
    let in_file = from.file.clone();
    let out_file = to.file.clone();

    match kind {
        EdgeKind::Calls => {
            db.query(
                "RELATE type::thing($from)->calls->type::thing($to) \
                 SET line = $line, in_file = $in_file, out_file = $out_file",
            )
            .bind(("from", from_id))
            .bind(("to", to_id))
            .bind(("line", line as i64))
            .bind(("in_file", in_file))
            .bind(("out_file", out_file))
            .await
            .context("insert calls edge")?;
        }
        EdgeKind::Uses => {
            db.query(
                "RELATE type::thing($from)->uses->type::thing($to) \
                 SET in_file = $in_file, out_file = $out_file",
            )
            .bind(("from", from_id))
            .bind(("to", to_id))
            .bind(("in_file", in_file))
            .bind(("out_file", out_file))
            .await
            .context("insert uses edge")?;
        }
        EdgeKind::Imports => {
            db.query(
                "RELATE type::thing($from)->imports->type::thing($to) \
                 SET in_file = $in_file, out_file = $out_file",
            )
            .bind(("from", from_id))
            .bind(("to", to_id))
            .bind(("in_file", in_file))
            .bind(("out_file", out_file))
            .await
            .context("insert imports edge")?;
        }
        EdgeKind::Contains => {
            db.query(
                "RELATE type::thing($from)->contains->type::thing($to) \
                 SET in_file = $in_file, out_file = $out_file",
            )
            .bind(("from", from_id))
            .bind(("to", to_id))
            .bind(("in_file", in_file))
            .bind(("out_file", out_file))
            .await
            .context("insert contains edge")?;
        }
        EdgeKind::Implements => {
            db.query(
                "RELATE type::thing($from)->implements->type::thing($to) \
                 SET in_file = $in_file, out_file = $out_file",
            )
            .bind(("from", from_id))
            .bind(("to", to_id))
            .bind(("in_file", in_file))
            .bind(("out_file", out_file))
            .await
            .context("insert implements edge")?;
        }
    }

    Ok(())
}

/// Insert a chunk with its embedding.
pub async fn insert_chunk(db: &Surreal<Db>, chunk: &Chunk, embedding: Vec<f32>) -> Result<()> {
    let symbol_ref = chunk.symbol_ref.as_ref().map(|fqn| format!("symbol:⟨{}⟩", fqn));

    db.query(
        "CREATE chunk SET \
         file = $file, line_start = $line_start, line_end = $line_end, \
         content = $content, embedding = $embedding, symbol_ref = $symbol_ref",
    )
    .bind(("file", chunk.file.clone()))
    .bind(("line_start", chunk.line_start as i64))
    .bind(("line_end", chunk.line_end as i64))
    .bind(("content", chunk.content.clone()))
    .bind(("embedding", embedding))
    .bind(("symbol_ref", symbol_ref))
    .await
    .context("insert chunk")?;

    Ok(())
}

/// Upsert file metadata.
pub async fn upsert_file_meta(db: &Surreal<Db>, meta: &FileMeta) -> Result<()> {
    db.query(
        "UPSERT file_meta SET path = $path, mtime = $mtime, size = $size, repo = $repo \
         WHERE path = $path",
    )
    .bind(("path", meta.path.clone()))
    .bind(("mtime", meta.mtime))
    .bind(("size", meta.size))
    .bind(("repo", meta.repo.clone()))
    .await
    .context("upsert file_meta")?;

    Ok(())
}

// ─── Query operations ─────────────────────────────────────────────────────

/// Fetch all file_meta rows for a given repo.
pub async fn get_all_file_meta(db: &Surreal<Db>, repo: &str) -> Result<Vec<FileMeta>> {
    let rows: Vec<FileMeta> = db
        .query("SELECT path, mtime, size, repo FROM file_meta WHERE repo = $repo")
        .bind(("repo", repo.to_string()))
        .await
        .context("get all file_meta")?
        .take(0)?;
    Ok(rows)
}

/// Get a single index_meta value by key.
pub async fn get_meta(db: &Surreal<Db>, key: &str) -> Result<Option<String>> {
    let rows: Vec<IndexMeta> = db
        .query("SELECT key, value FROM index_meta WHERE key = $key")
        .bind(("key", key.to_string()))
        .await
        .context("get index_meta")?
        .take(0)?;
    Ok(rows.into_iter().next().map(|r| r.value))
}

/// Set an index_meta key/value.
pub async fn set_meta(db: &Surreal<Db>, key: &str, value: &str) -> Result<()> {
    db.query(
        "UPSERT index_meta SET key = $key, value = $value WHERE key = $key",
    )
    .bind(("key", key.to_string()))
    .bind(("value", value.to_string()))
    .await
    .context("set index_meta")?;
    Ok(())
}

/// Get all symbols from a given file (used for edge resolution).
pub async fn get_symbols_for_file(
    db: &Surreal<Db>,
    file: &str,
) -> Result<Vec<QualifiedSymbol>> {
    #[derive(Deserialize)]
    struct Row {
        file: String,
        name: String,
    }
    let rows: Vec<Row> = db
        .query("SELECT file, name FROM symbol WHERE file = $file")
        .bind(("file", file.to_string()))
        .await
        .context("get symbols for file")?
        .take(0)?;

    Ok(rows
        .into_iter()
        .map(|r| QualifiedSymbol {
            file: r.file,
            scope_path: vec![],
            name: r.name,
        })
        .collect())
}

/// Find a symbol by name across all files (for cross-file edge resolution).
pub async fn find_symbol_by_name(
    db: &Surreal<Db>,
    name: &str,
) -> Result<Vec<QualifiedSymbol>> {
    #[derive(Deserialize)]
    struct Row {
        file: String,
        name: String,
    }
    let rows: Vec<Row> = db
        .query("SELECT file, name FROM symbol WHERE name = $name")
        .bind(("name", name.to_string()))
        .await
        .context("find symbol by name")?
        .take(0)?;

    Ok(rows
        .into_iter()
        .map(|r| QualifiedSymbol {
            file: r.file,
            scope_path: vec![],
            name: r.name,
        })
        .collect())
}

/// Count indexed files for a repo.
pub async fn count_indexed_files(db: &Surreal<Db>, repo: &str) -> Result<u64> {
    #[derive(Deserialize)]
    struct Row {
        count: i64,
    }
    let rows: Vec<Row> = db
        .query("SELECT count() AS count FROM file_meta WHERE repo = $repo GROUP ALL")
        .bind(("repo", repo.to_string()))
        .await
        .context("count indexed files")?
        .take(0)?;
    Ok(rows.first().map(|r| r.count as u64).unwrap_or(0))
}
