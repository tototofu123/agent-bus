//! `bus` — the universal adapter. One-shot commands over TCP loopback NDJSON.

mod watch;

use anyhow::{anyhow, Context, Result};
use clap::{Args, Parser, Subcommand};
use proto::{decode_line, encode_line, Agent};
use serde_json::{json, Value};
use std::io::Write as _;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;

#[derive(Parser)]
#[command(name = "bus", version, about = "agent-bus client — rooms for agents")]
struct Cli {
    /// daemon port (AGENT_BUS_PORT overrides; default 47113)
    #[arg(long, global = true)]
    port: Option<u16>,
    /// transport to try first: tcp (default) or pipe
    #[arg(long, global = true, default_value = "tcp")]
    transport: String,
    /// named pipe path when transport is pipe (default \\.\pipe\agent-busd)
    #[arg(long, global = true)]
    pipe: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Register an agent identity; prints + stores the token
    Register(RegisterArgs),
    /// Join a room
    Join(RoomArgs),
    /// Leave a room
    Leave(RoomArgs),
    /// Send a typed message
    Send(SendArgs),
    /// Send many messages in one frame (one commit on the broker)
    SendBatch(SendBatchArgs),
    /// Block until a message for me arrives (this is push, as far as you can tell)
    Recv(RecvArgs),
    /// Live-stream room traffic until Ctrl-C
    Tail(TailArgs),
    /// Three-lane live view: main agent, subagents, report
    Watch(WatchArgs),
    /// Who is here
    Who(WhoArgs),
    /// Announce this agent is alive (presence heartbeat, issue #9)
    Heartbeat(HeartbeatArgs),
    /// List rooms
    Rooms,
    /// Replay a room from a cursor
    History(HistoryArgs),
    /// Manage scheduled jobs (cron posts task.offer into rooms)
    Schedule(ScheduleArgs),
}

#[derive(Args)]
struct RegisterArgs {
    /// agent id, e.g. codex@myrepo (default $AGENT_ID)
    #[arg(long)]
    agent: Option<String>,
    #[arg(long, default_value = "bus-cli")]
    harness: String,
    #[arg(long)]
    model: Option<String>,
    #[arg(long, value_delimiter = ',')]
    caps: Vec<String>,
    #[arg(long)]
    session: Option<String>,
}

#[derive(Args)]
struct RoomArgs {
    #[arg(long)]
    room: String,
    #[arg(long)]
    agent: Option<String>,
}

#[derive(Args)]
struct SendArgs {
    #[arg(long)]
    room: String,
    #[arg(long)]
    kind: String,
    #[arg(long)]
    text: Option<String>,
    /// kind-specific JSON payload
    #[arg(long)]
    payload: Option<String>,
    #[arg(long)]
    thread: Option<String>,
    #[arg(long)]
    reply_to: Option<String>,
    #[arg(long)]
    agent: Option<String>,
}

#[derive(Args)]
struct SendBatchArgs {
    #[arg(long)]
    room: String,
    /// path to a JSON array of message bodies, or `-` for stdin
    #[arg(long)]
    file: String,
    #[arg(long)]
    agent: Option<String>,
}

#[derive(Args)]
struct RecvArgs {
    #[arg(long)]
    room: Option<String>,
    /// seconds to block (default 30)
    #[arg(long, default_value_t = 30)]
    timeout: u64,
    #[arg(long, default_value = "text")]
    format: String,
    #[arg(long)]
    agent: Option<String>,
}

#[derive(Args)]
struct TailArgs {
    /// comma-separated room filter
    #[arg(long)]
    rooms: Option<String>,
    /// comma-separated kind filter
    #[arg(long, value_delimiter = ',')]
    kinds: Option<Vec<String>>,
    #[arg(long, default_value = "text")]
    format: String,
    #[arg(long)]
    agent: Option<String>,
}

#[derive(Args)]
struct WatchArgs {
    /// room to watch
    #[arg(long, default_value = "#repo:agent-bus")]
    room: String,
    /// identity treated as the main agent (default $AGENT_ID)
    #[arg(long)]
    agent: Option<String>,
    /// rows kept per lane
    #[arg(long, default_value_t = 40)]
    limit: usize,
}
#[derive(Args)]
struct WhoArgs {
    #[arg(long)]
    room: Option<String>,
}

#[derive(Args)]
struct HeartbeatArgs {
    #[arg(long)]
    agent: Option<String>,
}

