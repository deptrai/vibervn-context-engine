async fn repo_index_stats_cold_or_proxy(
    State(state): State<RouterState>,
    AxumPath(repo_id): AxumPath<String>,
    req: Request,
) -> Response {
    let repo = resolve_repo_id(&repo_id);
    if let Some(response) = proxy::proxy_if_live(&state.proxy, &repo, req).await {
        return response;
    }
    match sidecar::read_sidecar(&state.data_dir, &repo) {
        Some(meta) => Json(json!({
            "repo": repo, "files": meta.file_count, "chunks": null, "symbols": null,
            "embedding_model": meta.embedding_model, "embedding_dim": meta.embedding_dim,
            "state": "indexed_cold", "last_indexed_at": meta.last_indexed_at,
            "note": "served from sidecar; open the repo to load full stats",
        }))
        .into_response(),
        None => Json(json!({
            "repo": repo, "files": 0, "chunks": 0, "symbols": 0,
            "embedding_dim": null, "state": "not_indexed", "last_indexed_at": null,
        }))
        .into_response(),
    }
}

async fn repo_graph_cold_or_proxy(
    State(state): State<RouterState>,
    AxumPath(repo_id): AxumPath<String>,
    req: Request,
) -> Response {
    let repo = resolve_repo_id(&repo_id);
    if let Some(response) = proxy::proxy_if_live(&state.proxy, &repo, req).await {
        return response;
    }
    match sidecar::read_aux_json::<crate::store::ops::CallGraph>(&state.data_dir, &repo, "graph") {
        Some(graph) => Json(json!({
            "nodes": graph.nodes, "edges": graph.edges,
            "truncated": graph.truncated, "cold": true,
        }))
        .into_response(),
        None => Json(json!({
            "nodes": [], "edges": [], "truncated": false, "cold": true,
            "note": "graph loads once the repo is indexed",
        }))
        .into_response(),
    }
}

async fn repo_status_cold_or_proxy(
    State(state): State<RouterState>,
    AxumPath(repo_id): AxumPath<String>,
    req: Request,
) -> Response {
    let repo = resolve_repo_id(&repo_id);
    if let Some(response) = proxy::proxy_if_live(&state.proxy, &repo, req).await {
        return response;
    }
    let (count, last_indexed_at) = sidecar::read_sidecar(&state.data_dir, &repo)
        .map(|meta| (meta.file_count, meta.last_indexed_at))
        .unwrap_or((0, None));
    Json(json!({
        "repo": repo, "state": "idle", "indexed_files": count,
        "total_files": count, "last_indexed_at": last_indexed_at,
    }))
    .into_response()
}

async fn repo_files_cold_or_proxy(
    State(state): State<RouterState>,
    AxumPath(repo_id): AxumPath<String>,
    req: Request,
) -> Response {
    let repo = resolve_repo_id(&repo_id);
    if let Some(response) = proxy::proxy_if_live(&state.proxy, &repo, req).await {
        return response;
    }
    let files = sidecar::read_aux_json::<serde_json::Value>(&state.data_dir, &repo, "files")
        .unwrap_or_else(|| json!([]));
    Json(json!({"files": files, "truncated": false, "cold": true})).into_response()
}

async fn repo_ignored_files_cold_or_proxy(
    State(state): State<RouterState>,
    AxumPath(repo_id): AxumPath<String>,
    req: Request,
) -> Response {
    proxy_live_or_json(
        &state,
        &resolve_repo_id(&repo_id),
        req,
        json!({"ignored": [], "cold": true}),
    )
    .await
}

async fn repo_index_events_cold_or_proxy(
    State(state): State<RouterState>,
    AxumPath(repo_id): AxumPath<String>,
    req: Request,
) -> Response {
    let repo = resolve_repo_id(&repo_id);
    proxy::proxy_if_live(&state.proxy, &repo, req)
        .await
        .unwrap_or_else(|| StatusCode::NO_CONTENT.into_response())
}

async fn repo_cancel_index_cold_or_proxy(
    State(state): State<RouterState>,
    AxumPath(repo_id): AxumPath<String>,
    req: Request,
) -> Response {
    proxy_live_or_json(
        &state,
        &resolve_repo_id(&repo_id),
        req,
        json!({"status": "ok", "note": "no active worker; nothing to cancel"}),
    )
    .await
}

async fn repo_chat_delete_cold_or_proxy(
    State(state): State<RouterState>,
    AxumPath((repo_id, _)): AxumPath<(String, String)>,
    req: Request,
) -> Response {
    proxy_live_or_json(
        &state,
        &resolve_repo_id(&repo_id),
        req,
        json!({"status": "ok", "note": "no active worker; conversation not resident"}),
    )
    .await
}

async fn proxy_live_or_json(
    state: &RouterState,
    repo: &str,
    req: Request,
    cold: serde_json::Value,
) -> Response {
    proxy::proxy_if_live(&state.proxy, repo, req)
        .await
        .unwrap_or_else(|| Json(cold).into_response())
}
