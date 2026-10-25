//! agent-busd — the local broker. See docs/protocol.md and docs/architecture.md.

pub mod router;
pub mod session;
pub mod store;

use anyhow::Result;
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};

pub use session::AppState;

/// Transport listener type that works across platforms.
/// On Windows: named pipe + TCP fallback. On Unix: Unix socket + TCP fallback.
#[allow(dead_code)]
pub enum TransportListener {
    Tcp(TcpListener),
    #[cfg(windows)]
    NamedPipe {
        pipe_name: String,
        server: tokio::net::windows::named_pipe::NamedPipeServer,
    },
}

impl TransportListener {
    /// Accept a new connection.
    pub async fn accept(&mut self) -> Result<TransportStream> {
        match self {
            TransportListener::Tcp(l) => {
                let (stream, _) = l.accept().await?;
                Ok(TransportStream::Tcp(stream))
            }
            #[cfg(windows)]
            TransportListener::NamedPipe { pipe_name, server } => {
                // Wait for client to connect
                server.connect().await?;
                
                // Create new server for next client (must do this before spawning task)
                let next_server = tokio::net::windows::named_pipe::ServerOptions::new()
                    .create(pipe_name)?;
                
                // Take the connected server
                let connected = std::mem::replace(server, next_server);
                
                Ok(TransportStream::NamedPipe(connected))
            }
        }
    }

    /// Get the local address for display.
    pub fn local_addr(&self) -> Result<String> {
        match self {
            TransportListener::Tcp(l) => Ok(l.local_addr()?.to_string()),
            #[cfg(windows)]
            TransportListener::NamedPipe { pipe_name, .. } => Ok(pipe_name.clone()),
        }
    }
}

/// Transport stream type that works across platforms.
pub enum TransportStream {
    Tcp(TcpStream),
    #[cfg(windows)]
    NamedPipe(tokio::net::windows::named_pipe::NamedPipeServer),
}

impl tokio::io::AsyncRead for TransportStream {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            TransportStream::Tcp(s) => std::pin::Pin::new(s).poll_read(cx, buf),
            #[cfg(windows)]
            TransportStream::NamedPipe(s) => std::pin::Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl tokio::io::AsyncWrite for TransportStream {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match self.get_mut() {
            TransportStream::Tcp(s) => std::pin::Pin::new(s).poll_write(cx, buf),
            #[cfg(windows)]
            TransportStream::NamedPipe(s) => std::pin::Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            TransportStream::Tcp(s) => std::pin::Pin::new(s).poll_flush(cx),
            #[cfg(windows)]
            TransportStream::NamedPipe(s) => std::pin::Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            TransportStream::Tcp(s) => std::pin::Pin::new(s).poll_shutdown(cx),
            #[cfg(windows)]
            TransportStream::NamedPipe(s) => std::pin::Pin::new(s).poll_shutdown(cx),
        }
    }
}

impl Unpin for TransportStream {}

