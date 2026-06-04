pub mod engine;
pub mod graph_expand;
pub mod merger;

pub use engine::{CodeResult, QueryResult, QueryTiming, run_query};

use std::collections::HashMap;
use surrealdb::Surreal;
use surrealdb::engine::local::Db;

/// Find the DB handle whose repo key is a prefix of `file`, or the first DB as fallback.
pub(crate) fn find_db_for_file<'a>(
    db_map: &'a HashMap<String, Surreal<Db>>,
    file: &str,
) -> Option<&'a Surreal<Db>> {
    for (repo_path, db) in db_map {
        if file.starts_with(repo_path.as_str()) {
            return Some(db);
        }
    }
    db_map.values().next()
}
