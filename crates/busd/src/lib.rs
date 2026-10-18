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
    /// Records the owning pid so the launcher can stop exactly this process.
    pub pid_file: std::path::PathBuf,
}

/// Is something already listening on the broker port? Binding is the probe, so
/// this needs no platform-specific liveness check. The port is the real mutex:
/// two brokers over one SQLite file would fight over `seq`.
pub async fn port_in_use(port: u16) -> bool {
    TcpListener::bind(("127.0.0.1", port)).await.is_err()
}

/// The pid recorded in the pid file, if any. May be stale if the broker was
/// killed outright, so callers that act on it must confirm the process first.
pub fn pid_from_file(path: &std::path::Path) -> Option<u32> {
    std::fs::read_to_string(path)
        .ok()?
        .lines()
        .next()?
        .trim()
        .parse()
        .ok()
}

fn write_pid_file(path: &std::path::Path, pid: u32, port: u16) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, format!("{pid}\n{port}\n"))?;
    Ok(())
}

/// Bind loopback, open the store, serve until stopped.
///
/// Starting an already-running broker is a no-op rather than an error, so
/// double-clicking the launcher twice cannot spawn a rival writer.
pub async fn run(cfg: Config) -> Result<()> {
    // Bind before touching the store: if the port is taken we want to bail out
    // without a second process ever opening the database.
    let listener = match TcpListener::bind(("127.0.0.1", cfg.port)).await {
        Ok(l) => l,
        Err(e) => {
            let owner = pid_from_file(&cfg.pid_file)
                .map(|p| format!(" (pid {p})"))
                .unwrap_or_default();
            eprintln!("agent-busd: already running on port {}{owner} -- {e}", cfg.port);
            return Ok(());
        }
    };

    let store = store::Store::open(&cfg.db_path)?;
    let state = Arc::new(AppState::new(store));
    state.warm_members()?;

    // TCP loopback is the universal transport (ADR-0003); Unix socket / named
    // pipe ride alongside in later Phase 1 iterations.
    let addr = listener.local_addr()?;
    let pid = std::process::id();
    write_pid_file(&cfg.pid_file, pid, cfg.port)?;
    println!(
        "agent-busd v{} listening on {} db={} pid={}",
        proto::V,
        addr,
        cfg.db_path.display(),
        pid
    );

    let result = tokio::select! {
        r = serve(listener, state) => r,
        _ = tokio::signal::ctrl_c() => {
            println!("agent-busd: stopping on ctrl-c");
            Ok(())
        }
    };

    // Only remove the file if it is still ours, so a restart that has already
    // claimed the port does not lose its own pid.
    if pid_from_file(&cfg.pid_file) == Some(pid) {
        let _ = std::fs::remove_file(&cfg.pid_file);
    }
    result
}
