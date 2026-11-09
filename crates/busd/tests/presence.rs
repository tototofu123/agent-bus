//! Presence (issue #9): `who` marks live vs stale agents, and a
//! registered agent can heartbeat without sending a message.

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::sync::Arc;
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
        "agent-bus-presence-test-{}-{}-{}.sqlite",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
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

    async fn register(&mut self, id: &str, harness: &str, model: Option<&str>) -> Result<Value> {
        let mut agent = json!({"id": id, "harness": harness, "caps": ["edit"]});
        if let Some(m) = model {
            agent["model"] = m.into();
        }
        self.send(&json!({"op": "hello", "v": 1})).await?;
        let welcome = self.read().await?;
        assert_eq!(welcome["op"], "welcome");
        self.op(json!({"op": "register", "agent": agent})).await
    }
}

fn agent_hello(id: &str, token: Option<&str>) -> Value {
    let mut v = json!({"op": "hello", "v": 1, "agent": {"id": id, "harness": "test"}});
    if let Some(t) = token {
        v["token"] = t.into();
    }
    v
}

#[tokio::test]
async fn who_shows_status_and_heartbeat_keeps_online() -> Result<()> {
    let server = start_server().await?;

    // -- one agent registers and joins a room
    let mut a = Client::connect(server.port).await?;
    let reg = a.register("codex@demo", "codex-cli", Some("gpt-5-codex")).await?;
    assert_eq!(reg["op"], "registered");
    let token = reg["token"].as_str().unwrap().to_string();

    let mut a = Client::connect(server.port).await?;
    a.send(&agent_hello("codex@demo", Some(&token))).await?;
    assert_eq!(a.read().await?["op"], "welcome");
    a.op(json!({"op": "join", "room": "#task:demo"})).await?;

    // -- who lists it as online, with caps and rooms
    let who = a.op(json!({"op": "who"})).await?;
    assert_eq!(who["op"], "who");
    assert_eq!(who["count"], 1);
    let row = &who["agents"][0];
    assert_eq!(row["id"], "codex@demo");
    assert_eq!(row["status"], "online");
    assert_eq!(row["model"], "gpt-5-codex");
    assert_eq!(row["caps"], json!(["edit"]));
    assert_eq!(row["rooms"], json!(["#task:demo"]));

    // -- heartbeat without a token'd identity fails; with one it works
    let mut anon = Client::connect(server.port).await?;
    anon.send(&json!({"op": "hello", "v": 1})).await?;
    assert_eq!(anon.read().await?["op"], "welcome");
    let err = anon.op(json!({"op": "heartbeat"})).await;
    assert!(err.is_err(), "anonymous heartbeat must be rejected");

    let hb = a.op(json!({"op": "heartbeat"})).await?;
    assert_eq!(hb["op"], "heartbeat");
    assert_eq!(hb["ok"], true);

    // -- still online after the heartbeat
    let who = a.op(json!({"op": "who"})).await?;
    assert_eq!(who["agents"][0]["status"], "online");
    Ok(())
}
