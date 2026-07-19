async fn backfill_missing_sidecars(home_dir: &std::path::Path, data_dir: &std::path::Path) {
    let settings = match ensure_dir_and_load(home_dir) {
        Ok(settings) => settings,
        Err(_) => return,
    };
    for repo in &settings.repos {
        let repo = normalize_repo_path(repo);
        if sidecar::read_sidecar(data_dir, &repo).is_some() {
            continue;
        }
        let generation = settings.repo_generation(&repo);
        if !crate::store::db_path(data_dir, &repo, generation).exists() {
            continue;
        }
        let db = match crate::store::open_db(data_dir, &repo, generation).await {
            Ok(db) => db,
            Err(error) => {
                tracing::warn!(repo = %repo, error = %format!("{error:#}"), "sidecar backfill could not open DB");
                continue;
            }
        };
        let count = crate::store::ops::count_indexed_files(&db, &repo)
            .await
            .unwrap_or(0);
        if count == 0 {
            continue;
        }
        let meta = sidecar::RepoSidecar {
            file_count: count,
            last_indexed_at: crate::store::ops::get_meta(&db, "last_indexed_at")
                .await
                .ok()
                .flatten(),
            state: "indexed".to_string(),
            embedding_model: settings.embedding.model.clone(),
            embedding_dim: settings.embedding.dimensions.unwrap_or(0) as u64,
            schema: sidecar::SIDECAR_SCHEMA,
        };
        if let Err(error) = sidecar::write_sidecar(data_dir, &repo, &meta) {
            tracing::warn!(repo = %repo, error = %format!("{error:#}"), "sidecar backfill write failed");
        } else {
            tracing::info!(repo = %repo, count, "sidecar backfilled at router boot");
        }
        if sidecar::read_aux_json::<crate::store::ops::CallGraph>(data_dir, &repo, "graph").is_none()
            && let Ok(graph) = crate::store::ops::compute_and_cache_graph(&db).await
        {
            let _ = sidecar::write_aux_json(data_dir, &repo, "graph", &graph);
        }
        if sidecar::read_aux_json::<serde_json::Value>(data_dir, &repo, "files").is_none()
            && let Ok(files) = crate::store::ops::files_page(&db, &repo, 2000, None).await
        {
            let _ = sidecar::write_aux_json(data_dir, &repo, "files", &files);
        }
    }
}