/// Serve an already-bound listener (used by `main` and by tests).
pub async fn serve(mut listener: TransportListener, state: Arc<AppState>) -> Result<()> {
    loop {
        let stream = match listener.accept().await {
            Ok(s) => s,
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

/// Serve one named pipe: accept, hand off, and immediately create the next
/// server instance so the accept loop never has a gap.
#[cfg(windows)]
pub async fn serve_named_pipe(
    pipe_name: String,
    mut server: tokio::net::windows::named_pipe::NamedPipeServer,
    state: Arc<AppState>,
) {
    loop {
        if let Err(e) = server.connect().await {
            eprintln!("busd: pipe connect error: {e}");
            return;
        }
        let next = tokio::net::windows::named_pipe::ServerOptions::new()
            .create(&pipe_name)
            .expect("recreate named pipe server");
        let connected = std::mem::replace(&mut server, next);
        let st = state.clone();
        tokio::spawn(async move {
            if let Err(e) = session::handle_conn(TransportStream::NamedPipe(connected), st).await {
                eprintln!("busd: pipe conn closed: {e}");
            }
        });
    }
}

pub struct Config {
    pub port: u16,
    pub db_path: std::path::PathBuf,
    /// Records the owning pid so the launcher can stop exactly this process.
    pub pid_file: std::path::PathBuf,
    /// Path for named pipe (Windows only). Default: \\.\pipe\agent-busd
    pub pipe_path: Option<String>,
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

/// A broker process that a caller can safely act on.
///
/// A recorded pid is not a claim: pids recycle. This is the raw datum plus the
/// check that turns it into one, so `--status`, the control panel's Stop
/// button, and the double-start no-op message all agree on what "the broker"
/// means instead of each guessing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrokerProcess {
    pub pid: u32,
    pub port: u16,
}

impl BrokerProcess {
    /// Does this pid exist AND is it a broker process?
    ///
    /// Without the name check, a recycled pid pointing at any other program
    /// would let a launcher kill a stranger on the user's behalf.
    pub fn is_live(&self) -> bool {
        process_is_named(self.pid, PROCESS_NAME)
    }
}

/// Image name the broker runs under, without an extension, for comparison.
const PROCESS_NAME: &str = "busd";

/// Is the process with this pid running under the given image name?
///
/// `tasklist /FI "PID eq n"` is the probe: it needs no extra dependency, and
/// the same filter the control panel already uses. A missing or inaccessible
/// process answers with no data row, which reads as "not live".
#[cfg(windows)]
fn process_is_named(pid: u32, name: &str) -> bool {
    let out = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
        .output();
    let Ok(out) = out else {
        return false;
    };
    if !out.status.success() {
        return false;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    // A matching row looks like: "busd.exe","1234","Console","1","10,000 K".
    text.lines().any(|line| {
        let first = line.split(',').next().unwrap_or("").trim_matches('"');
        let stem = first.trim_end_matches(".exe");
        stem.eq_ignore_ascii_case(name)
    })
}

#[cfg(unix)]
fn process_is_named(pid: u32, name: &str) -> bool {
    // /proc is the same idea as the Windows probe, without the extra
    // dependency: read the comm file and compare the image name.
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .map(|c| c.trim().eq_ignore_ascii_case(name))
        .unwrap_or(false)
}

/// Resolve the pid file into a live claim, or `None` if it is stale or names a
/// different process. `None` means "do not touch this pid", not "nothing runs".
pub fn live_broker(path: &std::path::Path) -> Option<BrokerProcess> {
    let pid = pid_from_file(path)?;
    let text = std::fs::read_to_string(path).ok()?;
    let port = text
        .lines()
        .nth(1)
        .and_then(|l| l.trim().parse::<u16>().ok())?;
    let proc = BrokerProcess { pid, port };
    proc.is_live().then_some(proc)
}

fn write_pid_file(path: &std::path::Path, pid: u32, port: u16) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, format!("{pid}\n{port}\n"))?;
    Ok(())
}

/// Create the transport listener.
///
/// TCP binds **first** and unconditionally: the bind is the lock that keeps a
/// second broker from serving a shared SQLite file (see `port_in_use`). On
/// Windows a named pipe binds *afterwards* as an additional face — not a
/// fallback — so pipe clients and TCP clients both reach the one broker.
///
/// Only one transport is returned by this function; the pipe, when it binds,
/// is served on a spawned task against the same state.
async fn create_listener(cfg: &Config) -> Result<TransportListener> {
    let listener = TcpListener::bind(("127.0.0.1", cfg.port))
        .await
        .map_err(|e| {
            // Name the live owner if we can verify it, never a bare recorded pid.
            let owner = live_broker(&cfg.pid_file)
                .map(|p| format!(" (pid {})", p.pid))
                .unwrap_or_default();
            anyhow::anyhow!(
                "agent-busd: already running on port {}{owner} -- {e}",
                cfg.port
            )
        })?;
    println!("agent-busd: listening on TCP {}", listener.local_addr()?);
    Ok(TransportListener::Tcp(listener))
}

/// Try to open the named pipe as an *additional* listener, to be served
/// alongside TCP. Returns `None` when the pipe cannot be created (or on
/// non-Windows), having explained why on stderr.
#[cfg(windows)]
async fn pipe_listener(cfg: &Config) -> Option<(String, tokio::net::windows::named_pipe::NamedPipeServer)> {
    let pipe_path = cfg
        .pipe_path
        .clone()
        .unwrap_or_else(|| r"\\.\pipe\agent-busd".to_string());
    // `first_pipe_instance` still guards against two brokers on one machine
    // both claiming this pipe path.
    match tokio::net::windows::named_pipe::ServerOptions::new()
        .first_pipe_instance(true)
        .create(&pipe_path)
    {
        Ok(server) => {
            println!("agent-busd: listening on named pipe {}", pipe_path);
            Some((pipe_path, server))
        }
        Err(e) => {
            eprintln!("agent-busd: named pipe {} unavailable ({e}); TCP only", pipe_path);
            None
        }
    }
}

#[cfg(not(windows))]
async fn pipe_listener(_cfg: &Config) -> Option<(String, ())> {
    None
}

/// Bind transport, open the store, serve until stopped.
///
/// Starting an already-running broker is a no-op rather than an error, so
/// double-clicking the launcher twice cannot spawn a rival writer.
pub async fn run(cfg: Config) -> Result<()> {
    // TCP binds first: the bind is the lock. A second broker over the same
    // database detects it here instead of becoming a second writer (issue #1).
    let listener = match create_listener(&cfg).await {
        Ok(l) => l,
        Err(e) => {
            // Documented contract: a double-start is a no-op, not a failure.
            println!("{e}");
            eprintln!("{e}");
            return Ok(());
        }
    };

    let store = store::Store::open(&cfg.db_path)?;
    let state = Arc::new(AppState::new(store));
    state.warm_members()?;

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

    // The named pipe, when available, is served alongside TCP against the same
    // state, so both faces reach the one broker. This never fails the start.
    #[cfg(windows)]
    {
        if let Some((pipe_name, server)) = pipe_listener(&cfg).await {
            let st = state.clone();
            tokio::spawn(async move {
                serve_named_pipe(pipe_name, server, st).await;
            });
        }
    }

    let result = tokio::select! {
        r = serve(listener, state) => r,
        _ = tokio::signal::ctrl_c() => {
            println!("agent-busd: stopping on ctrl-c");
            Ok(())
        }
    };

    // Only remove the file if it still names this process, so a restart that
    // has already claimed the port does not lose its own pid.
    if pid_from_file(&cfg.pid_file) == Some(pid) {
        let _ = std::fs::remove_file(&cfg.pid_file);
    }
    result
}