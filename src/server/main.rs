mod error;
mod extractors;
mod handlers;
mod job;
mod router;
mod session;
mod state;

#[cfg(test)]
mod tests;

use std::net::SocketAddr;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let state = state::AppState::new();

    session::SessionManager::start_ttl_cleaner(state.sessions.clone());

    let app = router::build_router(state);
    let addr = SocketAddr::from(([127, 0, 0, 1], 8080));
    println!("astroburst-server listening on {addr}");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
