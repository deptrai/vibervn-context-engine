//! End-to-end proof that an idle-dropped MCP session is transparently restored
//! from the bounded session store instead of returning `404 "Session not found"`.
//!
//! This is the real-flow test the unit tests in `mcp_session_store.rs` can't be:
//! those only exercise the LRU get/put/evict. Here we boot the actual
//! `StreamableHttpService` (the same transport `server.rs` mounts at `/mcp`),
//! wired with our `BoundedSessionStore`, and drive it over real HTTP:
//!
//!   1. POST `initialize` (no session id)  -> server creates a session, persists
//!      its `initialize_params` to the store, returns the id in `mcp-session-id`.
//!   2. Simulate the idle timeout exactly as rmcp does it: call
//!      `close_session` on the manager — drops the live worker from the in-memory
//!      map, but does NOT delete the store entry (only a client DELETE does).
//!   3. Assert the precondition: manager no longer `has_session`, store still has it.
//!   4. GET with the stale session id -> must be served (200), proving
//!      `try_restore_from_store` re-created the worker + replayed `initialize`.
//!   5. Control: a never-seen session id (not in the store) must still 404.

use std::net::SocketAddr;
use std::sync::Arc;

use rmcp::handler::server::ServerHandler;
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService,
    SessionManager,
    session::local::LocalSessionManager,
    session::store::SessionStore,
};
use tokio::net::TcpListener;

use context_engine_rs::mcp_session_store::BoundedSessionStore;

/// Minimal `ServerHandler` — all methods default; `get_info` returns a default
/// `ServerInfo`, which is enough for the `initialize` handshake to succeed.
#[derive(Clone)]
struct TestHandler;
impl ServerHandler for TestHandler {}

/// Boot an axum server that mounts the real `StreamableHttpService` at `/mcp`,
/// backed by our `BoundedSessionStore`. Returns the bound address plus the
/// manager and store handles so the test can drive the lifecycle directly.
async fn start_mcp_server() -> (
    SocketAddr,
    Arc<LocalSessionManager>,
    Arc<BoundedSessionStore>,
) {
    let store = Arc::new(BoundedSessionStore::new());
    let manager = Arc::new(LocalSessionManager::default());

    // Same construction as server.rs::mcp_config_with_store: default config
    // (short keep_alive, loopback) with our store attached. Commenting out the
    // line below makes `idle_dropped_session_is_restored_not_404` fail — proof
    // the store is what enables restore (without it the stale GET 404s).
    let mut config = StreamableHttpServerConfig::default();
    config.session_store = Some(store.clone());

    let service = StreamableHttpService::new(
        || Ok(TestHandler),
        manager.clone(),
        config,
    );

    let app = axum::Router::new().nest_service("/mcp", service);

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("server error");
    });
    (addr, manager, store)
}

/// POST an `initialize` request (no session id) and return the `mcp-session-id`
/// the server assigns. We only read response status + headers; the SSE body is
/// left unread (it streams), which is fine — the session is created and its
/// params persisted before the response headers are emitted.
async fn initialize_session(client: &reqwest::Client, addr: SocketAddr) -> String {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "restore-test", "version": "1.0.0" }
        }
    });
    let res = client
        .post(format!("http://{addr}/mcp"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .json(&body)
        .send()
        .await
        .expect("initialize request");
    assert!(
        res.status().is_success(),
        "initialize should succeed, got {}",
        res.status()
    );
    res.headers()
        .get("mcp-session-id")
        .expect("server must assign a session id on initialize")
        .to_str()
        .expect("ascii session id")
        .to_owned()
}

/// GET the standalone stream for `session_id`. Returns the HTTP status, which is
/// the discriminator: 200 = session served (live or restored), 404 = not found.
async fn get_stream_status(
    client: &reqwest::Client,
    addr: SocketAddr,
    session_id: &str,
) -> reqwest::StatusCode {
    client
        .get(format!("http://{addr}/mcp"))
        .header("accept", "text/event-stream")
        .header("mcp-session-id", session_id)
        .send()
        .await
        .expect("GET stream request")
        .status()
}

/// The core proof: init -> idle-drop -> stale request is restored, not 404'd.
#[tokio::test]
async fn idle_dropped_session_is_restored_not_404() {
    let (addr, manager, store) = start_mcp_server().await;
    let client = reqwest::Client::new();

    // 1. Establish a session.
    let session_id = initialize_session(&client, addr).await;

    // Sanity: the live session exists and the store has its params.
    assert!(
        manager.has_session(&session_id.clone().into()).await.unwrap(),
        "session should be live right after initialize"
    );
    assert!(
        store.load(&session_id).await.unwrap().is_some(),
        "store should hold init params after initialize"
    );

    // 2. Simulate the idle timeout EXACTLY as rmcp does: close_session drops the
    //    live worker from the in-memory map. It must NOT touch the store (only a
    //    client DELETE deletes from the store).
    manager
        .close_session(&session_id.clone().into())
        .await
        .expect("close_session");

    // 3. Precondition for a meaningful test: the worker is gone but the store
    //    entry survives — this is the exact state that used to produce 404.
    assert!(
        !manager.has_session(&session_id.clone().into()).await.unwrap(),
        "worker must be dropped from the live map after close_session"
    );
    assert!(
        store.load(&session_id).await.unwrap().is_some(),
        "idle close must NOT delete the store entry"
    );

    // 4. The stale request now arrives. Before the fix this 404'd; with the
    //    store, try_restore_from_store transparently re-creates the worker and
    //    replays initialize, so it is served.
    let status = get_stream_status(&client, addr, &session_id).await;
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "stale session id must be restored from the store and served (got {status})"
    );

    // And the restore actually re-populated the live map.
    assert!(
        manager.has_session(&session_id.into()).await.unwrap(),
        "restore must re-create the live worker"
    );
}

/// Control: a session id that was never stored (e.g. truly unknown) must still
/// 404 — restore only resurrects sessions the store knows about.
#[tokio::test]
async fn unknown_session_id_still_404s() {
    let (addr, _manager, _store) = start_mcp_server().await;
    let client = reqwest::Client::new();

    let status = get_stream_status(&client, addr, "never-existed-session-id").await;
    assert_eq!(
        status,
        reqwest::StatusCode::NOT_FOUND,
        "a session id absent from the store must 404, not be fabricated"
    );
}



