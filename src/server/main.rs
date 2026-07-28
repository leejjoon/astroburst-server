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
mod tui;
mod v2;

#[cfg(test)]
mod tests;

use std::sync::Arc;

use config::ServerConfig;

fn main() -> anyhow::Result<()> {
    // Client-side subcommands that never start the HTTP server:
    // - `connect <ssh-target>` manages an SSH tunnel to a remote
    //   (loopback-bound) server instance (issue #2);
    // - `tui [URL]` runs the live dashboard against a local/direct server
    //   (issue #3; `connect ... --tui` covers remote ones).
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("connect") => return connect::run(&args[2..]),
        Some("tui") => return tui::run_standalone(&args[2..]),
        Some("-h") | Some("--help") | Some("help") => {
            print_usage();
            return Ok(());
        }
        Some("-V") | Some("--version") => {
            println!("astroburst-server {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        // Any unrecognised argument — a stray flag OR a positional such as a
        // hostname meant for `connect` — must not silently boot the server (it
        // would bind the socket and swallow the mistake); print usage and fail.
        // The server takes no positional args, so the only "start" case is no
        // args at all (`None`).
        Some(other) => {
            let kind = if other.starts_with('-') { "flag" } else { "command" };
            eprintln!("astroburst-server: unknown {kind} '{other}'\n");
            print_usage();
            std::process::exit(2);
        }
        None => {}
    }
    serve()
}

/// Print CLI usage. The server itself takes no positional flags — runtime
/// configuration is entirely via `ASTROBURST_*` environment variables (see
/// `ServerConfig`); the only argv subcommands are the client-side helpers.
fn print_usage() {
    let d = ServerConfig::default();
    print!(
        "\
astroburst-server {version}

USAGE:
    astroburst-server                 Start the HTTP server (config via env vars)
    astroburst-server connect <target>  Open an SSH tunnel to a remote server
    astroburst-server tui [URL]         Run the live dashboard against a server
    astroburst-server --help            Print this help
    astroburst-server --version         Print the version

The server takes no flags; it is configured through environment variables:

    ASTROBURST_BIND             TCP address to listen on   [default: {bind}]
    ASTROBURST_SESSION_TTL      idle seconds before evict  [default: {ttl}]
    ASTROBURST_SESSION_MAX      max concurrent sessions    [default: {smax}]
    ASTROBURST_JOBS_MAX         max concurrent CPU jobs    [default: {jobs}]
    ASTROBURST_CACHE_MAX_ENTRIES per-session LRU slots     [default: {centries}]
    ASTROBURST_CACHE_MAX_BYTES  per-session LRU byte cap   [default: {cbytes}]
    ASTROBURST_CLEANUP_INTERVAL TTL sweep interval (secs)  [default: {clean}]
    ASTROBURST_LOG_LEVEL        log level (RUST_LOG wins)   [default: {log}]
",
        version = env!("CARGO_PKG_VERSION"),
        bind = d.bind,
        ttl = d.session_ttl.as_secs(),
        smax = d.session_max,
        jobs = d.jobs_max,
        centries = d.cache_max_entries,
        cbytes = d.cache_max_bytes,
        clean = d.cleanup_interval.as_secs(),
        log = d.log_level,
    );
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
