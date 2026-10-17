use anyhow::Result;
use std::path::PathBuf;

fn main() -> Result<()> {
    let mut port: u16 = 47113;
    let mut db: Option<PathBuf> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--port" => port = args.next().and_then(|v| v.parse().ok()).unwrap_or(port),
            "--db" => db = args.next().map(PathBuf::from),
            "--version" => {
                println!("agent-busd {}", proto::V);
                return Ok(());
            }
            _ => {}
        }
    }
    let home = std::env::var("AGENT_BUS_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let mut h = home_dir();
            h.push(".agent-bus");
            h
        });
    let db_path = db.unwrap_or_else(|| home.join("history.sqlite"));

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(busd::run(busd::Config { port, db_path }))
}

fn home_dir() -> PathBuf {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}
