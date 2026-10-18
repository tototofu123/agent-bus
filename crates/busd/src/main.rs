use anyhow::Result;
use std::path::PathBuf;

fn main() -> Result<()> {
    let mut port: u16 = 47113;
    let mut db: Option<PathBuf> = None;
    let mut pid_file: Option<PathBuf> = None;
    let mut status = false;

    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--port" => port = args.next().and_then(|v| v.parse().ok()).unwrap_or(port),
            "--db" => db = args.next().map(PathBuf::from),
            "--pid-file" => pid_file = args.next().map(PathBuf::from),
            "--status" => status = true,
            "--version" => {
                println!("agent-busd {}", proto::V);
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
        if rt.block_on(busd::port_in_use(port)) {
            let pid = busd::pid_from_file(&pid_path)
                .map(|p| p.to_string())
                .unwrap_or_else(|| "unknown".into());
            println!("running port={port} pid={pid}");
        } else {
            println!("stopped port={port}");
        }
        return Ok(());
    }

    rt.block_on(busd::run(busd::Config {
        port,
        db_path,
        pid_file: pid_path,
    }))
}

fn home_dir() -> PathBuf {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}
