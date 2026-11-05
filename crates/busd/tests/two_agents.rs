//! Phase 1 proof: two agents converse in one room through the broker.
//! Mirrors the t+0..t+8 flow in docs/protocol.md.

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
    // The clock alone is not enough: `SystemTime::now()` quantises to ~100ns on
    // Windows, so two tests racing in parallel can land in the same tick and
    // share one database file. The counter makes the name unique per call.
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let db = std::env::temp_dir().join(format!(
        "agent-bus-test-{}-{}-{}.sqlite",
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

fn agent_hello(id: &str, model: Option<&str>, token: Option<&str>) -> Value {
    let mut v = json!({"op": "hello", "v": 1, "agent": {"id": id, "harness": "test"}});
    if let Some(m) = model {
        v["agent"]["model"] = m.into();
    }
    if let Some(t) = token {
        v["token"] = t.into();
    }
    v
}

#[tokio::test]
async fn two_agents_one_room_full_flow() -> Result<()> {
    let server = start_server().await?;

    // -- codex registers (peer-class model) and joins
    let mut codex = Client::connect(server.port).await?;
    let reg = codex
        .register("codex@demo", "codex-cli", Some("gpt-5-codex"))
        .await?;
    assert_eq!(reg["op"], "registered");
    assert_eq!(reg["role"], "peer");
    let codex_token = reg["token"].as_str().expect("token issued").to_string();

    let mut codex = Client::connect(server.port).await?;
    codex
        .send(&agent_hello(
            "codex@demo",
            Some("gpt-5-codex"),
            Some(&codex_token),
        ))
        .await?;
    let w = codex.read().await?;
    assert_eq!(w["op"], "welcome");
    codex
        .op(json!({"op": "join", "room": "#task:fix-auth"}))
        .await?;

    // -- claude registers (unknown model -> protocol role) and joins
    let mut claude = Client::connect(server.port).await?;
    let reg = claude.register("claude@demo", "claude-code", None).await?;
    assert_eq!(reg["role"], "protocol");
    let claude_token = reg["token"].as_str().unwrap().to_string();

    let mut claude = Client::connect(server.port).await?;
    claude
        .send(&agent_hello("claude@demo", None, Some(&claude_token)))
        .await?;
    let _ = claude.read().await;
    claude
        .op(json!({"op": "join", "room": "#task:fix-auth"}))
        .await?;

    // -- t+2: codex offers the task (durable kind)
    let sent = codex
        .op(json!({
            "op": "send", "room": "#task:fix-auth", "kind": "task.offer",
            "text": "auth redirect broken on POST /login",
            "payload": {"files": ["src/auth/login.ts"], "claim_ttl_s": 300}
        }))
        .await?;
    assert_eq!(sent["op"], "sent");
    let seq1 = sent["seq"].as_i64().unwrap();
    let offer_id = sent["id"].as_str().unwrap().to_string();

    // -- claude receives it via blocking recv (the push substitute)
    let got = claude
        .op(json!({"op": "recv", "room": "#task:fix-auth", "timeout_ms": 3000}))
        .await?;
    assert_eq!(got["op"], "msg");
    assert_eq!(got["envelope"]["kind"], "task.offer");
    assert_eq!(got["envelope"]["seq"].as_i64().unwrap(), seq1);
    assert_eq!(got["envelope"]["from"]["id"], "codex@demo");
    assert_eq!(got["envelope"]["trust"], "peer");

    // -- protocol-role agent may not claim (typed, public deny)
    let err = claude
        .op(
            json!({"op": "send", "room": "#task:fix-auth", "kind": "task.claim",
                    "payload": {"files": ["src/auth/login.ts"]}}),
        )
        .await;
    assert!(err.is_err(), "protocol role must not claim");

    // -- claude (protocol) CAN ask a question with reply_to
    let sent = claude
        .op(json!({
            "op": "send", "room": "#task:fix-auth", "kind": "question",
            "text": "want me to take tests/?", "reply_to": offer_id
        }))
        .await?;
    let seq2 = sent["seq"].as_i64().unwrap();
    assert!(seq2 > seq1, "seq must be monotonic");

    // -- codex receives the question
    let got = codex
        .op(json!({"op": "recv", "room": "#task:fix-auth", "timeout_ms": 3000}))
        .await?;
    assert_eq!(got["envelope"]["kind"], "question");
    assert_eq!(got["envelope"]["reply_to"].as_str().unwrap(), offer_id);

    // -- answer requires reply_to: missing -> typed payload error
    let bad = codex
        .send_and_read_error(
            json!({"op": "send", "room": "#task:fix-auth", "kind": "answer", "text": "yes"}),
        )
        .await?;
    assert_eq!(bad["code"], "payload");

    // -- good answer
    codex
        .op(
            json!({"op": "send", "room": "#task:fix-auth", "kind": "answer",
                    "text": "yes, tests/ only", "reply_to": offer_id}),
        )
        .await?;

    // -- history: exact ordered thread
    let mut hist = Client::connect(server.port).await?;
    hist.send(&json!({"op": "hello", "v": 1})).await?;
    let _ = hist.read().await;
    let h = hist
        .op(json!({"op": "history", "room": "#task:fix-auth"}))
        .await?;
    let msgs = h["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 3);
    let seqs: Vec<i64> = msgs.iter().map(|m| m["seq"].as_i64().unwrap()).collect();
    assert_eq!(
        seqs,
        vec![1, 2, 3],
        "server-assigned seq must be 1..n with no gaps"
    );
    assert_eq!(msgs[0]["kind"], "task.offer");
    assert_eq!(msgs[1]["kind"], "question");
    assert_eq!(msgs[2]["kind"], "answer");

    // -- who: both present
    let who = hist
        .op(json!({"op": "who", "room": "#task:fix-auth"}))
        .await?;
    assert_eq!(who["count"].as_i64().unwrap(), 2);

    // -- who with no room: the unscoped listing. This binds no parameter to the
    // query, which a naive implementation gets wrong ("Wrong number of
    // parameters passed to query. Got 1, needed 0").
    let all = hist.op(json!({"op": "who"})).await?;
    let n = all["agents"].as_array().map(|a| a.len()).unwrap_or(0);
    assert!(n >= 2, "unscoped who listed {n} agents, expected at least the two here");
    assert!(
        all["agents"]
            .as_array()
            .map(|a| a.iter().all(|x| x.get("role").is_some()))
            .unwrap_or(false),
        "unscoped who must return a role per agent"
    );

    // -- rooms
    let rooms = hist.op(json!({"op": "rooms"})).await?;
    assert_eq!(rooms["rooms"][0]["name"], "#task:fix-auth");

    // -- recv with zero timeout when empty -> timeout op (no block)
    let t = hist.op(json!({"op": "recv", "timeout_ms": 10})).await?;
    assert_eq!(t["op"], "timeout");

    // -- leave: claude stops receiving
    claude
        .op(json!({"op": "leave", "room": "#task:fix-auth"}))
        .await?;
    codex
        .op(json!({"op": "send", "room": "#task:fix-auth", "kind": "chat", "text": "ping"}))
        .await?;
    let t = claude
        .op(json!({"op": "recv", "room": "#task:fix-auth", "timeout_ms": 100}))
        .await?;
    assert_eq!(t["op"], "timeout", "left agent must not receive");

    Ok(())
}

impl Client {
    /// Send an op expected to fail; return the error envelope.
    async fn send_and_read_error(&mut self, v: Value) -> Result<Value> {
        self.send(&v).await?;
        let r = self.read().await?;
        if r.get("op").and_then(|o| o.as_str()) != Some("error") {
            return Err(anyhow!("expected error, got {r}"));
        }
        Ok(r)
    }
}

#[tokio::test]
async fn handshake_and_auth_rules() -> Result<()> {
    let server = start_server().await?;

    // version mismatch is fatal
    let mut c = Client::connect(server.port).await?;
    c.send(&json!({"op": "hello", "v": 99})).await?;
    let bye = c.read().await?;
    assert_eq!(bye["op"], "bye");
    assert_eq!(bye["code"], "proto");

    // register issues token once
    let mut c = Client::connect(server.port).await?;
    c.send(&json!({"op": "hello", "v": 1})).await?;
    let _ = c.read().await;
    let reg = c.op(json!({"op": "register", "agent": {"id": "a@x", "harness": "t", "model": "llama-3-70b"}})).await?;
    let token = reg["token"].as_str().unwrap().to_string();
    assert_eq!(reg["role"], "peer");

    // re-register without token -> auth error
    let mut c2 = Client::connect(server.port).await?;
    c2.send(&json!({"op": "hello", "v": 1})).await?;
    let _ = c2.read().await;
    let err = c2
        .send_and_read_error(json!({"op": "register", "agent": {"id": "a@x", "harness": "t", "model": "llama-3-70b"}}))
        .await?;
    assert_eq!(err["code"], "auth");

    // hello with wrong token -> auth error, connection refused
    let mut c3 = Client::connect(server.port).await?;
    c3.send(&agent_hello("a@x", Some("llama-3-70b"), Some("wrong")))
        .await?;
    let err = c3.read().await?;
    assert_eq!(err["op"], "error");
    assert_eq!(err["code"], "auth");

    // hello with right token works, welcome carries cursor after join
    let mut c4 = Client::connect(server.port).await?;
    c4.send(&agent_hello("a@x", Some("llama-3-70b"), Some(&token)))
        .await?;
    let w = c4.read().await?;
    assert_eq!(w["op"], "welcome");
    c4.op(json!({"op": "join", "room": "#repo:x"})).await?;
    let mut c5 = Client::connect(server.port).await?;
    c5.send(&agent_hello("a@x", Some("llama-3-70b"), Some(&token)))
        .await?;
    let w = c5.read().await?;
    let cursor = w["cursor"].as_object().unwrap();
    assert!(
        cursor.contains_key("#repo:x"),
        "welcome cursor must include joined rooms"
    );
    assert_eq!(cursor["#repo:x"].as_i64().unwrap(), 0);
    Ok(())
}

#[tokio::test]
async fn rate_limit_trips_at_60_per_s() -> Result<()> {
    let server = start_server().await?;
    let mut c = Client::connect(server.port).await?;
    let reg = c.register("flood@demo", "script", None).await?;
    assert_eq!(reg["role"], "transport");
    let token = reg["token"].as_str().unwrap().to_string();

    let mut c = Client::connect(server.port).await?;
    c.send(&agent_hello("flood@demo", None, Some(&token)))
        .await?;
    let _ = c.read().await;
    c.op(json!({"op": "join", "room": "#flood"})).await?;

    let mut denied = 0;
    for i in 0..80 {
        c.send(&json!({"op": "send", "room": "#flood", "kind": "chat", "text": i}))
            .await?;
        let r = c.read().await?;
        if r["op"] == "error" && r["code"] == "rate" {
            denied += 1;
        }
    }
    assert!(denied >= 15, "expected rate denials, got {denied}");
    Ok(())
}

#[tokio::test]
async fn tail_streams_live_traffic() -> Result<()> {
    let server = start_server().await?;
    let mut c = Client::connect(server.port).await?;
    let reg = c
        .register("sender@demo", "test", Some("llama-3-70b"))
        .await?;
    let token = reg["token"].as_str().unwrap().to_string();

    // a tail observer (anonymous is fine — tail only reads)
    let mut tail = Client::connect(server.port).await?;
    tail.send(&json!({"op": "hello", "v": 1})).await?;
    let _ = tail.read().await;
    tail.send(&json!({"op": "tail", "rooms": ["#live"], "kinds": ["chat"]}))
        .await?;
    let ack = tail.read().await?;
    assert_eq!(ack["op"], "tail");

    let mut c = Client::connect(server.port).await?;
    c.send(&agent_hello(
        "sender@demo",
        Some("llama-3-70b"),
        Some(&token),
    ))
    .await?;
    let _ = c.read().await;
    c.send(&json!({"op": "send", "room": "#live", "kind": "chat", "text": "hello tail"}))
        .await?;
    let sent = c.read().await?;
    assert_eq!(sent["op"], "sent");

    let msg = tail.read().await?;
    assert_eq!(msg["op"], "msg");
    assert_eq!(msg["envelope"]["text"], "hello tail");
    Ok(())
}

#[tokio::test]
async fn human_surface_is_stamped_human_and_may_claim() -> Result<()> {
    let server = start_server().await?;

    // A person is not model-floored: `@human`/telegram registers as peer.
    let mut c = Client::connect(server.port).await?;
    let reg = c.register("@human", "telegram", None).await?;
    assert_eq!(reg["role"], "peer", "human surface must not be model-floored");
    let token = reg["token"].as_str().unwrap().to_string();

    let mut c = Client::connect(server.port).await?;
    c.send(&agent_hello("@human", None, Some(&token))).await?;
    let _ = c.read().await;
    c.op(json!({"op": "join", "room": "#human"})).await?;

    // Claims are allowed for a human surface.
    let claim = c
        .op(json!({"op": "send", "room": "#human", "kind": "task.claim",
                   "text": "taking login.ts",
                   "payload": {"files": ["src/auth/login.ts"], "claim_ttl_s": 300}}))
        .await?;
    assert_eq!(claim["op"], "sent");

    // Policy is human-only, and a human surface may send it.
    let policy = c
        .op(json!({"op": "send", "room": "#human", "kind": "policy",
                   "text": "ship it",
                   "payload": {"deny": ["src/db/**"]}}))
        .await?;
    assert_eq!(policy["op"], "sent");

    // The broker, not the sender, stamped trust=human on both.
    let mut hist = Client::connect(server.port).await?;
    hist.send(&json!({"op": "hello", "v": 1})).await?;
    let _ = hist.read().await;
    let h = hist.op(json!({"op": "history", "room": "#human"})).await?;
    let msgs = h["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0]["kind"], "task.claim");
    assert_eq!(msgs[0]["trust"], "human");
    assert_eq!(msgs[1]["kind"], "policy");
    assert_eq!(msgs[1]["trust"], "human");
    Ok(())
}

#[tokio::test]
async fn ordering_many_senders_no_gaps() -> Result<()> {
    let server = start_server().await?;
    let room = "#storm";

    // register + join N senders
    let mut tokens = vec![];
    for i in 0..8 {
        let mut c = Client::connect(server.port).await?;
        let id = format!("w{i}@storm");
        let reg = c.register(&id, "test", Some("llama-3-70b")).await?;
        tokens.push((id, reg["token"].as_str().unwrap().to_string()));
    }
    for (id, token) in &tokens {
        let mut c = Client::connect(server.port).await?;
        c.send(&agent_hello(id, Some("llama-3-70b"), Some(token)))
            .await?;
        let _ = c.read().await;
        c.op(json!({"op": "join", "room": room})).await?;
    }

    // 8 writers x 25 msgs each (under the 60/s rate limit), concurrently
    let mut handles = vec![];
    for (i, (id, token)) in tokens.iter().enumerate() {
        let port = server.port;
        let id = id.clone();
        let token = token.clone();
        handles.push(tokio::spawn(async move {
            let mut c = Client::connect(port).await?;
            c.send(&agent_hello(&id, Some("llama-3-70b"), Some(&token)))
                .await?;
            let _ = c.read().await;
            for j in 0..25 {
                let v = json!({"op": "send", "room": "#storm", "kind": "status",
                               "text": format!("{i}-{j}"), "payload": {"ctx_used": 0.1}});
                c.send(&v).await?;
                let r = c.read().await?;
                if r["op"] != "sent" {
                    return Err(anyhow!("send failed: {r}"));
                }
            }
            Ok::<(), anyhow::Error>(())
        }));
    }
    for h in handles {
        h.await??;
    }

    // verify: seq 1..=200, strictly monotonic, zero gaps
    let mut c = Client::connect(server.port).await?;
    c.send(&json!({"op": "hello", "v": 1})).await?;
    let _ = c.read().await;
    let h = c
        .op(json!({"op": "history", "room": room, "limit": 1000}))
        .await?;
    let msgs = h["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 200);
    for (i, m) in msgs.iter().enumerate() {
        assert_eq!(
            m["seq"].as_i64().unwrap(),
            (i + 1) as i64,
            "gap at {}",
            i + 1
        );
    }
    Ok(())
}
