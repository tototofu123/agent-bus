//! `bus telegram` — a two-way bridge between one bus room and one Telegram chat.
//!
//! Telegram is a human surface, so messages typed there enter the bus as
//! `@human` with `trust=human`. The bridge is deliberately not a model: it
//! forwards text, it does not summarise or decide.
//!
//! The first Telegram chat that messages the bot claims it, and the binding is
//! written once to `$AGENT_BUS_HOME/telegram.json`. Later chats are ignored.

use crate::{bus_home, make_agent, read_token, token_path, Transport};
use anyhow::{anyhow, bail, Context, Result};
use proto::{decode_line, Agent, Envelope};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc;

const DEFAULT_ROOM: &str = "#repo:agent-bus";
const TELEGRAM_TEXT_LIMIT: usize = 3_800;
const POLL_TIMEOUT_S: u64 = 20;

#[derive(Serialize, Deserialize, Default)]
struct BridgeState {
    /// The one chat allowed to drive the bridge. Written once, first-wins.
    chat_id: Option<i64>,
    /// Telegram update offset, so a restart does not replay old commands.
    offset: Option<i64>,
    /// Last room used, so `bus telegram` with no flag resumes the same room.
    room: Option<String>,
}

fn state_path() -> PathBuf {
    bus_home().join("telegram.json")
}

fn load_state() -> Result<BridgeState> {
    let path = state_path();
    match std::fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str(&s)
            .with_context(|| format!("parse {} (delete it to re-pair)", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BridgeState::default()),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

fn save_state(state: &BridgeState) -> Result<()> {
    let path = state_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(state)?)
        .with_context(|| format!("write {}", path.display()))
}

fn read_bot_token(explicit: Option<&str>) -> Result<String> {
    if let Ok(t) = std::env::var("AGENT_BUS_TELEGRAM_TOKEN") {
        let t = t.trim().to_string();
        if !t.is_empty() {
            return Ok(t);
        }
    }
    let path = explicit
        .map(PathBuf::from)
        .unwrap_or_else(|| bus_home().join("telegram.token"));
    let token = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "read Telegram bot token at {} (set AGENT_BUS_TELEGRAM_TOKEN or create the file)",
            path.display()
        )
    })?;
    let token = token.trim().to_string();
    if token.is_empty() {
        bail!("Telegram bot token at {} is empty", path.display());
    }
    Ok(token)
}

fn valid_room(room: &str) -> bool {
    room.starts_with('#') && room.len() <= 64 && !room.contains(char::is_whitespace)
}

// ---------------------------------------------------------------------------
// Telegram Bot API

struct Telegram {
    http: reqwest::Client,
    token: String,
    base: String,
}

impl Telegram {
    fn new(token: String, base: String) -> Result<Telegram> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(70))
            .build()
            .context("build Telegram HTTP client")?;
        Ok(Telegram {
            http,
            token,
            base: base.trim_end_matches('/').to_string(),
        })
    }

    async fn call(&self, method: &str, body: Value) -> Result<Value> {
        let url = format!("{}/bot{}/{}", self.base, self.token, method);
        // reqwest errors embed the request URL, and this URL contains the bot
        // token. Strip it before the error can reach a log.
        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                anyhow!(
                    "Telegram {method} request failed: {}",
                    redact_token(&e.to_string(), &self.token)
                )
            })?;
        let status = resp.status();
        let raw = resp.bytes().await.map_err(|e| {
            anyhow!(
                "Telegram {method} body failed: {}",
                redact_token(&e.to_string(), &self.token)
            )
        })?;
        let reply: Value = serde_json::from_slice(&raw).map_err(|e| {
            anyhow!("Telegram {method} returned non-JSON ({status}): {e}")
        })?;
        if !reply.get("ok").and_then(|o| o.as_bool()).unwrap_or(false) {
            let desc = reply
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("unknown error");
            bail!("Telegram {method} failed ({status}): {desc}");
        }
        Ok(reply.get("result").cloned().unwrap_or(Value::Null))
    }

    async fn get_me(&self) -> Result<Value> {
        self.call("getMe", json!({})).await
    }

    async fn get_updates(&self, offset: i64, timeout_s: u64) -> Result<Vec<Value>> {
        let result = self
            .call(
                "getUpdates",
                json!({
                    "offset": offset,
                    "timeout": timeout_s,
                    "allowed_updates": ["message"],
                }),
            )
            .await?;
        Ok(result.as_array().cloned().unwrap_or_default())
    }

    async fn send_message(&self, chat_id: i64, text: &str) -> Result<()> {
        let text = if text.trim().is_empty() {
            "(empty)"
        } else {
            text
        };
        let chunks = split_chunks(text, TELEGRAM_TEXT_LIMIT);
        let total = chunks.len();
        for (i, chunk) in chunks.iter().enumerate() {
            let body = if total > 1 {
                format!("({}/{})\n{}", i + 1, total, chunk)
            } else {
                chunk.to_string()
            };
            self.call(
                "sendMessage",
                json!({
                    "chat_id": chat_id,
                    "text": body,
                    "disable_web_page_preview": true,
                }),
            )
            .await?;
        }
        Ok(())
    }
}

