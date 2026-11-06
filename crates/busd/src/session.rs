// Per-connection protocol state machine: handshake, then one op per line.
use crate::router::Router;
use crate::store::{broker_envelope, Store};
use anyhow::{anyhow, Result};
use proto::{
    decode_line, encode_line, hash_token, new_token, Agent, Envelope, Kind, Role, Trust,
    MAX_LINE_BYTES, OP_BYE, OP_ERROR, OP_HELLO, OP_SENT, OP_SENT_BATCH, OP_WELCOME, V,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use crate::TransportStream;
pub struct AppState {
    pub store: Store,
    pub router: Router,
    pub started: std::time::SystemTime,
    /// Live connections per agent id. A count, not a flag: an agent commonly
    /// holds a tail and one-shot send connections at the same time, and the
    /// last one to close is what makes it offline.
    ///
    /// In memory on purpose. This is a property of *this broker process*, not
    /// of the durable identity: after a restart every agent is offline until
    /// it reconnects, which is the truthful answer.
    pub presence: Mutex<HashMap<String, usize>>,
}
impl AppState {
    pub fn new(store: Store) -> AppState {
        AppState {
            store,
            router: Router::new(),
            started: std::time::SystemTime::now(),
            presence: Mutex::new(HashMap::new()),
        }
    }

    /// Record one live connection for `id`.
    pub fn presence_join(&self, id: &str) {
        if let Ok(mut p) = self.presence.lock() {
            *p.entry(id.to_string()).or_insert(0) += 1;
        }
    }

    /// Drop one live connection for `id`, removing the entry at zero so a
    /// disconnecting agent cannot leave a phantom behind.
    pub fn presence_leave(&self, id: &str) {
        if let Ok(mut p) = self.presence.lock() {
            let gone = match p.get_mut(id) {
                Some(n) if *n > 1 => {
                    *n -= 1;
                    false
                }
                Some(_) => true,
                None => false,
            };
            if gone {
                p.remove(id);
            }
        }
    }

    /// How many live connections this agent currently holds.
    pub fn presence_count(&self, id: &str) -> usize {
        self.presence
            .lock()
            .ok()
            .and_then(|p| p.get(id).copied())
            .unwrap_or(0)
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

/// Releases an agent's presence slot when the connection ends, on every exit
/// path including a protocol error or a dropped socket. Without the guard the
/// early `?` returns in `handle_conn` would leave the agent permanently online.
struct PresenceGuard {
    state: Arc<AppState>,
    id: String,
}

impl Drop for PresenceGuard {
    fn drop(&mut self) {
        self.state.presence_leave(&self.id);
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
//
// A line longer than the BufReader's buffer spans several `fill_buf` chunks, so
// the newline is often found in a *later* chunk than the first. Within that
// later chunk the newline offset is only the tail of the line -- the earlier
// chunks are already accumulated in `buf` -- so the returned length is
// `buf.len()`, not the newline offset. Returning the offset alone silently
// truncated every frame larger than the buffer.
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
            let tail = nl + 1;
            buf.extend_from_slice(&available[..tail]);
            r.consume(tail);

            // `buf` now holds the whole line: the chunks accumulated by earlier
            // iterations plus this chunk up to and including its newline.
            return Ok(buf.len());
        }
        if buf.len() + available.len() > MAX_LINE_BYTES {
            return Err(anyhow!("line exceeds {} bytes", MAX_LINE_BYTES));
        }
        let l = available.len();
        buf.extend_from_slice(available);
        r.consume(l);
    }
}
pub async fn handle_conn(stream: TransportStream, state: Arc<AppState>) -> Result<()> {
    // set_nodelay only applies to TCP, skip for named pipes
    if let TransportStream::Tcp(ref s) = stream {
        s.set_nodelay(true).ok();
    }
    let (rd, wr) = tokio::io::split(stream);
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
    // From here on this connection is a live sighting of the agent. The guard
    // releases it however the function exits.
    let _presence = me.as_ref().map(|a| {
        state.presence_join(&a.id);
        PresenceGuard {
            state: state.clone(),
            id: a.id.clone(),
        }
    });
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
        tokio::select! {            n = read_line_capped(&mut reader, &mut line) => {                let n = n?;                if n == 0 { break; }                let msg = match decode_line(&line) {                    Ok(v) => v,                    Err(e) => {                        write(&mut writer, &err_value("frame", e.to_string())).await?;                        continue;                    }                };                let op = msg.get("op").and_then(|o| o.as_str()).unwrap_or_default().to_string();                match op.as_str() {                    OP_BYE => {                        write(&mut writer, &json!({"op": OP_BYE})).await?;                        break;                    }                    "register" => {                        if let Err(e) = op_register(&state, &msg, &mut writer).await {
            let _ = write(&mut writer, &err_value("internal", e.to_string())).await;
        }                    }                    "join" => {                        if let Err(e) = op_join(&state, &me, &msg, &mut writer).await {
            let _ = write(&mut writer, &err_value("internal", e.to_string())).await;
        }                    }                    "leave" => {                        if let Err(e) = op_leave(&state, &me, &msg, &mut writer).await {
            let _ = write(&mut writer, &err_value("internal", e.to_string())).await;
        }                    }                    "send" => {                        if let Err(e) = op_send(&state, &me, &msg, &mut writer).await {
            let _ = write(&mut writer, &err_value("internal", e.to_string())).await;
        }                    }                    "send_batch" => {                        if let Err(e) = op_send_batch(&state, &me, &msg, &mut writer).await {
            let _ = write(&mut writer, &err_value("internal", e.to_string())).await;
        }                    }                    "recv" => {                        if let Err(e) = op_recv(&state, &me, &msg, &mut writer).await {
            let _ = write(&mut writer, &err_value("internal", e.to_string())).await;
        }                    }                    "history" => {                        if let Err(e) = op_history(&state, &msg, &mut writer).await {
            let _ = write(&mut writer, &err_value("internal", e.to_string())).await;
        }                    }                    "who" => {                        if let Err(e) = op_who(&state, &msg, &mut writer).await {
            let _ = write(&mut writer, &err_value("internal", e.to_string())).await;
        }                    }                    "rooms" => {                        if let Err(e) = op_rooms(&state, &mut writer).await {
            let _ = write(&mut writer, &err_value("internal", e.to_string())).await;
        }                    }                    "tail" => {                        let rooms: Option<std::collections::HashSet<String>> = msg                            .get("rooms")                            .and_then(|r| r.as_array())                            .map(|a| a.iter().filter_map(|s| s.as_str().map(String::from)).collect());                        let kinds: Option<std::collections::HashSet<String>> = msg                            .get("kinds")                            .and_then(|k| k.as_array())                            .map(|a| a.iter().filter_map(|s| s.as_str().map(String::from)).collect());                        let (id, rx) = state.router.subscribe_tail(rooms, kinds);                        tail_id = Some(id);                        tail_rx = Some(rx);                        write(&mut writer, &json!({"op": "tail", "ok": true})).await?;                    }                    other => {                        write(&mut writer, &err_value("op", format!("unknown op {other:?}"))).await?;                    }                }            }            ev = async { match tail_rx.as_mut() { Some(rx) => rx.recv().await, None => std::future::pending().await } } => {                if let Some(env) = ev {                    write(&mut writer, &json!({"op": "msg", "envelope": env})).await?;                }            }        }
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
async fn op_register(state: &AppState, msg: &Value, writer: &mut (impl AsyncWrite + Unpin)) -> Result<()> {
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
    // A person is not a model, so the model floor does not apply. Keeping the
    // role at `peer` is the honest capability statement: a human surface can
    // send every non-broker kind, and `validate_send` separately allows
    // `policy` only for human senders.
    let role = if agent.is_human_surface() {
        Role::Peer
    } else {
        Role::assign(&agent)
    };
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
/// Membership and messages both foreign-key into `agents`, so an identity that
/// never registered gets a typed auth error instead of an opaque constraint
/// failure. Returns false when the op must not proceed.
async fn require_registered(
    state: &AppState,
    id: &str,
    writer: &mut (impl AsyncWrite + Unpin),
) -> Result<bool> {
    match state.store.is_registered(id) {
        Ok(true) => Ok(true),
        Ok(false) => {
            write(
                writer,
                &err_value(
                    "auth",
                    format!("{id} has not registered; run `bus register --agent {id}` first"),
                ),
            )
            .await?;
            Ok(false)
        }
        Err(e) => {
            write(writer, &err_value("internal", e.to_string())).await?;
            Ok(false)
        }
    }
}
async fn op_join(
    state: &AppState,
    me: &Option<Agent>,
    msg: &Value,
    writer: &mut (impl AsyncWrite + Unpin),
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
    if !require_registered(state, &agent.id, writer).await? {
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
    writer: &mut (impl AsyncWrite + Unpin),
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
    if !require_registered(state, &agent.id, writer).await? {
        return Ok(());
    }
    state.store.leave(&agent.id, room)?;
    state.router.member_left(room, &agent.id);
    write(writer, &json!({"op": "left", "room": room})).await?;
    Ok(())
}
async fn op_send(
    state: &AppState,
    me: &Option<Agent>,
    msg: &Value,
    writer: &mut (impl AsyncWrite + Unpin),
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
    // Registration is checked inside `append_from_registered`, which does it
    // in the same transaction as the append, so it is not checked here first.
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
    let human = agent.is_human_surface();
    let role = if human {
        Role::Peer
    } else {
        state.store.agent_role(&agent.id)?.unwrap_or(Role::Protocol)
    };
    let mut env = envelope_from_op(&room, agent, msg, kind, human);
    if let Err((code, m)) = crate::router::validate_send(&env, kind, role, human) {
        write(writer, &err_value(&code, m)).await?;
        return Ok(());
    }
    // Registration, room existence and the append itself share one
    // transaction. `op_send` used to run these as four separate store calls,
    // each committing; folding them is what brought send p99 under the 1 ms
    // criterion. `None` means the sender never registered.
    let Some(seq) = state.store.append_from_registered(&mut env, kind)? else {
        write(
            writer,
            &err_value("auth", "anonymous@bus has not registered; run `bus register` first"),
        )
        .await?;
        return Ok(());
    };
    state.router.fan_out(&env);
    let mut reply = json!({"op": OP_SENT, "id": env.id, "seq": seq, "room": room});
    if let Some(Trust::Peer) = env.trust {
        reply["trust"] = json!("peer");
    }
    write(writer, &reply).await?;
    Ok(())
}
/// Build an envelope from a client `send`/`send_batch` message body. Shared so
/// the single and batch paths accept exactly the same fields.
fn envelope_from_op(
    room: &str,
    agent: &Agent,
    msg: &Value,
    kind: Kind,
    human: bool,
) -> Envelope {
    let mut env = Envelope::new(room, agent.clone(), kind);
    env.text = msg.get("text").and_then(|t| t.as_str().map(String::from));
    env.payload = msg.get("payload").cloned().filter(|p| !p.is_null());
    env.thread = msg.get("thread").and_then(|t| t.as_str().map(String::from));
    env.reply_to = msg
        .get("reply_to")
        .and_then(|t| t.as_str().map(String::from));
    env.lamport = msg.get("lamport").and_then(|l| l.as_u64()).unwrap_or(0);
    // Trust is stamped here, broker-side, never accepted from the sender.
    // `assign_and_insert` only fills this when it is still unset.
    if human {
        env.trust = Some(Trust::Human);
    }
    env
}

/// `send_batch` — many envelopes in one frame, appended in one transaction.
///
/// `busd` replied to each single-message `send` with its own commit; the
/// throughput gate measured that ceiling at ~8k msg/s, which the store itself
/// exhibits with no sockets in play. This op amortises one commit across up to
/// `proto::MAX_BATCH` envelopes, so a room being driven hard pays one fsync for
/// the batch.
///
/// Reply is one `sent_batch` carrying all seqs, so an ack still maps 1:1 to the
/// messages the client asked to send.
async fn op_send_batch(
    state: &AppState,
    me: &Option<Agent>,
    msg: &Value,
    writer: &mut (impl AsyncWrite + Unpin),
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
        write(writer, &err_value("payload", "send_batch requires room")).await?;
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
    let Some(items) = msg.get("messages").and_then(|m| m.as_array()) else {
        write(
            writer,
            &err_value("payload", "send_batch requires messages[]"),
        )
        .await?;
        return Ok(());
    };
    if items.is_empty() {
        write(writer, &err_value("payload", "send_batch has no messages")).await?;
        return Ok(());
    }
    if items.len() > proto::MAX_BATCH {
        write(
            writer,
            &err_value("payload", format!("send_batch over {} messages", proto::MAX_BATCH)),
        )
        .await?;
        return Ok(());
    }

    // Build every envelope first: a frame that cannot parse must not leave a
    // half-committed batch behind.
    let human = agent.is_human_surface();
    let role = if human {
        Role::Peer
    } else {
        state.store.agent_role(&agent.id)?.unwrap_or(Role::Protocol)
    };
    let mut built: Vec<(Envelope, Kind)> = Vec::with_capacity(items.len());
    for item in items {
        let Some(kind_str) = item.get("kind").and_then(|k| k.as_str()) else {
            write(writer, &err_value("payload", "each message requires kind")).await?;
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
        let env = envelope_from_op(&room, agent, item, kind, human);
        // Validate before anything is written, so a rejected frame rejects wholly.
        if let Err((code, m)) = crate::router::validate_send(&env, kind, role, human) {
            write(writer, &err_value(&code, m)).await?;
            return Ok(());
        }
        built.push((env, kind));
    }

    // One rate check per frame against the message count, so a batch cannot be
    // used to slip past the 60 msg/s policy in bulk.
    if !state.router.rate_check_batch(&agent.id, built.len()) {
        write(writer, &err_value("rate", "over 60 msg/s")).await?;
        return Ok(());
    }

    let Some(seqs) = state.store.append_batch(&mut built)? else {
        write(
            writer,
            &err_value("auth", "anonymous@bus has not registered; run `bus register` first"),
        )
        .await?;
        return Ok(());
    };
    for (env, _) in &built {
        state.router.fan_out(env);
    }

    let ids: Vec<&str> = built.iter().map(|(e, _)| e.id.as_str()).collect();
    write(
        writer,
        &json!({"op": OP_SENT_BATCH, "ids": ids, "seqs": seqs, "room": room, "count": seqs.len()}),
    )
    .await?;
    Ok(())
}

async fn op_recv(
    state: &AppState,
    me: &Option<Agent>,
    msg: &Value,
    writer: &mut (impl AsyncWrite + Unpin),
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
async fn op_history(state: &AppState, msg: &Value, writer: &mut (impl AsyncWrite + Unpin)) -> Result<()> {
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
async fn op_who(state: &AppState, msg: &Value, writer: &mut (impl AsyncWrite + Unpin)) -> Result<()> {
    let room = msg.get("room").and_then(|r| r.as_str());
    let mut agents = state.store.who(room)?;
    // Presence is live state, so it is merged here rather than stored: the
    // persisted row can only say when an agent was last seen, never whether it
    // is connected right now.
    for agent in &mut agents {
        let id = agent.get("id").and_then(|i| i.as_str()).unwrap_or_default();
        let connections = state.presence_count(id);
        agent["connections"] = json!(connections);
        agent["online"] = json!(connections > 0);
    }
    write(
        writer,
        &json!({"op": "who", "agents": agents, "count": agents.len()}),
    )
    .await?;
    Ok(())
}
async fn op_rooms(state: &AppState, writer: &mut (impl AsyncWrite + Unpin)) -> Result<()> {
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