#[derive(Args)]
struct HistoryArgs {
    #[arg(long)]
    room: String,
    /// replay from this seq (exclusive)
    #[arg(long, default_value_t = 0)]
    since: i64,
    #[arg(long, default_value_t = 500)]
    limit: i64,
    /// print full envelopes as JSON lines
    #[arg(long)]
    full: bool,
}

#[derive(Args)]
struct ScheduleArgs {
    #[command(subcommand)]
    cmd: ScheduleCmd,
}

#[derive(Subcommand)]
enum ScheduleCmd {
    /// Add a schedule: posts a task.offer into a room on a cron
    Add(ScheduleAddArgs),
    /// List schedules
    List(ScheduleListArgs),
    /// Remove a schedule by name
    Remove(ScheduleNameArgs),
    /// Enable a schedule by name
    Enable(ScheduleNameArgs),
    /// Disable a schedule by name
    Disable(ScheduleNameArgs),
    /// Run one tick: fire every due schedule once (run by an external timer each minute)
    Tick(ScheduleTickArgs),
}

#[derive(Args)]
struct ScheduleAddArgs {
    #[arg(long)]
    name: String,
    #[arg(long)]
    room: String,
    /// cron: 5-field crontab, 6/7-field, or @daily/@weekly/@monthly/@yearly
    #[arg(long)]
    cron: String,
    #[arg(long, default_value = "")]
    text: String,
    /// comma-separated file globs for the task.offer payload (may be empty)
    #[arg(long, value_delimiter = ',')]
    files: Vec<String>,
    /// extra JSON object merged into the task.offer payload
    #[arg(long)]
    payload: Option<String>,
    /// timezone: local (default), UTC, or an IANA name like Asia/Hong_Kong
    #[arg(long, default_value = "local")]
    timezone: String,
    /// create the schedule disabled
    #[arg(long)]
    disabled: bool,
    #[arg(long)]
    agent: Option<String>,
}

#[derive(Args)]
struct ScheduleListArgs {
    #[arg(long)]
    room: Option<String>,
    #[arg(long)]
    agent: Option<String>,
}

#[derive(Args)]
struct ScheduleNameArgs {
    #[arg(long)]
    name: String,
    #[arg(long)]
    agent: Option<String>,
}

#[derive(Args)]
struct ScheduleTickArgs {
    /// identity that runs the tick (default scheduler@bus)
    #[arg(long)]
    agent: Option<String>,
}