/// Never let a reqwest error print the bot token from the request URL.
fn redact_token(text: &str, token: &str) -> String {
    if token.is_empty() {
        return text.to_string();
    }
    text.replace(token, "<telegram-token>")
}

/// Split on UTF-16 code units, which is what Telegram's 4096 limit counts.
/// Splitting on characters would overflow for emoji-heavy text, and splitting
/// on bytes would slice a code point in half.
fn split_chunks(text: &str, max_units: usize) -> Vec<String> {
    if text.encode_utf16().count() <= max_units {
        return vec![text.to_string()];
    }
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut units = 0usize;
    for ch in text.chars() {
        let width = ch.len_utf16();
        if units + width > max_units && !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
            units = 0;
        }
        current.push(ch);
        units += width;
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

// ---------------------------------------------------------------------------
// Bus helpers (one-shot connections, plus the long-lived tail)

async fn ensure_registered(transport: &Transport, port: u16, agent: &Agent) -> Result<()> {
    let existing = read_token(&agent.id);
    let mut c = transport.connect(port).await?;
    c.hello(None, None).await?;
    let mut v = json!({"op": "register", "agent": agent});
    if let Some(t) = &existing {
        v["token"] = t.as_str().into();
    }
    let reply = c.op(v).await?;
    c.bye().await;
    if let Some(token) = reply.get("token").and_then(|t| t.as_str()) {
        let path = token_path(&agent.id);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, token)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        eprintln!("telegram: registered {} token stored", agent.id);
    }
    Ok(())
}

async fn join_room(transport: &Transport, port: u16, agent: &Agent, room: &str) -> Result<i64> {
    let token = read_token(&agent.id);
    let mut c = transport.connect(port).await?;
    c.hello(Some(agent), token.as_deref()).await?;
    let reply = c.op(json!({"op": "join", "room": room})).await?;
    c.bye().await;
    Ok(reply.get("seq").and_then(|s| s.as_i64()).unwrap_or(0))
}

async fn send_once(
    transport: &Transport,
    port: u16,
    agent: &Agent,
    room: &str,
    kind: &str,
    text: Option<&str>,
    payload: Option<Value>,
) -> Result<(String, i64)> {
    let token = read_token(&agent.id);
    let mut c = transport.connect(port).await?;
    c.hello(Some(agent), token.as_deref()).await?;
    let mut v = json!({"op": "send", "room": room, "kind": kind});
    if let Some(t) = text {
        v["text"] = t.into();
    }
    if let Some(p) = payload {
        v["payload"] = p;
    }
    let reply = c.op(v).await?;
    c.bye().await;
    Ok((
        reply
            .get("id")
            .and_then(|i| i.as_str())
            .unwrap_or("?")
            .to_string(),
        reply.get("seq").and_then(|s| s.as_i64()).unwrap_or(0),
    ))
}

async fn bus_who(transport: &Transport, port: u16, room: &str) -> Result<String> {
    let mut c = transport.connect(port).await?;
    c.hello(None, None).await?;
    let reply = c.op(json!({"op": "who", "room": room})).await?;
    c.bye().await;
    let agents = reply
        .get("agents")
        .and_then(|a| a.as_array())
        .cloned()
        .unwrap_or_default();
    if agents.is_empty() {
        return Ok("(nobody in this room)".into());
    }
    let mut lines = Vec::new();
    for a in agents {
        lines.push(format!(
            "{:<28} {:<12} {:<20} role={}",
            a.get("id").and_then(|v| v.as_str()).unwrap_or("?"),
            a.get("harness").and_then(|v| v.as_str()).unwrap_or("?"),
            a.get("model").and_then(|v| v.as_str()).unwrap_or("-"),
            a.get("role").and_then(|v| v.as_str()).unwrap_or("?"),
        ));
    }
    Ok(lines.join("\n"))
}

