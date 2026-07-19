use std::io::Write as _;
use std::time::Duration;

use tracing::info;

use context_engine_rs::engine_boot::{BootOptions, BootedEngine, boot_engine_with_home};
use context_engine_rs::{router, server};

use crate::Cli;

pub async fn run(cli: &Cli, bind: &str, repo: String) {
    let requested_port = cli.port.unwrap_or(0);
    let BootedEngine {
        home_dir,
        data_dir,
        embeddings_dir,
        index_engine,
        repo_dbs,
        settings,
    } = match boot_engine_with_home(
        BootOptions {
            data_dir: cli.data_dir.clone(),
            embeddings_dir: cli.embeddings_dir.clone(),
            no_watchers: false,
            only_repo: Some(repo.clone()),
        },
        cli.home_dir.clone(),
    )
    .await
    {
        Ok(booted) => booted,
        Err(error) => exit_with_error(&format!("{error:#}"), 2),
    };

    let addr: std::net::SocketAddr =
        format!("{bind}:{requested_port}")
            .parse()
            .unwrap_or_else(|error| {
                exit_with_error(
                    &format!("invalid bind address '{bind}:{requested_port}': {error}"),
                    2,
                )
            });
    let idle_secs = cli
        .worker_idle_secs
        .unwrap_or(settings.read().await.worker_idle_secs);
    let idle = router::worker::IdleTracker::new(Duration::from_secs(idle_secs));

    let app = server::build_router(
        home_dir.clone(),
        data_dir,
        embeddings_dir,
        index_engine.clone(),
        repo_dbs.clone(),
        settings.clone(),
        bind,
    );
    let app = router::worker::with_idle_tracking(app, idle.clone());
    let app = router::worker::with_config_reload(
        app,
        router::worker::ConfigReloader::new(home_dir, settings),
    );

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .unwrap_or_else(|error| exit_with_error(&format!("could not bind to {addr}: {error}"), 2));
    let actual = listener
        .local_addr()
        .unwrap_or_else(|error| exit_with_error(&format!("could not read local_addr: {error}"), 2));

    println!(
        "{} port={} pid={} repo={}",
        router::worker::READY_PREFIX,
        actual.port(),
        std::process::id(),
        repo
    );
    let _ = std::io::stdout().flush();
    info!(repo = %repo, addr = %actual, "worker ready");

    // Reconcile edits made while this scale-to-zero worker was stopped.
    if let Err(error) = index_engine.trigger_index(&repo).await {
        info!(repo = %repo, error = %error, "boot catch-up incremental trigger failed (non-fatal)");
    }
    router::worker::spawn_idle_watchdog(idle, index_engine, repo_dbs, repo);

    axum::serve(listener, app)
        .await
        .unwrap_or_else(|error| exit_with_error(&format!("worker server error: {error}"), 1));
}

fn exit_with_error(message: &str, code: i32) -> ! {
    eprintln!("error: {message}");
    std::process::exit(code);
}
