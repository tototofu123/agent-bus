use anyhow::Result;
use std::path::PathBuf;

fn main() -> Result<()> {
    let mut port: u16 = 47113;
    let mut db: Option<PathBuf> = None;
    let mut pid_file: Option<PathBuf> = None;
    let mut pipe_path: Option<String> = None;
    let mut status = false;

    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--port" => port = args.next().and_then(|v| v.parse().ok()).unwrap_or(port),
            "--db" => db = args.next().map(PathBuf::from),
            "--pid-file" => pid_file = args.next().map(PathBuf::from),
            "--pipe" => pipe_path = args.next(),
            "--status" => status = true,
            "--version" => {
                println!("agent-busd {}", proto::V);
                return Ok(());
            }
            "--help" => {
                print_usage();
                return Ok(());
            }
            _ => {}
        }
    }

    // One global home for all sessions, folders and logins, so state survives
    // closing the shell and reopening the machine.
    let home = std::env::var("AGENT_BUS_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let mut h = home_dir();
            h.push(".agent-bus");
            h
        });
    let db_path = db.unwrap_or_else(|| home.join("history.sqlite"));
    let pid_path = pid_file.unwrap_or_else(|| home.join("busd.pid"));

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    // Query mode for the launcher: exit 0 while the broker is up, 1 otherwise,
    // so a script can branch on it directly.
    if status {
        let listening = rt.block_on(busd::port_in_use(port));
        // A pid only counts if it is alive AND is a broker, so a recycled pid
        // is reported as unknown rather than as the broker (issue #5).
        let live = busd::live_broker(&pid_path);
        match (listening, live) {
            (true, Some(p)) => println!("running port={port} pid={}", p.pid),
            (true, None) => println!("running port={port} pid=unknown"),
            (false, _) => println!("stopped port={port}"),
        }
        return Ok(());
    }

    rt.block_on(busd::run(busd::Config {
        port,
        db_path,
        pid_file: pid_path,
        pipe_path,
    }))
}

fn home_dir() -> PathBuf {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

fn print_usage() {
    println!(
        r#"agent-busd {} — local agent message bus

USAGE:
    agent-busd [OPTIONS]

OPTIONS:
    --port <PORT>        TCP port to listen on (default: 47113)
    --db <PATH>          SQLite database path (default: $AGENT_BUS_HOME/history.sqlite)
    --pid-file <PATH>    PID file path (default: $AGENT_BUS_HOME/busd.pid)
    --pipe <PATH>        Windows named pipe path (default: \\.\pipe\agent-busd)
    --status             Check if broker is running (exit 0 if running, 1 if stopped)
    --version            Print version and exit
    --help               Print this help and exit

ENVIRONMENT:
    AGENT_BUS_HOME       Base directory for data files (default: ~/.agent-bus)
    AGENT_BUS_PORT       Default TCP port (default: 47113)

TRANSPORTS:
    Windows: Named pipe (\\.\pipe\agent-busd) with TCP loopback fallback
    Unix:    Unix socket with TCP loopback fallback
"#,
        proto::V
    );
}
