async fn post_index_all(State(state): State<RouterState>) -> Response {
    let repos = match ensure_dir_and_load(&state.home_dir) {
        Ok(settings) => settings.repos,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{error}")})),
            )
                .into_response();
        }
    };
    let mut triggered = Vec::new();
    let mut failed = Vec::new();
    for repo in repos {
        let repo = normalize_repo_path(&repo);
        match proxy::forward_json_to_worker(
            &state.proxy,
            &repo,
            &format!("/api/repos/{}/index", urlencode_segment(&repo)),
            json!({}),
        )
        .await
        {
            Ok(_) => triggered.push(repo),
            Err(error) => failed.push(json!({"repo": repo, "error": error})),
        }
    }
    (
        StatusCode::ACCEPTED,
        Json(json!({"status": "accepted", "triggered": triggered, "failed": failed})),
    )
        .into_response()
}

async fn get_index_status(State(state): State<RouterState>) -> Response {
    let settings = match ensure_dir_and_load(&state.home_dir) {
        Ok(settings) => settings,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{error}")})),
            )
                .into_response();
        }
    };
    let ready = state.proxy.registry.ready_repos().await;
    let spawning = state.proxy.registry.spawning_repos().await;
    let mut statuses = Vec::new();
    for repo in settings.repos {
        let repo = normalize_repo_path(&repo);
        if ready.contains(&repo)
            && let Some(mut status) = proxy::get_json_if_live(
                &state.proxy,
                &repo,
                &format!("/api/repos/{}/status", urlencode_segment(&repo)),
            )
            .await
        {
            if let Some(object) = status.as_object_mut() {
                object.insert("repo".to_string(), json!(repo));
                object.insert("worker_active".to_string(), json!(true));
            }
            statuses.push(status);
            continue;
        }
        if spawning.contains(&repo) {
            statuses.push(json!({
                "repo": repo, "state": "indexing", "phase": "starting",
                "indexed_files": 0, "total_files": 0, "worker_active": true,
            }));
            continue;
        }
        match sidecar::read_sidecar(&state.data_dir, &repo) {
            Some(meta) => statuses.push(json!({
                "repo": repo, "state": meta.state, "indexed_files": meta.file_count,
                "last_indexed_at": meta.last_indexed_at, "worker_active": false,
            })),
            None => statuses.push(json!({
                "repo": repo, "state": "not_indexed", "indexed_files": 0,
                "last_indexed_at": null, "worker_active": false,
            })),
        }
    }
    Json(statuses).into_response()
}

fn urlencode_segment(repo: &str) -> String {
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    URL_SAFE_NO_PAD.encode(repo.as_bytes())
}
