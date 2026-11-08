//! Scheduling proof: the scheduler identity, the schedule ops, and one
//! end-to-end tick that posts a `task.offer` exactly once.
//!
//! The tick is minute-granularity, so firing is proven with a 10-second
//! cron (`*/10 * * * * *`): the test sleeps until just past the next
//! 10 s boundary, ticks (fires the boundary just passed), then ticks
//! again immediately (the next occurrence is ~10 s away, so nothing
//! fires). That makes the once-per-tick idempotency deterministic
//! without waiting for a minute boundary.

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};

struct Server {
    port: u16,
    _db: std::path::PathBuf,
}

async fn start_server() -> Result<Server> {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let db = std::env::temp_dir().join(format!(
        "agent-bus-sched-{}-{}-{}.sqlite",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let store = busd::store::Store::open(&db)?;
    let state = Arc::new(busd::AppState::new(store));
    state.warm_members()?;
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();
    tokio::spawn(busd::serve(busd::TransportListener::Tcp(listener), state));
    Ok(Server { port, _db: db })
}

struct Client {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
}

impl Client {
    async fn connect(port: u16) -> Result<Client> {
        let (rd, wr) = TcpStream::connect(("127.0.0.1", port)).await?.into_split();
        Ok(Client {
            reader: BufReader::new(rd),
            writer: wr,
        })
    }

    async fn send(&mut self, v: &Value) -> Result<()> {
        let line = proto::encode_line(v);
        self.writer.write_all(line.as_bytes()).await?;
        self.writer.flush().await?;
        Ok(())
    }

    async fn read(&mut self) -> Result<Value> {
        let mut line = String::new();
        let n = self.reader.read_line(&mut line).await?;
        if n == 0 {
            return Err(anyhow!("eof"));
        }
        proto::decode_line(line.trim_end_matches('\n').as_bytes()).map_err(|e| anyhow!("{e}"))
    }

    async fn op(&mut self, v: Value) -> Result<Value> {
        self.send(&v).await?;
        let r = self.read().await?;
        if r.get("op").and_then(|o| o.as_str()) == Some("error") {
            return Err(anyhow!(
                "op error: {} {}",
                r.get("code").and_then(|c| c.as_str()).unwrap_or("?"),
                r.get("message").and_then(|m| m.as_str()).unwrap_or("")
            ));
        }
        Ok(r)
    }

    async fn send_and_read_error(&mut self, v: Value) -> Result<Value> {
        self.send(&v).await?;
        let r = self.read().await?;
        if r.get("op").and_then(|o| o.as_str()) != Some("error") {
            return Err(anyhow!("expected error, got {r}"));
        }
        Ok(r)
    }

    async fn register(&mut self, id: &str, harness: &str, model: Option<&str>) -> Result<Value> {
        let mut agent = json!({"id": id, "harness": harness, "caps": []});
        if let Some(m) = model {
            agent["model"] = m.into();
        }
        self.send(&json!({"op": "hello", "v": 1})).await?;
        let welcome = self.read().await?;
        assert_eq!(welcome["op"], "welcome");
        self.op(json!({"op": "register", "agent": agent})).await
    }

    async fn hello_as(&mut self, id: &str, harness: &str, token: &str) -> Result<()> {
        self.send(&json!({"op": "hello", "v": 1, "agent": {"id": id, "harness": harness}, "token": token})).await?;
        let w = self.read().await?;
        assert_eq!(w["op"], "welcome");
        Ok(())
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// Sleep until just past the next 10 s wall-clock boundary (+300 ms), so
/// a `*/10 * * * * *` cron has an occurrence due within the tick window.
async fn sleep_past_next_10s_boundary() {
    let now = now_ms();
    let next = ((now / 10_000) + 1) * 10_000 + 300;
    let wait = (next - now).max(0) as u64;
    tokio::time::sleep(Duration::from_millis(wait)).await;
}

#[tokio::test]
async fn scheduler_role_and_tick_fires_once() -> Result<()> {
    let server = start_server().await?;

    // -- the scheduler identity: harness "scheduler", no model -> Conversation
    let mut sched = Client::connect(server.port).await?;
    let reg = sched.register("scheduler@bus", "scheduler", None).await?;
    assert_eq!(reg["op"], "registered");
    assert_eq!(reg["role"], "conversation");
    let sched_token = reg["token"].as_str().expect("token issued").to_string();

    // -- a protocol-role agent may not run the tick (role gate)
    let mut claude = Client::connect(server.port).await?;
    let reg = claude.register("claude@demo", "claude-code", None).await?;
    assert_eq!(reg["role"], "protocol");
    let claude_token = reg["token"].as_str().unwrap().to_string();
    let mut claude = Client::connect(server.port).await?;
    claude
        .hello_as("claude@demo", "claude-code", &claude_token)
        .await?;
    let err = claude
        .send_and_read_error(json!({"op": "schedule.tick"}))
        .await?;
    assert_eq!(err["code"], "role");

    // -- add a schedule as the scheduler (10 s cron, UTC)
    let mut sched = Client::connect(server.port).await?;
    sched
        .hello_as("scheduler@bus", "scheduler", &sched_token)
        .await?;
    let added = sched
        .op(json!({
            "op": "schedule.add", "name": "probe", "room": "#sched",
            "cron": "*/10 * * * * *", "timezone": "UTC",
            "text": "probe offer", "payload": {"files": []},
        }))
        .await?;
    assert_eq!(added["op"], "schedule.added");
    assert_eq!(added["schedule"]["name"], "probe");

    // -- tick before the next boundary: nothing due yet (or the boundary
    // just passed — either way the reply shape is the contract)
    let tick = sched.op(json!({"op": "schedule.tick"})).await?;
    assert_eq!(tick["op"], "schedule.tick");
    let fired_first = tick["fired"].as_array().map(|a| a.len()).unwrap_or(0);
    assert!(
        fired_first <= 1,
        "at most one fire per tick, got {fired_first}"
    );

    // -- sleep past the next 10 s boundary, then tick: the boundary just
    // passed is due, so exactly one offer fires.
    sleep_past_next_10s_boundary().await;
    let tick = sched.op(json!({"op": "schedule.tick"})).await?;
    assert_eq!(tick["fired"].as_array().map(|a| a.len()).unwrap_or(0), 1);
    assert_eq!(tick["fired"][0], "probe");

    // -- tick again immediately: the next occurrence is ~10 s away, so
    // nothing fires (once per occurrence, even if tick runs twice).
    let tick = sched.op(json!({"op": "schedule.tick"})).await?;
    assert_eq!(tick["fired"].as_array().map(|a| a.len()).unwrap_or(0), 0);

    // -- the offer is durable in history, from the scheduler
    let mut hist = Client::connect(server.port).await?;
    hist.send(&json!({"op": "hello", "v": 1})).await?;
    let _ = hist.read().await;
    let h = hist.op(json!({"op": "history", "room": "#sched"})).await?;
    let msgs = h["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 1 + fired_first);
    assert_eq!(msgs.last().unwrap()["kind"], "task.offer");
    assert_eq!(msgs.last().unwrap()["from"]["id"], "scheduler@bus");
    assert_eq!(msgs.last().unwrap()["text"], "probe offer");

    // -- disable: the next boundary passes without firing
    sched
        .op(json!({"op": "schedule.disable", "name": "probe"}))
        .await?;
    sleep_past_next_10s_boundary().await;
    let tick = sched.op(json!({"op": "schedule.tick"})).await?;
    assert_eq!(tick["fired"].as_array().map(|a| a.len()).unwrap_or(0), 0);

    // -- remove: the list is empty
    sched
        .op(json!({"op": "schedule.remove", "name": "probe"}))
        .await?;
    let list = sched.op(json!({"op": "schedule.list"})).await?;
    assert_eq!(list["count"], 0);
    Ok(())
}

#[tokio::test]
async fn schedule_crud_validation() -> Result<()> {
    let server = start_server().await?;

    let mut sched = Client::connect(server.port).await?;
    let reg = sched.register("scheduler@bus", "scheduler", None).await?;
    let token = reg["token"].as_str().unwrap().to_string();
    let mut sched = Client::connect(server.port).await?;
    sched.hello_as("scheduler@bus", "scheduler", &token).await?;

    // Bad cron / bad timezone / bad payload are typed payload errors.
    let err = sched
        .send_and_read_error(json!({
            "op": "schedule.add", "name": "bad", "room": "#sched",
            "cron": "not a cron", "payload": {"files": []},
        }))
        .await?;
    assert_eq!(err["code"], "payload");
    let err = sched
        .send_and_read_error(json!({
            "op": "schedule.add", "name": "bad", "room": "#sched",
            "cron": "0 8 * * *", "timezone": "Mars/Olympus",
            "payload": {"files": []},
        }))
        .await?;
    assert_eq!(err["code"], "payload");
    let err = sched
        .send_and_read_error(json!({
            "op": "schedule.add", "name": "bad", "room": "#sched",
            "cron": "0 8 * * *", "payload": {},
        }))
        .await?;
    assert_eq!(err["code"], "payload");

    // Good add, then a duplicate name is rejected.
    sched
        .op(json!({
            "op": "schedule.add", "name": "one", "room": "#sched",
            "cron": "0 8 * * *", "timezone": "Asia/Hong_Kong",
            "text": "digest", "payload": {"files": []},
        }))
        .await?;
    let err = sched
        .send_and_read_error(json!({
            "op": "schedule.add", "name": "one", "room": "#sched",
            "cron": "0 9 * * *", "payload": {"files": []},
        }))
        .await?;
    assert_eq!(err["code"], "payload");

    // List shows the one schedule, with its timezone.
    let list = sched.op(json!({"op": "schedule.list"})).await?;
    assert_eq!(list["count"], 1);
    assert_eq!(list["schedules"][0]["name"], "one");
    assert_eq!(list["schedules"][0]["timezone"], "Asia/Hong_Kong");
    assert_eq!(list["schedules"][0]["enabled"], true);
    let filtered = sched
        .op(json!({"op": "schedule.list", "room": "#other"}))
        .await?;
    assert_eq!(filtered["count"], 0);

    // Unknown names are typed errors, not silent no-ops.
    let err = sched
        .send_and_read_error(json!({"op": "schedule.remove", "name": "nope"}))
        .await?;
    assert_eq!(err["code"], "payload");
    let err = sched
        .send_and_read_error(json!({"op": "schedule.enable", "name": "nope"}))
        .await?;
    assert_eq!(err["code"], "payload");

    // Anonymous (unregistered) callers may not manage schedules.
    let mut anon = Client::connect(server.port).await?;
    anon.send(&json!({"op": "hello", "v": 1})).await?;
    let _ = anon.read().await;
    let err = anon
        .send_and_read_error(json!({"op": "schedule.list"}))
        .await?;
    assert_eq!(err["code"], "proto");
    Ok(())
}