fn port(cli_port: Option<u16>) -> u16 {
    std::env::var("AGENT_BUS_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .or(cli_port)
        .unwrap_or(47113)
}

fn default_agent(explicit: Option<String>) -> String {
    explicit
        .or_else(|| std::env::var("AGENT_ID").ok())
        .unwrap_or_else(|| "anonymous@bus".into())
}

/// Name the JSON shape actually handed to us, for error messages.
fn messages_kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

fn bus_home() -> std::path::PathBuf {
    if let Ok(h) = std::env::var("AGENT_BUS_HOME") {
        return h.into();
    }
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_else(|_| ".".into());
    std::path::PathBuf::from(home).join(".agent-bus")
}

fn token_path(agent: &str) -> std::path::PathBuf {
    let sanitized: String = agent
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    bus_home().join("tokens").join(format!("{sanitized}.token"))
}

/// A connected stream: TCP loopback, or a Windows named pipe.
enum Stream {
    Tcp(TcpStream),
    #[cfg(windows)]
    Pipe(tokio::net::windows::named_pipe::NamedPipeClient),
}

/// Transport preference, derived once from the CLI flags.
#[derive(Clone)]
struct Transport {
    /// `tcp` (default) or `pipe`.
    prefer: String,
    pipe: Option<String>,
}

impl Transport {
    fn pipe_path(&self) -> String {
        self.pipe
            .clone()
            .unwrap_or_else(|| r"\\.\pipe\agent-busd".to_string())
    }

    /// Try the named pipe. Compiles to a failing attempt off Windows.
    fn try_pipe(&self, attempts: &mut Vec<String>) -> Option<Stream> {
        #[cfg(windows)]
        {
            match tokio::net::windows::named_pipe::ClientOptions::new().open(self.pipe_path()) {
                Ok(cli) => return Some(Stream::Pipe(cli)),
                Err(e) => attempts.push(format!("named pipe {} ({e})", self.pipe_path())),
            }
            None
        }
        #[cfg(not(windows))]
        {
            attempts.push(format!("named pipe {}", self.pipe_path()));
            None
        }
    }

    /// Connect over the preferred transport, falling back to the other one.
    ///
    /// On failure the message names every attempt, so `os error 10061` is never
    /// the whole story.
    async fn connect(&self, p: u16) -> Result<Client> {
        let mut attempts: Vec<String> = Vec::new();

        if self.prefer == "pipe" {
            if let Some(s) = self.try_pipe(&mut attempts) {
                return Ok(Client::new(s));
            }
        }

        match TcpStream::connect(("127.0.0.1", p)).await {
            Ok(s) => return Ok(Client::new(Stream::Tcp(s))),
            Err(e) => attempts.push(format!("tcp 127.0.0.1:{p} ({e})")),
        }

        if self.prefer != "pipe" {
            if let Some(s) = self.try_pipe(&mut attempts) {
                return Ok(Client::new(s));
            }
        }

        anyhow::bail!(
            "connect to agent-busd failed; tried: {}. Is the broker running?",
            attempts.join("; ")
        )
    }
}

enum Client {
    /// TCP loopback: two independent halves.
    Tcp {
        reader: BufReader<OwnedReadHalf>,
        writer: OwnedWriteHalf,
    },
    /// Named pipe: one duplex handle shared by reads and writes.
    #[cfg(windows)]
    Pipe(tokio::net::windows::named_pipe::NamedPipeClient),
}

impl Client {
    fn new(stream: Stream) -> Client {
        match stream {
            Stream::Tcp(s) => {
                let (rd, wr) = s.into_split();
                Client::Tcp {
                    reader: BufReader::new(rd),
                    writer: wr,
                }
            }
            #[cfg(windows)]
            Stream::Pipe(s) => Client::Pipe(s),
        }
    }

    async fn send_raw(&mut self, v: &Value) -> Result<()> {
        match self {
            Client::Tcp { writer, .. } => {
                writer.write_all(encode_line(v).as_bytes()).await?;
                writer.flush().await?;
                Ok(())
            }
            #[cfg(windows)]
            Client::Pipe(h) => {
                use tokio::io::AsyncWriteExt;
                h.write_all(encode_line(v).as_bytes()).await?;
                h.flush().await?;
                Ok(())
            }
        }
    }

    async fn read_msg(&mut self) -> Result<Value> {
        let mut line = String::new();
        match self {
            Client::Tcp { reader, .. } => {
                use tokio::io::AsyncBufReadExt;
                let n = reader.read_line(&mut line).await?;
                if n == 0 {
                    return Err(anyhow!("connection closed by busd"));
                }
            }
            #[cfg(windows)]
            Client::Pipe(h) => {
                use tokio::io::AsyncReadExt;
                let mut buf = [0u8; 4096];
                // A pipe carries no framing of its own; the NDJSON terminator
                // is the newline, so read until one arrives.
                loop {
                    let n = h.read(&mut buf).await?;
                    if n == 0 {
                        return Err(anyhow!("connection closed by busd"));
                    }
                    line.push_str(&String::from_utf8_lossy(&buf[..n]));
                    if line.contains('\n') {
                        break;
                    }
                }
            }
        }
        let trimmed = line.trim_end_matches('\n');
        decode_line(trimmed.as_bytes()).map_err(|e| anyhow!("{e}"))
    }

    async fn op(&mut self, v: Value) -> Result<Value> {
        self.send_raw(&v).await?;
        let reply = self.read_msg().await?;
        if reply.get("op").and_then(|o| o.as_str()) == Some("error") {
            let code = reply.get("code").and_then(|c| c.as_str()).unwrap_or("?");
            let msg = reply.get("message").and_then(|m| m.as_str()).unwrap_or("");
            return Err(anyhow!("[{code}] {msg}"));
        }
        Ok(reply)
    }

    async fn hello(&mut self, agent: Option<&Agent>, token: Option<&str>) -> Result<Value> {
        let mut v = json!({"op": "hello", "v": 1});
        if let Some(a) = agent {
            v["agent"] = serde_json::to_value(a)?;
        }
        if let Some(t) = token {
            v["token"] = t.into();
        }
        self.send_raw(&v).await?;
        let reply = self.read_msg().await?;
        match reply.get("op").and_then(|o| o.as_str()) {
            Some("welcome") => Ok(reply),
            Some("error") => Err(anyhow!(
                "[{}] {}",
                reply.get("code").and_then(|c| c.as_str()).unwrap_or("?"),
                reply.get("message").and_then(|m| m.as_str()).unwrap_or("")
            )),
            other => Err(anyhow!("unexpected handshake reply {other:?}")),
        }
    }

    async fn bye(&mut self) {
        let _ = self.send_raw(&json!({"op": "bye"})).await;
    }

    /// Read one more line into `line`, returning how many bytes arrived.
    /// Used by the streaming commands, which own the connection for a while.
    async fn read_line_dyn(&mut self, line: &mut String) -> Result<usize> {
        match self {
            Client::Tcp { reader, .. } => {
                use tokio::io::AsyncBufReadExt;
                Ok(reader.read_line(line).await?)
            }
            #[cfg(windows)]
            Client::Pipe(h) => {
                use tokio::io::AsyncReadExt;
                // A pipe carries no framing of its own; read until the NDJSON
                // newline arrives.
                loop {
                    let mut buf = [0u8; 4096];
                    let n = h.read(&mut buf).await?;
                    if n == 0 {
                        return Ok(0);
                    }
                    line.push_str(&String::from_utf8_lossy(&buf[..n]));
                    if line.contains('\n') {
                        return Ok(line.len());
                    }
                }
            }
        }
    }
}

fn read_token(agent: &str) -> Option<String> {
    std::fs::read_to_string(token_path(agent))
        .ok()
        .map(|s| s.trim().to_string())
}

fn make_agent(
    id: &str,
    harness: &str,
    model: Option<&str>,
    caps: &[String],
    session: Option<&str>,
) -> Agent {
    Agent {
        id: id.to_string(),
        harness: harness.to_string(),
        model: model.map(String::from),
        session: session.map(String::from),
        cwd: std::env::current_dir()
            .ok()
            .map(|p| p.display().to_string()),
        caps: caps.to_vec(),
    }
}

fn fmt_env(env: &Value, format: &str) -> String {
    if format == "json" {
        return serde_json::to_string(env).unwrap_or_default();
    }
    let seq = env.get("seq").and_then(|s| s.as_i64()).unwrap_or(0);
    let kind = env.get("kind").and_then(|k| k.as_str()).unwrap_or("?");
    let from = env
        .get("from")
        .and_then(|f| f.get("id"))
        .and_then(|i| i.as_str())
        .unwrap_or("?");
    let room = env.get("room").and_then(|r| r.as_str()).unwrap_or("?");
    let text = env.get("text").and_then(|t| t.as_str()).unwrap_or("");
    format!("[{seq}] {kind} {from} ({room}): {text}")
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(cli).await {
        eprintln!("bus: {e:#}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<()> {
    let p = port(cli.port);
    let transport = Transport {
        prefer: cli.transport.clone(),
        pipe: cli.pipe.clone(),
    };
    let mut c = transport.connect(p).await?;

    match cli.cmd {
        Cmd::Register(a) => {
            let id = default_agent(a.agent);
            let agent = make_agent(
                &id,
                &a.harness,
                a.model.as_deref(),
                &a.caps,
                a.session.as_deref(),
            );
            let existing = read_token(&id);
            let mut v = json!({"op": "register", "agent": agent});
            if let Some(t) = &existing {
                v["token"] = t.as_str().into();
            }
            c.hello(None, None).await?;
            let reply = c.op(v).await?;
            c.bye().await;
            let role = reply.get("role").and_then(|r| r.as_str()).unwrap_or("?");
            if let Some(token) = reply.get("token").and_then(|t| t.as_str()) {
                let path = token_path(&id);
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                std::fs::write(&path, token)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
                }
                println!(
                    "registered {id} role={role} token stored at {}",
                    path.display()
                );
            } else {
                println!("registered {id} role={role} (token verified)");
            }
        }
        Cmd::Join(a) => {
            let id = default_agent(a.agent);
            let agent = make_agent(&id, "bus-cli", None, &[], None);
            let token = read_token(&id);
            c.hello(Some(&agent), token.as_deref()).await?;
            let reply = c.op(json!({"op": "join", "room": a.room})).await?;
            c.bye().await;
            println!(
                "joined {} seq={}",
                a.room,
                reply.get("seq").and_then(|s| s.as_i64()).unwrap_or(0)
            );
        }
        Cmd::Leave(a) => {
            let id = default_agent(a.agent);
            let agent = make_agent(&id, "bus-cli", None, &[], None);
            let token = read_token(&id);
            c.hello(Some(&agent), token.as_deref()).await?;
            c.op(json!({"op": "leave", "room": a.room})).await?;
            c.bye().await;
            println!("left {}", a.room);
        }
        Cmd::Send(a) => {
            let id = default_agent(a.agent);
            let agent = make_agent(&id, "bus-cli", None, &[], None);
            let token = read_token(&id);
            c.hello(Some(&agent), token.as_deref()).await?;
            let mut v = json!({"op": "send", "room": a.room, "kind": a.kind});
            if let Some(t) = &a.text {
                v["text"] = t.as_str().into();
            }
            if let Some(p) = &a.payload {
                v["payload"] = serde_json::from_str(p).context("payload must be valid JSON")?;
            }
            if let Some(t) = &a.thread {
                v["thread"] = t.as_str().into();
            }
            if let Some(r) = &a.reply_to {
                v["reply_to"] = r.as_str().into();
            }
            let reply = c.op(v).await?;
            c.bye().await;
            println!(
                "sent {} seq={}",
                reply.get("id").and_then(|i| i.as_str()).unwrap_or("?"),
                reply.get("seq").and_then(|s| s.as_i64()).unwrap_or(0)
            );
        }
        Cmd::SendBatch(a) => {
            let id = default_agent(a.agent);
            let agent = make_agent(&id, "bus-cli", None, &[], None);
            let token = read_token(&id);
            c.hello(Some(&agent), token.as_deref()).await?;

            // Message bodies come from a file or stdin, so a batch is not
            // limited by command-line length.
            let raw = if a.file == "-" {
                use std::io::Read as _;
                let mut s = String::new();
                std::io::stdin()
                    .read_to_string(&mut s)
                    .context("read stdin")?;
                s
            } else {
                std::fs::read_to_string(&a.file).with_context(|| format!("read {}", a.file))?
            };
            let messages: Value = serde_json::from_str(&raw)
                .context("batch file must be a JSON array of message objects")?;
            if !messages.is_array() {
                anyhow::bail!(
                    "batch file must be a JSON array, got {}",
                    messages_kind(&messages)
                );
            }
            if messages.as_array().map(|a| a.len()).unwrap_or(0) == 0 {
                anyhow::bail!("batch file has no messages");
            }

            // A frame is one NDJSON line, so it must fit the protocol's cap.
            // Split rather than let the broker reject the whole frame.
            let frame = json!({"op": "send_batch", "room": a.room, "messages": messages});
            let reply = c.op(frame).await?;
            c.bye().await;
            println!(
                "sent_batch {} messages, seqs {}..{}",
                reply.get("count").and_then(|n| n.as_u64()).unwrap_or(0),
                reply
                    .get("seqs")
                    .and_then(|s| s.as_array())
                    .and_then(|s| s.first())
                    .and_then(|n| n.as_i64())
                    .unwrap_or(0),
                reply
                    .get("seqs")
                    .and_then(|s| s.as_array())
                    .and_then(|s| s.last())
                    .and_then(|n| n.as_i64())
                    .unwrap_or(0),
            );
        }
        Cmd::Recv(a) => {
            let id = default_agent(a.agent);
            let agent = make_agent(&id, "bus-cli", None, &[], None);
            let token = read_token(&id);
            c.hello(Some(&agent), token.as_deref()).await?;
            let mut v = json!({"op": "recv", "timeout_ms": a.timeout * 1000});
            if let Some(r) = &a.room {
                v["room"] = r.as_str().into();
            }
            let reply = c.op(v).await?;
            c.bye().await;
            if reply.get("op").and_then(|o| o.as_str()) == Some("timeout") {
                eprintln!("bus: recv timed out after {}s", a.timeout);
                std::process::exit(3);
            }
            if let Some(env) = reply.get("envelope") {
                println!("{}", fmt_env(env, &a.format));
            }
        }
        Cmd::Watch(a) => {
            let id = default_agent(a.agent);
            let agent = make_agent(&id, "bus-cli", None, &[], None);
            let token = read_token(&id);
            c.hello(Some(&agent), token.as_deref()).await?;
            c.send_raw(&json!({"op": "tail", "rooms": [a.room.clone()]}))
                .await?;
            let ack = c.read_msg().await?;
            if ack.get("op").and_then(|o| o.as_str()) != Some("tail") {
                return Err(anyhow!("tail rejected: {ack}"));
            }

            let me = id.clone();
            let cap = a.limit.max(4);
            let feed = std::sync::Arc::new(std::sync::Mutex::new(watch::Feed::new(cap)));

            // Reader runs in its own task so repainting is not gated on
            // whether traffic happens to arrive.
            let mut reader = c;
            let sink = feed.clone();
            let reader_task = tokio::spawn(async move {
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line_dyn(&mut line).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                    let Ok(v) = decode_line(line.trim_end_matches('\n').as_bytes()) else {
                        continue;
                    };
                    if v.get("op").and_then(|o| o.as_str()) != Some("msg") {
                        continue;
                    }
                    let Some(env) = v.get("envelope") else {
                        continue;
                    };
                    let from = env
                        .get("from")
                        .and_then(|f| f.get("id"))
                        .and_then(|i| i.as_str())
                        .unwrap_or("")
                        .to_string();
                    let text = watch::env_line(env, from == me);
                    if let Ok(mut f) = sink.lock() {
                        if from == me {
                            f.push_main(text);
                        } else {
                            f.push_sub(text);
                        }
                    }
                }
            });

            // Alternate screen, so the user's scrollback survives exit.
            print!("\x1b[?1049h\x1b[?25l");
            let _ = std::io::stdout().flush();

            let mut ticker = tokio::time::interval(std::time::Duration::from_millis(400));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut ctrl_c = std::pin::pin!(tokio::signal::ctrl_c());
            let mut last_size: Option<(usize, usize)> = None;

            loop {
                tokio::select! {
                                    _ = ticker.tick() => {
                                        let (w, h) = watch::terminal_size();
                                        let lanes: [watch::Lane; 3] = feed.lock().map(|f| {
                                                [
                                                    watch::Lane {
                                                        title: format!("MAIN   {id}"),
                                                        rows: f.mine.clone(),
                                                    },
                                                    watch::Lane {
                                                        title: "SUBAGENTS   other bus members".into(),
                                                        rows: f.theirs.clone(),
                                                    },
                                                    watch::Lane {
                                                        title: "REPORT   reserved".into(),
                                                        rows: vec!["- not implemented yet -".into()],
                                                    },
                                                ]
                                            }).unwrap_or_else(|_| [
                                                watch::Lane { title: String::new(), rows: vec![] },
                                                watch::Lane { title: String::new(), rows: vec![] },
                                                watch::Lane { title: String::new(), rows: vec![] },
                                            ]);
                                        // Home the cursor, then redraw in place.
                                        // Overwrite rather than clear-and-redraw: every cell of every row is
                // rewritten each frame, so clearing is unnecessary and makes the pane
                // visibly flicker at the repaint rate. Clear only when the size changes,
                // so a resize cannot leave stale cells behind.
                if last_size != Some((w, h)) {
                    print!("\x1b[2J");
                    last_size = Some((w, h));
                }
                print!("\x1b[H{}", watch::render(w, h, &lanes));
                                        let _ = std::io::stdout().flush();
                                    }
                                    _ = &mut ctrl_c => break,
                                }
            }

            reader_task.abort();
            print!("\x1b[?25h\x1b[?1049l");
            let _ = std::io::stdout().flush();
        }
        Cmd::Tail(a) => {
            let id = default_agent(a.agent);
            let agent = make_agent(&id, "bus-cli", None, &[], None);
            let token = read_token(&id);
            c.hello(Some(&agent), token.as_deref()).await?;
            let mut v = json!({"op": "tail"});
            if let Some(r) = &a.rooms {
                let list: Vec<String> = r.split(',').map(|s| s.trim().to_string()).collect();
                v["rooms"] = json!(list);
            }
            if let Some(k) = &a.kinds {
                v["kinds"] = json!(k);
            }
            c.send_raw(&v).await?;
            let ack = c.read_msg().await?;
            if ack.get("op").and_then(|o| o.as_str()) != Some("tail") {
                return Err(anyhow!("tail rejected: {ack}"));
            }
            let mut line = String::new();
            loop {
                line.clear();
                let n = c.read_line_dyn(&mut line).await?;
                if n == 0 {
                    break;
                }
                if let Ok(v) = decode_line(line.trim_end_matches('\n').as_bytes()) {
                    if v.get("op").and_then(|o| o.as_str()) == Some("msg") {
                        if let Some(env) = v.get("envelope") {
                            println!("{}", fmt_env(env, &a.format));
                            let _ = std::io::stdout().flush();
                        }
                    }
                }
            }
        }
        Cmd::Who(a) => {
            c.hello(None, None).await?;
            let mut v = json!({"op": "who"});
            if let Some(r) = &a.room {
                v["room"] = r.as_str().into();
            }
            let reply = c.op(v).await?;
            c.bye().await;
            if let Some(agents) = reply.get("agents").and_then(|a| a.as_array()) {
                for a in agents {
                    // last_seen_ms is server time; the CLI clock is the
                    // same machine in practice (local broker), so the
                    // age below is approximate, not exact.
                    let seen_age_s = a
                        .get("last_seen_ms")
                        .and_then(|s| s.as_i64())
                        .map(|ms| {
                            (std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_millis() as i64)
                                .unwrap_or(ms)
                                - ms)
                                .max(0)
                                / 1000
                        })
                        .unwrap_or(0);
                    let rooms = a
                        .get("rooms")
                        .and_then(|r| r.as_array())
                        .map(|rs| {
                            rs.iter()
                                .filter_map(|r| r.as_str())
                                .collect::<Vec<_>>()
                                .join(",")
                        })
                        .unwrap_or_default();
                    let caps = a
                        .get("caps")
                        .and_then(|c| c.as_array())
                        .map(|cs| {
                            cs.iter()
                                .filter_map(|c| c.as_str())
                                .collect::<Vec<_>>()
                                .join(",")
                        })
                        .unwrap_or_default();
                    println!(
                        "{:<28} {:<12} {:<22} role={:<12} {:<8} seen={}s rooms=[{}]{}",
                        a.get("id").and_then(|i| i.as_str()).unwrap_or("?"),
                        a.get("harness").and_then(|h| h.as_str()).unwrap_or("?"),
                        a.get("model").and_then(|m| m.as_str()).unwrap_or("-"),
                        a.get("role").and_then(|r| r.as_str()).unwrap_or("?"),
                        a.get("status").and_then(|s| s.as_str()).unwrap_or("?"),
                        seen_age_s,
                        rooms,
                        if caps.is_empty() {
                            String::new()
                        } else {
                            format!(" caps=[{caps}]")
                        },
                    );
                }
                if agents.is_empty() {
                    println!("(nobody)");
                }
            }
        }
        Cmd::Heartbeat(a) => {
            let id = default_agent(a.agent);
            let agent = make_agent(&id, "bus-cli", None, &[], None);
            let token = read_token(&id);
            c.hello(Some(&agent), token.as_deref()).await?;
            let reply = c.op(json!({"op": "heartbeat"})).await?;
            c.bye().await;
            println!(
                "heartbeat {} ok={}",
                id,
                reply.get("ok").and_then(|o| o.as_bool()).unwrap_or(false)
            );
        }
        Cmd::Rooms => {
            c.hello(None, None).await?;
            let reply = c.op(json!({"op": "rooms"})).await?;
            c.bye().await;
            if let Some(rooms) = reply.get("rooms").and_then(|r| r.as_array()) {
                for r in rooms {
                    println!(
                        "{:<40} seq={}",
                        r.get("name").and_then(|n| n.as_str()).unwrap_or("?"),
                        r.get("seq").and_then(|s| s.as_i64()).unwrap_or(0)
                    );
                }
                if rooms.is_empty() {
                    println!("(no rooms)");
                }
            }
        }
        Cmd::History(a) => {
            c.hello(None, None).await?;
            let reply = c
                .op(json!({"op": "history", "room": a.room, "since": a.since, "limit": a.limit}))
                .await?;
            c.bye().await;
            if let Some(msgs) = reply.get("messages").and_then(|m| m.as_array()) {
                for m in msgs {
                    if a.full {
                        println!("{}", serde_json::to_string(m)?);
                    } else {
                        println!("{}", fmt_env(m, "text"));
                    }
                }
                if msgs.is_empty() {
                    println!("(empty from seq {})", a.since);
                }
            }
        }
        Cmd::Schedule(a) => match a.cmd {
            ScheduleCmd::Add(a) => {
                // Fail fast locally: a bad cron, timezone, or payload is a
                // CLI error, not a broker round-trip.
                proto::schedule::parse_cron(&a.cron).map_err(|e| anyhow!("bad cron: {e}"))?;
                proto::schedule::resolve_tz(&a.timezone)
                    .map_err(|e| anyhow!("bad timezone: {e}"))?;
                let mut payload = match &a.payload {
                    Some(p) => {
                        let v: Value = serde_json::from_str(p)
                            .context("payload must be a valid JSON object")?;
                        if !v.is_object() {
                            anyhow::bail!(
                                "payload must be a JSON object, got {}",
                                messages_kind(&v)
                            );
                        }
                        v
                    }
                    None => json!({}),
                };
                if payload.get("files").is_some() {
                    anyhow::bail!("pass files with --files, not inside --payload");
                }
                payload["files"] = json!(a.files);
                proto::schedule::validate_offer_payload(&payload)
                    .map_err(|e| anyhow!("bad payload: {e}"))?;

                let id = default_agent(a.agent);
                let agent = make_agent(&id, "bus-cli", None, &[], None);
                let token = read_token(&id);
                c.hello(Some(&agent), token.as_deref()).await?;
                let reply = c
                    .op(json!({
                        "op": "schedule.add",
                        "name": a.name, "room": a.room, "cron": a.cron,
                        "timezone": a.timezone, "text": a.text,
                        "payload": payload, "enabled": !a.disabled,
                    }))
                    .await?;
                c.bye().await;
                let s = reply.get("schedule").cloned().unwrap_or(json!({}));
                println!(
                    "added schedule {} -> {} cron={} tz={} enabled={}",
                    s.get("name").and_then(|n| n.as_str()).unwrap_or("?"),
                    s.get("room").and_then(|r| r.as_str()).unwrap_or("?"),
                    s.get("cron").and_then(|x| x.as_str()).unwrap_or("?"),
                    s.get("timezone").and_then(|t| t.as_str()).unwrap_or("?"),
                    s.get("enabled").and_then(|e| e.as_bool()).unwrap_or(true),
                );
            }
            ScheduleCmd::List(a) => {
                let id = default_agent(a.agent);
                let agent = make_agent(&id, "bus-cli", None, &[], None);
                let token = read_token(&id);
                c.hello(Some(&agent), token.as_deref()).await?;
                let mut v = json!({"op": "schedule.list"});
                if let Some(r) = &a.room {
                    v["room"] = r.as_str().into();
                }
                let reply = c.op(v).await?;
                c.bye().await;
                if let Some(list) = reply.get("schedules").and_then(|s| s.as_array()) {
                    for s in list {
                        println!(
                            "{:<24} {:<28} cron={:<22} tz={:<16} enabled={:<5} last_fired={}",
                            s.get("name").and_then(|n| n.as_str()).unwrap_or("?"),
                            s.get("room").and_then(|r| r.as_str()).unwrap_or("?"),
                            s.get("cron").and_then(|x| x.as_str()).unwrap_or("?"),
                            s.get("timezone").and_then(|t| t.as_str()).unwrap_or("?"),
                            s.get("enabled").and_then(|e| e.as_bool()).unwrap_or(true),
                            s.get("last_fired_ms")
                                .and_then(|l| l.as_i64())
                                .map(|m| m.to_string())
                                .unwrap_or_else(|| "-".into()),
                        );
                    }
                    if list.is_empty() {
                        println!("(no schedules)");
                    }
                }
            }
            ScheduleCmd::Remove(a) => {
                let id = default_agent(a.agent);
                let agent = make_agent(&id, "bus-cli", None, &[], None);
                let token = read_token(&id);
                c.hello(Some(&agent), token.as_deref()).await?;
                c.op(json!({"op": "schedule.remove", "name": a.name}))
                    .await?;
                c.bye().await;
                println!("removed schedule {}", a.name);
            }
            ScheduleCmd::Enable(a) => {
                let id = default_agent(a.agent);
                let agent = make_agent(&id, "bus-cli", None, &[], None);
                let token = read_token(&id);
                c.hello(Some(&agent), token.as_deref()).await?;
                c.op(json!({"op": "schedule.enable", "name": a.name}))
                    .await?;
                c.bye().await;
                println!("enabled schedule {}", a.name);
            }
            ScheduleCmd::Disable(a) => {
                let id = default_agent(a.agent);
                let agent = make_agent(&id, "bus-cli", None, &[], None);
                let token = read_token(&id);
                c.hello(Some(&agent), token.as_deref()).await?;
                c.op(json!({"op": "schedule.disable", "name": a.name}))
                    .await?;
                c.bye().await;
                println!("disabled schedule {}", a.name);
            }
            ScheduleCmd::Tick(a) => {
                // The tick runs as the scheduler identity (registered once
                // with `bus register --agent scheduler@bus --harness
                // scheduler`); $AGENT_ID or --agent overrides it.
                let id = a
                    .agent
                    .or_else(|| std::env::var("AGENT_ID").ok())
                    .unwrap_or_else(|| "scheduler@bus".into());
                let agent = make_agent(&id, "scheduler", None, &[], None);
                let token = read_token(&id);
                c.hello(Some(&agent), token.as_deref()).await?;
                let reply = c.op(json!({"op": "schedule.tick"})).await?;
                c.bye().await;
                let fired = reply
                    .get("fired")
                    .and_then(|f| f.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0);
                if fired == 0 {
                    println!("tick: nothing due");
                } else {
                    for f in reply
                        .get("fired")
                        .and_then(|f| f.as_array())
                        .into_iter()
                        .flatten()
                    {
                        println!("tick: fired {}", f.as_str().unwrap_or("?"));
                    }
                }
                if let Some(skipped) = reply.get("skipped").and_then(|s| s.as_array()) {
                    for s in skipped {
                        eprintln!(
                            "tick: skipped {} ({})",
                            s.get("name").and_then(|n| n.as_str()).unwrap_or("?"),
                            s.get("reason").and_then(|r| r.as_str()).unwrap_or("?"),
                        );
                    }
                }
            }
        },
    }
    Ok(())
}
