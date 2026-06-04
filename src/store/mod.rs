pub mod ops;
pub mod schema;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use surrealdb::Surreal;
use surrealdb::engine::local::{Db, SurrealKv};

use crate::store::schema::SCHEMA_DDL;

/// Sanitize a repo path to a safe directory name (max 64 chars).
pub fn sanitize_repo_name(repo_path: &str) -> String {
    let sanitized: String = repo_path
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect();
    let trimmed = sanitized.trim_matches('_');
    if trimmed.len() > 64 {
        trimmed[trimmed.len() - 64..].to_string()
    } else {
        trimmed.to_string()
    }
}

/// Return the SurrealDB data directory for a given repo.
pub fn db_path(home_dir: &Path, repo_path: &str) -> PathBuf {
    let name = sanitize_repo_name(repo_path);
    home_dir
        .join(".vibervn")
        .join("context-engine")
        .join("surreal")
        .join(name)
}

/// Open (or create) a SurrealDB RocksDB database for the given repo.
/// Runs schema DDL to ensure all tables/indexes exist.
pub async fn open_db(home_dir: &Path, repo_path: &str) -> Result<Surreal<Db>> {
    let path = db_path(home_dir, repo_path);
    std::fs::create_dir_all(&path).with_context(|| format!("create db dir {:?}", path))?;

    let db = Surreal::new::<SurrealKv>(path.to_str().unwrap())
        .await
        .context("open surrealdb")?;

    db.use_ns("context_engine")
        .use_db(sanitize_repo_name(repo_path))
        .await
        .context("select ns/db")?;

    // Run schema DDL — each statement is idempotent (DEFINE ... IF NOT EXISTS equivalent
    // via SurrealDB's DEFINE behaviour which doesn't error on re-execution).
    db.query(SCHEMA_DDL).await.context("apply schema DDL")?;

    Ok(db)
}
