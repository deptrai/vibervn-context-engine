async fn proxy_repo_root(
    State(state): State<RouterState>,
    AxumPath(repo_id): AxumPath<String>,
    req: Request,
) -> Response {
    acquire_and_proxy(&state.proxy, &resolve_repo_id(&repo_id), req).await
}

async fn proxy_repo_subpath(
    State(state): State<RouterState>,
    AxumPath((repo_id, _)): AxumPath<(String, String)>,
    req: Request,
) -> Response {
    acquire_and_proxy(&state.proxy, &resolve_repo_id(&repo_id), req).await
}

async fn proxy_mcp_repo(
    State(state): State<RouterState>,
    AxumPath(repo_id): AxumPath<String>,
    req: Request,
) -> Response {
    match resolve_mcp_repo_name(&state.home_dir, &repo_id) {
        Some(repo) => acquire_and_proxy(&state.proxy, &repo, req).await,
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": format!("unknown repo: {repo_id}")})),
        )
            .into_response(),
    }
}

async fn proxy_repo_index_post(
    State(state): State<RouterState>,
    AxumPath(repo_id): AxumPath<String>,
    req: Request,
) -> Response {
    acquire_and_proxy(&state.proxy, &resolve_repo_id(&repo_id), req).await
}

async fn repo_delete_index_native(
    State(state): State<RouterState>,
    AxumPath(repo_id): AxumPath<String>,
    AxumQuery(params): AxumQuery<std::collections::HashMap<String, String>>,
) -> Response {
    let repo = resolve_repo_id(&repo_id);
    let remove_repo = params
        .get("remove_repo")
        .is_some_and(|value| value == "true" || value == "1");
    state.proxy.registry.kill(&repo).await;

    let settings = match ensure_dir_and_load(&state.home_dir) {
        Ok(settings) => settings,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("could not load settings: {error}")})),
            )
                .into_response();
        }
    };
    let generation = settings.repo_generation(&repo);
    let mut updated = settings.clone();
    updated
        .repo_generations
        .insert(repo.clone(), generation.saturating_add(1));
    if remove_repo {
        updated.repos.retain(|configured| configured != &repo);
    }
    let target = crate::config::config_path(&state.home_dir);
    if let Err(error) = tokio::task::spawn_blocking(move || {
        crate::config::write_settings_atomic(&target, &updated)
    })
    .await
    .map_err(|error| format!("join: {error}"))
    .and_then(|result| result.map_err(|error| error.to_string()))
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("failed to persist generation bump: {error}")})),
        )
            .into_response();
    }

    let removed = crate::store::remove_old_generation_dir(&state.data_dir, &repo, generation).await;
    sidecar::remove_all_sidecars(&state.data_dir, &repo);
    if removed {
        Json(json!({"status": "ok"})).into_response()
    } else {
        Json(json!({
            "status": "pending",
            "message": "old index directory not fully removed yet; it will be reclaimed on next restart",
        }))
        .into_response()
    }
}

fn peek_body_field(bytes: &[u8], field: &str) -> Option<String> {
    serde_json::from_slice::<serde_json::Value>(bytes)
        .ok()?
        .get(field)?
        .as_str()
        .map(str::to_string)
}

async fn proxy_by_body_field(state: &RouterState, field: &str, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let bytes = match axum::body::to_bytes(body, 16 * 1024 * 1024).await {
        Ok(bytes) => bytes,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("could not read request body: {error}")})),
            )
                .into_response();
        }
    };
    let repo = match peek_body_field(&bytes, field).map(|repo| normalize_repo_path(repo.trim())) {
        Some(repo) if !repo.is_empty() => repo,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("`{field}` is required to route the request to a repository worker")})),
            )
                .into_response();
        }
    };
    acquire_and_proxy(
        &state.proxy,
        &repo,
        Request::from_parts(parts, axum::body::Body::from(bytes)),
    )
    .await
}

async fn proxy_query_by_body(State(state): State<RouterState>, req: Request) -> Response {
    proxy_by_body_field(&state, "repo", req).await
}

async fn proxy_mcp_tool_by_body(State(state): State<RouterState>, req: Request) -> Response {
    proxy_by_body_field(&state, "workspace_full_path", req).await
}

async fn proxy_file_retrieval_by_body(State(state): State<RouterState>, req: Request) -> Response {
    proxy_by_body_field(&state, "workspace_full_path", req).await
}
