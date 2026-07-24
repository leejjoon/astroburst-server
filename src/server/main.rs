mod activity;
mod config;
mod connect;
mod error;
mod extractors;
mod handlers;
mod job;
mod router;
mod session;
mod state;
mod v2;

#[cfg(test)]
mod tests;

use std::sync::Arc;

use config::ServerConfig;

fn main() -> anyhow::Result<()> {
    // Client-side subcommand: `astroburst-server connect <ssh-target>`
    // manages an SSH tunnel to a remote (loopback-bound) server instance
    // and never starts the HTTP server itself (issue #2).
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("connect") {
        return connect::run(&args[2..]);
    }
    serve()
}

#[tokio::main]
async fn serve() -> anyhow::Result<()> {
    let cfg = Arc::new(ServerConfig::from_env());

    // Initialise the logger. Honour RUST_LOG if already set, otherwise use
    // the ASTROBURST_LOG_LEVEL value (default: "info").
    if std::env::var("RUST_LOG").is_err() {
        std::env::set_var("RUST_LOG", &cfg.log_level);
    }
    env_logger::init();

    log::info!(
        "AstroBurst Headless Server v{}",
        env!("CARGO_PKG_VERSION")
    );
    log::info!("Listening on {}", cfg.bind);
    log::info!(
        "Session TTL: {}s, Max sessions: {}",
        cfg.session_ttl.as_secs(),
        cfg.session_max
    );
    log::info!(
        "Per-session cache: {} entries / {} MiB",
        cfg.cache_max_entries,
        cfg.cache_max_bytes / (1024 * 1024)
    );
    log::info!("Cleanup interval: {}s", cfg.cleanup_interval.as_secs());

    let state = state::AppState::new(Arc::clone(&cfg));

    session::SessionManager::start_ttl_cleaner(
        state.sessions.clone(),
        Arc::clone(&cfg),
    );

    let app = router::build_router(state);
    let listener = tokio::net::TcpListener::bind(cfg.bind).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