async fn bus_rooms(transport: &Transport, port: u16) -> Result<String> {
    let mut c = transport.connect(port).await?;
    c.hello(None, None).await?;
    let reply = c.op(json!({"op": "rooms"})).await?;
    c.bye().await;
    let rooms = reply
        .get("rooms")
        .and_then(|r| r.as_array())
        .cloned()
        .unwrap_or_default();
    if rooms.is_empty() {
        return Ok("(no rooms)".into());
    }
    let mut lines = Vec::new();
    for r in rooms {
        lines.push(format!(
            "{:<40} seq={}",
            r.get("name").and_then(|v| v.as_str()).unwrap_or("?"),
            r.get("seq").and_then(|v| v.as_i64()).unwrap_or(0),
        ));
    }
    Ok(lines.join("\n"))
}

async fn bus_seq(transport: &Transport, port: u16, room: &str) -> Result<i64> {
    let mut c = transport.connect(port).await?;
    c.hello(None, None).await?;
    let reply = c.op(json!({"op": "rooms"})).await?;
    c.bye().await;
    Ok(reply
        .get("rooms")
        .and_then(|r| r.as_array())
        .and_then(|rooms| {
            rooms.iter().find_map(|r| {
                (r.get("name").and_then(|n| n.as_str()) == Some(room))
                    .then(|| r.get("seq").and_then(|s| s.as_i64()).unwrap_or(0))
            })
        })
        .unwrap_or(0))
}

async fn bus_history(
    transport: &Transport,
    port: u16,
    room: &str,
    since: i64,
    limit: i64,
) -> Result<Vec<Envelope>> {
    let mut c = transport.connect(port).await?;
    c.hello(None, None).await?;
    let reply = c
        .op(json!({"op": "history", "room": room, "since": since, "limit": limit}))
        .await?;
    c.bye().await;
    let messages = reply.get("messages").cloned().unwrap_or_else(|| json!([]));
    serde_json::from_value(messages).context("decode bus history")
}

