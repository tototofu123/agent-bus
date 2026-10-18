// Per-connection protocol state machine: handshake, then one op per line.
use crate::router::Router;
use crate::store::{broker_envelope, Store};
use anyhow::{anyhow, Result};
use proto::{
    decode_line, encode_line, hash_token, new_token, Agent, Envelope, Kind, Role, Trust,
    MAX_LINE_BYTES, OP_BYE, OP_ERROR, OP_HELLO, OP_SENT, OP_WELCOME, V,
};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::tcp::OwnedWriteHalf;
use tokio::net::TcpStream;
pub struct AppState {
    pub store: Store,
    pub router: Router,
    pub started: std::time::SystemTime,
}
impl AppState {
    pub fn new(store: Store) -> AppState {
        AppState {
            store,
            router: Router::new(),
            started: std::time::SystemTime::now(),
        }
    }
    // Warm the membership mirror from the DB (call once at boot).
    pub fn warm_members(&self) -> Result<()> {
        let rooms = self.store.rooms()?;
        for r in rooms {
            if let Some(name) = r["name"].as_str() {
                let members = self.store.members_of(name)?;
                self.router.load_members(name, members);
            }
        }
        Ok(())
    }
}
fn valid_room(name: &str) -> bool {
    name.starts_with('#') && name.len() <= 64 && !name.contains(char::is_whitespace)
}
// Builds an error frame as a `Value`. `write` does the line encoding, so this
// must not pre-encode — returning a String here would double-encode.
fn err_value(code: &str, msg: impl AsRef<str>) -> Value {
    json!({"op": OP_ERROR, "code": code, "message": msg.as_ref()})
}
// Read one NDJSON line with a hard cap. Frame violations are fatal.
async fn read_line_capped<R: AsyncBufRead + Unpin>(r: &mut R, buf: &mut Vec<u8>) -> Result<usize> {
    buf.clear();
    loop {
        let available = match r.fill_buf().await {
            Ok(a) => a,
            Err(e) => return Err(e.into()),
        };
        if available.is_empty() {
            return Ok(0); // EOF
        }
        if let Some(nl) = available.iter().position(|&b| b == b'\n') {
            let take = nl + 1;
            buf.extend_from_slice(&available[..take]);
            r.consume(take);
            return Ok(take);
        }
        if buf.len() + available.len() > MAX_LINE_BYTES {
            return Err(anyhow!("line exceeds {} bytes", MAX_LINE_BYTES));
        }
        let l = available.len();
        buf.extend_from_slice(available);
        r.consume(l);
    }
}
pub async fn handle_conn(stream: TcpStream, state: Arc<AppState>) -> Result<()> {
    stream.set_nodelay(true).ok();
    let (rd, wr) = stream.into_split();
    let mut reader = BufReader::with_capacity(64 * 1024, rd);
    let mut writer = wr;
    let mut line = Vec::with_capacity(4096); // -- handshake ----------------------------------------------------------
    let n = read_line_capped(&mut reader, &mut line).await?;
    if n == 0 {
        return Ok(());
    }
    let hello = decode_line(&line).map_err(|e| anyhow!("bad hello: {e}"))?;
    if hello.get("v").and_then(|v| v.as_u64()) != Some(V as u64) {
        write(
            &mut writer,
            &json!({"op": OP_BYE, "code": "proto", "got": hello.get("v")}),
        )
        .await?;
        return Err(anyhow!("version mismatch"));
    }
    if hello.get("op").and_then(|o| o.as_str()) != Some(OP_HELLO) {
        write(&mut writer, &json!({"op": OP_BYE, "code": "proto"})).await?;
        return Err(anyhow!("first op must be hello"));
    }
    let agent: Option<Agent> = hello
        .get("agent")
        .and_then(|a| serde_json::from_value(a.clone()).ok());
    if let Some(agent) = &agent {
        if let Ok(Some(_)) = state.store.token_hash(&agent.id) {
            let ok = hello
                .get("token")
                .and_then(|t| t.as_str())
                .is_some_and(|t| state.store.token_matches(&agent.id, t));
            if !ok {
                write(&mut writer, &err_value("auth", "token required/mismatched")).await?;
                return Err(anyhow!("auth"));
            }
        }
    }
    let mut me: Option<Agent> = None;
    if let Some(agent) = agent {
        state.store.touch_agent(&agent.id).ok();
        me = Some(agent);
    }
    let cursor: Value = match &me {
        Some(a) => state
            .store
            .rooms_of(&a.id)?
            .into_iter()
            .map(|(room, seq)| (room, json!(seq)))
            .collect::<serde_json::Map<String, Value>>()
            .into(),
        None => json!({}),
    };
    write(        &mut writer,        &json!({            "op": OP_WELCOME,            "server": "agent-busd",            "v": V,            "since_ms": state                .started                .duration_since(std::time::UNIX_EPOCH)                .map(|d| d.as_millis() as i64)                .unwrap_or(0),            "cursor": cursor,            "limits": {"max_line": MAX_LINE_BYTES, "rate_per_s": 60},        }),    )    .await?; // -- op loop --------------------------------------------------------------
    let mut tail_rx: Option<tokio::sync::mpsc::UnboundedReceiver<Envelope>> = None;
    let mut tail_id: Option<u32> = None;
    loop {
        tokio::select! {            n = read_line_capped(&mut reader, &mut line) => {                let n = n?;                if n == 0 { break; }                let msg = match decode_line(&line) {                    Ok(v) => v,                    Err(e) => {                        write(&mut writer, &err_value("frame", e.to_string())).await?;                        continue;                    }                };                let op = msg.get("op").and_then(|o| o.as_str()).unwrap_or_default().to_string();                match op.as_str() {                    OP_BYE => {                        write(&mut writer, &json!({"op": OP_BYE})).await?;                        break;                    }                    "register" => {                        op_register(&state, &msg, &mut writer).await?;                    }                    "join" => {                        op_join(&state, &me, &msg, &mut writer).await?;                    }                    "leave" => {                        op_leave(&state, &me, &msg, &mut writer).await?;                    }                    "send" => {                        op_send(&state, &me, &msg, &mut writer).await?;                    }                    "recv" => {                        op_recv(&state, &me, &msg, &mut writer).await?;                    }                    "history" => {                        op_history(&state, &msg, &mut writer).await?;                    }                    "who" => {                        op_who(&state, &msg, &mut writer).await?;                    }                    "rooms" => {                        op_rooms(&state, &mut writer).await?;                    }                    "tail" => {                        let rooms: Option<std::collections::HashSet<String>> = msg                            .get("rooms")                            .and_then(|r| r.as_array())                            .map(|a| a.iter().filter_map(|s| s.as_str().map(String::from)).collect());                        let kinds: Option<std::collections::HashSet<String>> = msg                            .get("kinds")                            .and_then(|k| k.as_array())                            .map(|a| a.iter().filter_map(|s| s.as_str().map(String::from)).collect());                        let (id, rx) = state.router.subscribe_tail(rooms, kinds);                        tail_id = Some(id);                        tail_rx = Some(rx);                        write(&mut writer, &json!({"op": "tail", "ok": true})).await?;                    }                    other => {                        write(&mut writer, &err_value("op", format!("unknown op {other:?}"))).await?;                    }                }            }            ev = async { match tail_rx.as_mut() { Some(rx) => rx.recv().await, None => std::future::pending().await } } => {                if let Some(env) = ev {                    write(&mut writer, &json!({"op": "msg", "envelope": env})).await?;                }            }        }
    }
    if let Some(id) = tail_id {
        state.router.unsubscribe_tail(id);
    }
    Ok(())
}
async fn write<W: AsyncWrite + Unpin>(w: &mut W, v: &Value) -> Result<()> {
    w.write_all(encode_line(v).as_bytes()).await?;
    w.flush().await?;
    Ok(())
}
async fn op_register(state: &AppState, msg: &Value, writer: &mut OwnedWriteHalf) -> Result<()> {
    let agent: Agent = match msg
        .get("agent")
        .and_then(|a| serde_json::from_value(a.clone()).ok())
    {
        Some(a) => a,
        None => {
            write(writer, &err_value("payload", "register requires agent")).await?;
            return Ok(());
        }
    };
    if agent.id.is_empty() || agent.id.len() > 128 {
        write(writer, &err_value("payload", "agent.id invalid")).await?;
        return Ok(());
    }
    let existing = state.store.token_hash(&agent.id)?;
    let provided = msg.get("token").and_then(|t| t.as_str());
    let mut issued: Option<String> = None;
    let token_hash = match (&existing, provided) {
        (Some(_), None) => {
            write(writer, &err_value("auth", "re-register requires token")).await?;
            return Ok(());
        }
        (Some(_), Some(t)) if !state.store.token_matches(&agent.id, t) => {
            write(writer, &err_value("auth", "token mismatch")).await?;
            return Ok(());
        }
        (Some(_), Some(t)) => Some(hash_token(t)),
        (None, _) => {
            let t = new_token();
            issued = Some(t.clone());
            Some(hash_token(&t))
        }
    };
    let role = Role::assign(&agent);
    state
        .store
        .upsert_agent(&agent, role, token_hash.as_deref())?;
    write(
        writer,
        &json!({"op": "registered", "agent": agent.id, "role": role, "token": issued}),
    )
    .await?;
    Ok(())
}
async fn op_join(
    state: &AppState,
    me: &Option<Agent>,
    msg: &Value,
    writer: &mut OwnedWriteHalf,
) -> Result<()> {
    let Some(agent) = me else {
        write(
            writer,
            &err_value("proto", "hello with agent required first"),
        )
        .await?;
        return Ok(());
    };
    let Some(room) = msg.get("room").and_then(|r| r.as_str()) else {
        write(writer, &err_value("payload", "join requires room")).await?;
        return Ok(());
    };
    if !valid_room(room) {
        write(
            writer,
            &err_value("payload", "room must be # prefixed, no whitespace"),
        )
        .await?;
        return Ok(());
    }
    state.store.join(&agent.id, room)?;
    state.router.member_joined(room, &agent.id);
    let seq = state.store.room_seq(room)?;
    write(writer, &json!({"op": "joined", "room": room, "seq": seq})).await?;
    Ok(())
}
async fn op_leave(
    state: &AppState,
    me: &Option<Agent>,
    msg: &Value,
    writer: &mut OwnedWriteHalf,
) -> Result<()> {
    let Some(agent) = me else {
        write(
            writer,
            &err_value("proto", "hello with agent required first"),
        )
        .await?;
        return Ok(());
    };
    let Some(room) = msg.get("room").and_then(|r| r.as_str()) else {
        write(writer, &err_value("payload", "leave requires room")).await?;
        return Ok(());
    };
    state.store.leave(&agent.id, room)?;
    state.router.member_left(room, &agent.id);
    write(writer, &json!({"op": "left", "room": room})).await?;
    Ok(())
}
async fn op_send(
    state: &AppState,
    me: &Option<Agent>,
    msg: &Value,
    writer: &mut OwnedWriteHalf,
) -> Result<()> {
    let Some(agent) = me else {
        write(
            writer,
            &err_value("proto", "hello with agent required first"),
        )
        .await?;
        return Ok(());
    };
    let Some(room) = msg.get("room").and_then(|r| r.as_str()).map(String::from) else {
        write(writer, &err_value("payload", "send requires room")).await?;
        return Ok(());
    };
    if !valid_room(&room) {
        write(
            writer,
            &err_value("payload", "room must be # prefixed, no whitespace"),
        )
        .await?;
        return Ok(());
    }
    let Some(kind_str) = msg.get("kind").and_then(|k| k.as_str()) else {
        write(writer, &err_value("payload", "send requires kind")).await?;
        return Ok(());
    };
    let Some(kind) = Kind::parse(kind_str) else {
        write(
            writer,
            &err_value("payload", format!("unknown kind {kind_str:?}")),
        )
        .await?;
        return Ok(());
    };
    if !state.router.rate_check(&agent.id) {
        write(writer, &err_value("rate", "over 60 msg/s")).await?;
        return Ok(());
    }
    let role = state.store.agent_role(&agent.id)?.unwrap_or(Role::Protocol);
    let mut env = Envelope::new(&room, agent.clone(), kind);
    env.text = msg.get("text").and_then(|t| t.as_str().map(String::from));
    env.payload = msg.get("payload").cloned().filter(|p| !p.is_null());
    env.thread = msg.get("thread").and_then(|t| t.as_str().map(String::from));
    env.reply_to = msg
        .get("reply_to")
        .and_then(|t| t.as_str().map(String::from));
    env.lamport = msg.get("lamport").and_then(|l| l.as_u64()).unwrap_or(0);
    if let Err((code, m)) = crate::router::validate_send(&env, kind, role) {
        write(writer, &err_value(&code, m)).await?;
        return Ok(());
    }
    state.store.ensure_room(&room)?;
    let seq = state.store.append(&mut env, kind)?;
    state.router.fan_out(&env);
    let mut reply = json!({"op": OP_SENT, "id": env.id, "seq": seq, "room": room});
    if let Some(Trust::Peer) = env.trust {
        reply["trust"] = json!("peer");
    }
    write(writer, &reply).await?;
    Ok(())
}
async fn op_recv(
    state: &AppState,
    me: &Option<Agent>,
    msg: &Value,
    writer: &mut OwnedWriteHalf,
) -> Result<()> {
    let room = msg.get("room").and_then(|r| r.as_str());
    let timeout_ms = msg
        .get("timeout_ms")
        .and_then(|t| t.as_u64())
        .unwrap_or(5000);
    let timeout = Duration::from_millis(timeout_ms.min(120_000)); /* An unidentified connection has no subscription and no cursor, so there is nothing it may be told. It times out like an agent with an empty queue instead of erroring, which would read as a protocol fault. */
    let Some(agent) = me else {
        tokio::time::sleep(timeout).await;
        write(writer, &json!({"op": "timeout"})).await?;
        return Ok(());
    };
    match state.router.recv(&agent.id, room, timeout).await {
        Some(env) => write(writer, &json!({"op": "msg", "envelope": env})).await?,
        None => write(writer, &json!({"op": "timeout"})).await?,
    }
    Ok(())
}
async fn op_history(state: &AppState, msg: &Value, writer: &mut OwnedWriteHalf) -> Result<()> {
    let Some(room) = msg.get("room").and_then(|r| r.as_str()) else {
        write(writer, &err_value("payload", "history requires room")).await?;
        return Ok(());
    };
    let since = msg.get("since").and_then(|s| s.as_i64()).unwrap_or(0);
    let limit = msg
        .get("limit")
        .and_then(|l| l.as_i64())
        .unwrap_or(500)
        .clamp(1, 5000);
    let msgs = state.store.history(room, since, limit)?;
    write(
        writer,
        &json!({"op": "history", "room": room, "messages": msgs, "count": msgs.len()}),
    )
    .await?;
    Ok(())
}
async fn op_who(state: &AppState, msg: &Value, writer: &mut OwnedWriteHalf) -> Result<()> {
    let room = msg.get("room").and_then(|r| r.as_str());
    let agents = state.store.who(room)?;
    write(
        writer,
        &json!({"op": "who", "agents": agents, "count": agents.len()}),
    )
    .await?;
    Ok(())
}
async fn op_rooms(state: &AppState, writer: &mut OwnedWriteHalf) -> Result<()> {
    let rooms = state.store.rooms()?;
    write(
        writer,
        &json!({"op": "rooms", "rooms": rooms, "count": rooms.len()}),
    )
    .await?;
    Ok(())
}

// Silence unused warning for broker_envelope (used from Phase 3 paths/tests).
#[allow(dead_code)]
pub fn presence_note(room: &str, text: &str) -> Envelope {
    broker_envelope(room, Kind::Presence, text)
}
