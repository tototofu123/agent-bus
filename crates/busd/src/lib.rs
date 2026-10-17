//! agent-busd — the local broker. See docs/protocol.md and docs/architecture.md.

pub mod router;
pub mod session;
pub mod store;

use anyhow::Result;
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};

pub use session::AppState;

/// Serve an already-bound listener (used by `main` and by tests).
pub async fn serve(listener: TcpListener, state: Arc<AppState>) -> Result<()> {
    loop {
        let (stream, _peer): (TcpStream, _) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                eprintln!("busd: accept error: {e}");
                continue;
            }
        };
        let st = state.clone();
        tokio::spawn(async move {
            if let Err(e) = session::handle_conn(stream, st).await {
                eprintln!("busd: conn closed: {e}");
            }
        });
    }
}

pub struct Config {
    pub port: u16,
    pub db_path: std::path::PathBuf,
}

/// Bind loopback, open the store, serve until killed.
pub async fn run(cfg: Config) -> Result<()> {
    let store = store::Store::open(&cfg.db_path)?;
    let state = Arc::new(AppState::new(store));
    state.warm_members()?;
    // TCP loopback is the universal transport (ADR-0003); Unix socket / named
    // pipe ride alongside in later Phase 1 iterations.
    let listener = TcpListener::bind(("127.0.0.1", cfg.port)).await?;
    let addr = listener.local_addr()?;
    println!(
        "agent-busd v{} listening on {} db={} pid={}",
        proto::V,
        addr,
        cfg.db_path.display(),
        std::process::id()
    );
    serve(listener, state).await
}