async fn tail_loop(
    transport: Transport,
    port: u16,
    agent: Agent,
    room: String,
    tx: mpsc::UnboundedSender<Envelope>,
) {
    loop {
        if let Err(e) = tail_once(&transport, port, &agent, &room, &tx).await {
            eprintln!("telegram: bus tail error: {e:#}");
        }
        if tx.is_closed() {
            return;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn tail_once(
    transport: &Transport,
    port: u16,
    agent: &Agent,
    room: &str,
    tx: &mpsc::UnboundedSender<Envelope>,
) -> Result<()> {
    let token = read_token(&agent.id);
    let mut c = transport.connect(port).await?;
    c.hello(Some(agent), token.as_deref()).await?;
    c.send_raw(&json!({"op": "tail", "rooms": [room]})).await?;
    let ack = c.read_msg().await?;
    if ack.get("op").and_then(|o| o.as_str()) != Some("tail") {
        bail!("tail rejected: {ack}");
    }
    eprintln!("telegram: tailing {room}");
    let mut line = String::new();
    loop {
        line.clear();
        let n = c.read_line_dyn(&mut line).await?;
        if n == 0 {
            bail!("tail connection closed");
        }
        let v = match decode_line(line.trim_end_matches('\n').as_bytes()) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v.get("op").and_then(|o| o.as_str()) != Some("msg") {
            continue;
        }
        let Some(envelope) = v.get("envelope") else {
            continue;
        };
        let env: Envelope = match serde_json::from_value(envelope.clone()) {
            Ok(env) => env,
            Err(_) => continue,
        };
        // Never echo our own messages back to Telegram.
        if env.from.id == agent.id {
            continue;
        }
        if tx.send(env).is_err() {
            return Ok(());
        }
    }
}

// ---------------------------------------------------------------------------
// Formatting and commands

fn format_envelope(env: &Envelope) -> String {
    let seq = env.seq.map(|s| s.to_string()).unwrap_or_else(|| "-".into());
    let mut out = format!("[{seq}] {} · {}\n", env.kind, env.from.id);
    if let Some(text) = &env.text {
        out.push_str(text);
    }
    if let Some(payload) = &env.payload {
        if env.text.is_none() || env.kind != "chat" {
            out.push('\n');
            out.push_str(&serde_json::to_string(payload).unwrap_or_default());
        }
    }
    if out
        .trim_end()
        .ends_with(&format!("{} · {}", env.kind, env.from.id))
    {
        out.push_str("(no text)");
    }
    out
}

fn split_command(text: &str) -> Option<(&str, &str)> {
    let t = text.trim();
    if !t.starts_with('/') {
        return None;
    }
    let (cmd, rest) = match t.find(char::is_whitespace) {
        Some(i) => (&t[..i], t[i..].trim()),
        None => (t, ""),
    };
    // Groups address commands as /help@botname.
    let cmd = cmd.split('@').next().unwrap_or(cmd);
    Some((cmd, rest))
}

fn claim_args(rest: &str) -> Option<(Vec<String>, u64)> {
    let mut parts: Vec<&str> = rest.split_whitespace().collect();
    if parts.is_empty() {
        return None;
    }
    let mut ttl = 300u64;
    if parts.len() > 1 {
        if let Ok(n) = parts[parts.len() - 1].parse::<u64>() {
            if n > 0 {
                ttl = n;
                parts.pop();
            }
        }
    }
    let files: Vec<String> = parts
        .join(" ")
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();
    if files.is_empty() {
        None
    } else {
        Some((files, ttl))
    }
}

fn help_text(room: &str) -> String {
    format!(
        "agent-bus Telegram bridge\n\
         Room: {room}\n\n\
         Plain text      send a chat message\n\
         /send <text>    send a chat message\n\
         /claim <globs> [ttl]  send task.claim (comma-separated globs)\n\
         /status <text>  send a status message\n\
         /who            who is in the room\n\
         /rooms          list rooms\n\
         /history [n]    replay the last n messages (default 20)\n\
         /room           show the bridged room\n\
         /id             show this chat id\n\
         /help           this message"
    )
}

fn parse_update(update: &Value) -> Option<(i64, String, bool)> {
    let msg = update.get("message")?;
    let chat_id = msg.get("chat")?.get("id")?.as_i64()?;
    let text = msg.get("text")?.as_str()?.to_string();
    let from_bot = msg
        .get("from")
        .and_then(|f| f.get("is_bot"))
        .and_then(|b| b.as_bool())
        .unwrap_or(false);
    Some((chat_id, text, from_bot))
}

async fn handle_text(
    text: &str,
    tg: &Telegram,
    chat_id: i64,
    room: &str,
    agent: &Agent,
    transport: &Transport,
    port: u16,
) -> Result<()> {
    let Some((cmd, rest)) = split_command(text) else {
        let (_, seq) = send_once(
            transport,
            port,
            agent,
            room,
            "chat",
            Some(text.trim()),
            None,
        )
        .await?;
        tg.send_message(chat_id, &format!("→ chat seq {seq}"))
            .await?;
        return Ok(());
    };

    match cmd {
        "/start" | "/help" => {
            tg.send_message(chat_id, &help_text(room)).await?;
        }
        "/id" => {
            tg.send_message(chat_id, &format!("chat_id: {chat_id}"))
                .await?;
        }
        "/room" => {
            tg.send_message(chat_id, &format!("room: {room}\nidentity: {}", agent.id))
                .await?;
        }
        "/who" => {
            let text = bus_who(transport, port, room).await?;
            tg.send_message(chat_id, &text).await?;
        }
        "/rooms" => {
            let text = bus_rooms(transport, port).await?;
            tg.send_message(chat_id, &text).await?;
        }
        "/history" => {
            let limit = rest.parse::<i64>().unwrap_or(20).clamp(1, 50);
            let seq = bus_seq(transport, port, room).await?;
            let since = (seq - limit).max(0);
            let messages = bus_history(transport, port, room, since, limit).await?;
            if messages.is_empty() {
                tg.send_message(chat_id, &format!("(no messages in {room})"))
                    .await?;
            } else {
                let body = messages
                    .iter()
                    .map(format_envelope)
                    .collect::<Vec<_>>()
                    .join("\n\n");
                tg.send_message(chat_id, &body).await?;
            }
        }
        "/claim" => {
            let Some((files, ttl)) = claim_args(rest) else {
                tg.send_message(
                    chat_id,
                    "usage: /claim <glob>[,<glob>...] [ttl_seconds]\nexample: /claim src/auth/login.ts 300",
                )
                .await?;
                return Ok(());
            };
            let payload = json!({"files": files, "claim_ttl_s": ttl});
            let label = format!("claim from Telegram: {}", files.join(", "));
            let (_, seq) = send_once(
                transport,
                port,
                agent,
                room,
                "task.claim",
                Some(&label),
                Some(payload),
            )
            .await?;
            tg.send_message(
                chat_id,
                &format!(
                    "claim sent · seq {seq}\nfiles: {}\nttl: {ttl}s",
                    files.join(", ")
                ),
            )
            .await?;
        }
        "/status" => {
            if rest.is_empty() {
                tg.send_message(chat_id, "usage: /status <text>").await?;
                return Ok(());
            }
            let (_, seq) =
                send_once(transport, port, agent, room, "status", Some(rest), None).await?;
            tg.send_message(chat_id, &format!("status sent · seq {seq}"))
                .await?;
        }
        "/send" => {
            if rest.is_empty() {
                tg.send_message(chat_id, "usage: /send <text>").await?;
                return Ok(());
            }
            let (_, seq) =
                send_once(transport, port, agent, room, "chat", Some(rest), None).await?;
            tg.send_message(chat_id, &format!("→ chat seq {seq}"))
                .await?;
        }
        other => {
            tg.send_message(
                chat_id,
                &format!("unknown command {other}\n\n{}", help_text(room)),
            )
            .await?;
        }
    }
    Ok(())
}

async fn discover_chat(tg: &Telegram, state: &mut BridgeState) -> Result<i64> {
    eprintln!("telegram: waiting for the first chat to message the bot...");
    loop {
        let updates = tg.get_updates(state.offset.unwrap_or(0), 0).await?;
        // Advance past what we have seen even if it was not a message, so a
        // stream of service updates cannot make this loop spin on the same batch.
        let mut picked = None;
        for update in &updates {
            if let Some(id) = update.get("update_id").and_then(|i| i.as_i64()) {
                state.offset = Some(id + 1);
            }
            if let Some((chat_id, _, from_bot)) = parse_update(update) {
                if !from_bot {
                    picked = Some(chat_id);
                }
            }
        }
        if let Some(chat_id) = picked {
            state.chat_id = Some(chat_id);
            save_state(state)?;
            eprintln!("telegram: chat {chat_id} claimed the bot");
            return Ok(chat_id);
        }
        save_state(state)?;
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

// ---------------------------------------------------------------------------
// Entry point

pub async fn run(args: crate::TelegramArgs, port: u16, transport: Transport) -> Result<()> {
    let bot_token = read_bot_token(args.token_file.as_deref())?;
    let tg = Telegram::new(bot_token, args.api_base.clone())?;
    let me = tg.get_me().await.context("Telegram getMe")?;
    let username = me
        .get("username")
        .and_then(|u| u.as_str())
        .unwrap_or("unknown");

    let mut state = load_state()?;
    let room = args
        .room
        .clone()
        .or_else(|| state.room.clone())
        .unwrap_or_else(|| DEFAULT_ROOM.to_string());
    if !valid_room(&room) {
        bail!("room must be # prefixed and contain no whitespace: {room:?}");
    }
    state.room = Some(room.clone());
    if let Some(chat) = args.chat {
        state.chat_id = Some(chat);
    }
    save_state(&state)?;

    let agent = make_agent(&args.agent, "telegram", None, &[], None);
    ensure_registered(&transport, port, &agent).await?;
    let seq = join_room(&transport, port, &agent, &room).await?;
    eprintln!(
        "telegram: @{username} -> {room} as {} (joined at seq {seq})",
        agent.id
    );

    let chat_id = match state.chat_id {
        Some(chat) => chat,
        None => discover_chat(&tg, &mut state).await?,
    };
    let _ = tg
        .send_message(
            chat_id,
            &format!(
                "agent-bus connected.\nRoom: {room}\nIdentity: {}\nThe first chat to message the bot owns it. Send /help for commands.",
                agent.id
            ),
        )
        .await
        .map_err(|e| eprintln!("telegram: greeting failed: {e:#}"));

    let (tx, mut rx) = mpsc::unbounded_channel();
    tokio::spawn(tail_loop(
        transport.clone(),
        port,
        agent.clone(),
        room.clone(),
        tx,
    ));

    loop {
        tokio::select! {
            env = rx.recv() => {
                match env {
                    Some(env) => {
                        if let Err(e) = tg.send_message(chat_id, &format_envelope(&env)).await {
                            eprintln!("telegram: forward failed: {e:#}");
                        }
                    }
                    None => break,
                }
            }
            updates = tg.get_updates(state.offset.unwrap_or(0), POLL_TIMEOUT_S) => {
                match updates {
                    Ok(updates) => {
                        for update in updates {
                            if let Some(id) = update.get("update_id").and_then(|i| i.as_i64()) {
                                state.offset = Some(id + 1);
                            }
                            let Some((from_chat, text, from_bot)) = parse_update(&update) else {
                                save_state(&state)?;
                                continue;
                            };
                            if from_chat != chat_id || from_bot {
                                eprintln!("telegram: ignoring chat {from_chat}");
                                save_state(&state)?;
                                continue;
                            }
                            if let Err(e) = handle_text(
                                &text,
                                &tg,
                                chat_id,
                                &room,
                                &agent,
                                &transport,
                                port,
                            )
                            .await
                            {
                                eprintln!("telegram: command failed: {e:#}");
                                let _ = tg
                                    .send_message(chat_id, &format!("error: {e:#}"))
                                    .await;
                            }
                            save_state(&state)?;
                        }
                    }
                    Err(e) => {
                        eprintln!("telegram: getUpdates failed: {e:#}");
                        tokio::time::sleep(Duration::from_secs(3)).await;
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proto::Agent as ProtoAgent;

    fn agent() -> ProtoAgent {
        ProtoAgent {
            id: "codex@test".into(),
            harness: "codex-cli".into(),
            model: Some("gpt-5-codex".into()),
            session: None,
            cwd: None,
            caps: vec![],
        }
    }

    #[test]
    fn chunks_long_text_without_splitting_characters() {
        let text = "a".repeat(10) + &"é".repeat(5);
        let chunks = split_chunks(&text, 8);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn short_text_is_one_chunk() {
        assert_eq!(split_chunks("hello", 8), vec!["hello"]);
    }

    #[test]
    fn token_is_redacted_from_errors() {
        let msg = "error sending request for url (https://api.telegram.org/bot123:ABC/getMe)";
        assert_eq!(
            redact_token(msg, "123:ABC"),
            "error sending request for url (https://api.telegram.org/bot<telegram-token>/getMe)"
        );
    }

    #[test]
    fn chunks_respect_utf16_units_for_emoji() {
        let text = "😀".repeat(5);
        let chunks = split_chunks(&text, 3);
        assert_eq!(chunks.concat(), text);
        assert!(chunks
            .iter()
            .all(|c| c.encode_utf16().count() <= 3));
    }

    #[test]
    fn commands_strip_group_suffix_and_split_args() {
        assert_eq!(
            split_command("/help@opencodetototofubot"),
            Some(("/help", ""))
        );
        assert_eq!(
            split_command("  /claim src/a.ts 60 "),
            Some(("/claim", "src/a.ts 60"))
        );
        assert_eq!(split_command("hello"), None);
    }

    #[test]
    fn claim_args_parse_ttl_and_globs() {
        assert_eq!(
            claim_args("src/a.ts,src/b.ts 300"),
            Some((vec!["src/a.ts".into(), "src/b.ts".into()], 300))
        );
        assert_eq!(claim_args("src/a.ts"), Some((vec!["src/a.ts".into()], 300)));
        assert_eq!(claim_args(""), None);
    }

    #[test]
    fn envelope_line_names_kind_and_sender() {
        let mut env = Envelope::new("#repo:test", agent(), proto::Kind::Chat);
        env.seq = Some(7);
        env.text = Some("hi".into());
        let line = format_envelope(&env);
        assert!(line.starts_with("[7] chat · codex@test"));
        assert!(line.ends_with("hi"));
    }
}
