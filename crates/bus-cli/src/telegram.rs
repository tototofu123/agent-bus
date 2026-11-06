//! `bus telegram` — a two-way bridge between one bus room and one Telegram chat.
//!
//! Telegram is a human surface, so messages typed there enter the bus as
//! `@human` with `trust=human`. The bridge is deliberately not a model: it
//! forwards text, it does not summarise or decide.
//!
//! The first Telegram chat that messages the bot claims it, and the binding is
//! written once to `$AGENT_BUS_HOME/telegram.json`. Later chats are ignored.

use crate::{bus_home, ensure_registered, make_agent, read_token, Transport};
use anyhow::{anyhow, bail, Context, Result};
use proto::{decode_line, Agent, Envelope};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc;

const DEFAULT_ROOM: &str = "#repo:agent-bus";
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
        let chunks = split_markdown_chunks(text, TELEGRAM_MARKDOWN_LIMIT);
        let total = chunks.len();
        for (i, chunk) in chunks.iter().enumerate() {
            let prefix = if total > 1 {
                format!("({}/{})\n", i + 1, total)
            } else {
                String::new()
            };
            let html = format!("{prefix}{}", render_telegram_html(chunk));
            match self
                .call(
                    "sendMessage",
                    json!({
                        "chat_id": chat_id,
                        "text": html,
                        "parse_mode": "HTML",
                    }),
                )
                .await
            {
                Ok(_) => {}
                // The model wrote markup Telegram's parser rejects. Deliver it
                // as plain text rather than losing the answer.
                Err(e) if entity_error(&e) => {
                    eprintln!("telegram: HTML rejected ({e}), resending as plain text");
                    let plain = format!("{prefix}{chunk}");
                    self.call(
                        "sendMessage",
                        json!({
                            "chat_id": chat_id,
                            "text": plain,
                        }),
                    )
                    .await?;
                }
                Err(e) => return Err(e),
            }
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

/// Remove the bot's own `@username` from group text. Telegram includes it when
/// a human addresses the bot to wake it; the agents behind the bus should see
/// the question, not the wake word.
fn strip_self_mention(text: &str, username: &str) -> String {
    let needle = format!("@{}", username.to_ascii_lowercase());
    let lower = text.to_ascii_lowercase();
    let mut out = String::new();
    let mut last = 0usize;
    let mut search = 0usize;
    while let Some(pos) = lower[search..].find(&needle) {
        let at = search + pos;
        let end = at + needle.len();
        let boundary = text[end..]
            .chars()
            .next()
            .map(|c| !(c.is_alphanumeric() || c == '_'))
            .unwrap_or(true);
        if boundary {
            out.push_str(&text[last..at]);
            last = end;
        }
        search = end;
    }
    out.push_str(&text[last..]);
    out.trim().to_string()
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

/// Telegram rejects messages over 4096 UTF-16 units, and HTML markup adds
/// characters after rendering, so the raw Markdown is chunked below that and
/// rendered per chunk. Splitting the raw text, not the rendered HTML, is what
/// keeps a tag from being cut in half between two messages.
const TELEGRAM_MARKDOWN_LIMIT: usize = 2_800;

/// Render the slice of Markdown the agents actually write into the HTML that
/// Telegram accepts: bold headings, emphasis, inline and fenced code, links,
/// and pipe tables as an aligned monospace block. Anything else is escaped as
/// literal text, so a `<` can never be mistaken for a tag.
fn render_telegram_html(markdown: &str) -> String {
    let lines: Vec<&str> = markdown.lines().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim_start();

        // Fenced code block.
        if let Some(fence) = trimmed.strip_prefix("```") {
            let lang = fence.trim();
            let mut code = String::new();
            i += 1;
            while i < lines.len() && !lines[i].trim_start().starts_with("```") {
                code.push_str(lines[i]);
                code.push('\n');
                i += 1;
            }
            i += 1; // the closing fence, or the end of the input
            let escaped = escape_html(code.trim_end_matches('\n'));
            if lang.is_empty() {
                out.push_str(&format!("<pre>{escaped}</pre>\n"));
            } else {
                out.push_str(&format!(
                    "<pre><code class=\"language-{}\">{escaped}</code></pre>\n",
                    escape_html(lang)
                ));
            }
            continue;
        }

        // Pipe table: header, separator, then rows until a blank line.
        if line.contains('|') && i + 1 < lines.len() && is_table_separator(lines[i + 1]) {
            let mut rows = vec![split_cells(line)];
            i += 2;
            while i < lines.len() && !lines[i].trim().is_empty() && lines[i].contains('|') {
                rows.push(split_cells(lines[i]));
                i += 1;
            }
            out.push_str("<pre>");
            out.push_str(&escape_html(&render_table(&rows)));
            out.push_str("</pre>\n");
            continue;
        }

        // Headings become bold: Telegram HTML has no heading tag.
        let hashes = trimmed.chars().take_while(|c| *c == '#').count();
        if (1..=6).contains(&hashes) && trimmed[hashes..].starts_with(' ') {
            let content = trimmed[hashes..].trim();
            out.push_str(&format!("<b>{}</b>\n", render_inline(content)));
            i += 1;
            continue;
        }

        if trimmed == "---" || trimmed == "***" || trimmed == "___" {
            out.push_str("────────\n");
            i += 1;
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("> ") {
            out.push_str(&format!("▎ {}\n", render_inline(rest)));
            i += 1;
            continue;
        }

        if let Some(rest) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
        {
            out.push_str(&format!("• {}\n", render_inline(rest)));
            i += 1;
            continue;
        }

        out.push_str(&render_inline(line));
        out.push('\n');
        i += 1;
    }
    out.trim_end().to_string()
}

/// Inline formatting inside one line: code spans first, then links, then bold,
/// then emphasis. Escaping happens as the text is copied, so literal markup in
/// the agent's answer stays literal.
fn render_inline(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '&' => {
                out.push_str("&amp;");
                i += 1;
            }
            '<' => {
                out.push_str("&lt;");
                i += 1;
            }
            '>' => {
                out.push_str("&gt;");
                i += 1;
            }
            '`' => {
                if let Some(end) = find_char(&chars, i + 1, '`') {
                    let code: String = chars[i + 1..end].iter().collect();
                    out.push_str("<code>");
                    out.push_str(&escape_html(&code));
                    out.push_str("</code>");
                    i = end + 1;
                } else {
                    out.push('`');
                    i += 1;
                }
            }
            '[' => {
                if let Some((label, url, next)) = parse_link(&chars, i) {
                    out.push_str("<a href=\"");
                    out.push_str(&escape_attr(&url));
                    out.push_str("\">");
                    out.push_str(&render_inline(&label));
                    out.push_str("</a>");
                    i = next;
                } else {
                    out.push('[');
                    i += 1;
                }
            }
            '*' | '_' => {
                if c == '*' && chars.get(i + 1) == Some(&'*') {
                    if let Some(end) = find_double(&chars, i + 2) {
                        let inner: String = chars[i + 2..end].iter().collect();
                        out.push_str("<b>");
                        out.push_str(&render_inline(&inner));
                        out.push_str("</b>");
                        i = end + 2;
                        continue;
                    }
                }
                // Emphasis only at word boundaries, so `snake_case` and
                // `a*b` stay untouched.
                if can_open_emphasis(&chars, i) {
                    if let Some(end) = find_emphasis_close(&chars, i + 1, c) {
                        let inner: String = chars[i + 1..end].iter().collect();
                        out.push_str("<i>");
                        out.push_str(&render_inline(&inner));
                        out.push_str("</i>");
                        i = end + 1;
                        continue;
                    }
                }
                out.push(c);
                i += 1;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

fn find_char(chars: &[char], start: usize, needle: char) -> Option<usize> {
    chars[start..]
        .iter()
        .position(|&c| c == needle)
        .map(|p| start + p)
}

fn find_double(chars: &[char], start: usize) -> Option<usize> {
    let mut j = start;
    while j + 1 < chars.len() {
        if chars[j] == '*' && chars[j + 1] == '*' {
            return Some(j);
        }
        j += 1;
    }
    None
}

fn is_emphasis_boundary(c: char) -> bool {
    c.is_whitespace() || "([{<\"'“‘,;:!?)]}>.-".contains(c)
}

fn can_open_emphasis(chars: &[char], i: usize) -> bool {
    i == 0 || is_emphasis_boundary(chars[i - 1])
}

fn find_emphasis_close(chars: &[char], start: usize, marker: char) -> Option<usize> {
    let mut j = start;
    while j < chars.len() {
        if chars[j] == marker && j > start {
            let after_ok = chars
                .get(j + 1)
                .map(|c| is_emphasis_boundary(*c))
                .unwrap_or(true);
            if after_ok {
                return Some(j);
            }
        }
        j += 1;
    }
    None
}

fn parse_link(chars: &[char], i: usize) -> Option<(String, String, usize)> {
    let close = find_char(chars, i + 1, ']')?;
    if chars.get(close + 1) != Some(&'(') {
        return None;
    }
    let end = find_char(chars, close + 2, ')')?;
    let url: String = chars[close + 2..end].iter().collect();
    let url = url.trim().to_string();
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return None;
    }
    let label: String = chars[i + 1..close].iter().collect();
    Some((label, url, end + 1))
}

fn split_cells(line: &str) -> Vec<String> {
    let trimmed = line.trim();
    let trimmed = trimmed.strip_prefix('|').unwrap_or(trimmed);
    let trimmed = trimmed.strip_suffix('|').unwrap_or(trimmed);
    trimmed.split('|').map(|c| c.trim().to_string()).collect()
}

fn is_table_separator(line: &str) -> bool {
    let cells = split_cells(line);
    !cells.is_empty()
        && cells
            .iter()
            .all(|c| !c.is_empty() && c.chars().all(|ch| ch == '-' || ch == ':'))
}

/// Align columns the way musebot's code-block "tables" do. Wide characters
/// (CJK, emoji) count as two cells so the columns do not drift.
fn render_table(rows: &[Vec<String>]) -> String {
    let cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    let mut widths = vec![0usize; cols];
    for row in rows {
        for (c, cell) in row.iter().enumerate() {
            widths[c] = widths[c].max(cell_width(cell));
        }
    }
    let mut out = String::new();
    for (r, row) in rows.iter().enumerate() {
        for (c, width) in widths.iter().enumerate() {
            let cell = row.get(c).map(String::as_str).unwrap_or("");
            out.push_str(cell);
            if c + 1 < cols {
                let pad = width.saturating_sub(cell_width(cell)) + 2;
                out.push_str(&" ".repeat(pad));
            }
        }
        if r + 1 < rows.len() {
            out.push('\n');
        }
    }
    out
}

fn cell_width(text: &str) -> usize {
    text.chars().map(char_width).sum()
}

fn char_width(c: char) -> usize {
    let cp = c as u32;
    let wide = (0x1100..=0x115F).contains(&cp)
        || (0x2E80..=0xA4CF).contains(&cp)
        || (0xAC00..=0xD7A3).contains(&cp)
        || (0xF900..=0xFAFF).contains(&cp)
        || (0xFE30..=0xFE4F).contains(&cp)
        || (0xFF00..=0xFF60).contains(&cp)
        || (0xFFE0..=0xFFE6).contains(&cp)
        || (0x1F300..=0x1FAFF).contains(&cp)
        || (0x20000..=0x3FFFD).contains(&cp);
    if wide {
        2
    } else {
        1
    }
}

fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
    out
}

fn escape_attr(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

/// True when Telegram rejected the message because of the HTML itself. The
/// fallback is to resend the same chunk as plain text instead of dropping it.
fn entity_error(err: &anyhow::Error) -> bool {
    let msg = format!("{err:#}").to_ascii_lowercase();
    msg.contains("can't parse entities")
        || msg.contains("can't find end tag")
        || msg.contains("unsupported start tag")
}

/// Group whole lines into chunks under `raw_limit` so the renderer never has
/// to make a tag that spans two messages. An oversized single line falls back
/// to the character splitter.
fn split_markdown_chunks(text: &str, raw_limit: usize) -> Vec<String> {
    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut units = 0usize;
    for line in text.lines() {
        let line_units = line.encode_utf16().count();
        if line_units > raw_limit {
            if !current.is_empty() {
                chunks.push(std::mem::take(&mut current));
                units = 0;
            }
            chunks.extend(split_chunks(line, raw_limit));
            continue;
        }
        if units > 0 && units + 1 + line_units > raw_limit {
            chunks.push(std::mem::take(&mut current));
            units = 0;
        }
        if units > 0 {
            current.push('\n');
            units += 1;
        }
        current.push_str(line);
        units += line_units;
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    if chunks.is_empty() {
        chunks.push(String::new());
    }
    chunks
}

// ---------------------------------------------------------------------------
// Bus helpers (one-shot connections, plus the long-lived tail)

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
        let online = a.get("online").and_then(|o| o.as_bool()).unwrap_or(false);
        lines.push(format!(
            "{:<26} {:<11} {:<18} {:<8} {}",
            a.get("id").and_then(|v| v.as_str()).unwrap_or("?"),
            a.get("harness").and_then(|v| v.as_str()).unwrap_or("?"),
            a.get("model").and_then(|v| v.as_str()).unwrap_or("-"),
            a.get("role").and_then(|v| v.as_str()).unwrap_or("?"),
            if online { "online" } else { "offline" },
        ));
    }
    Ok(lines.join("\n"))
}

/// How many *other* agents currently hold a live connection in this room.
///
/// Used in the send ack so a receipt does not read as a reply: with nobody
/// online, the human should see that instead of an unexplained `seq`.
async fn other_agents_online(
    transport: &Transport,
    port: u16,
    room: &str,
    exclude: &str,
) -> Result<usize> {
    let mut c = transport.connect(port).await?;
    c.hello(None, None).await?;
    let reply = c.op(json!({"op": "who", "room": room})).await?;
    c.bye().await;
    Ok(reply
        .get("agents")
        .and_then(|a| a.as_array())
        .map(|agents| {
            agents
                .iter()
                .filter(|a| {
                    a.get("online").and_then(|o| o.as_bool()).unwrap_or(false)
                        && a.get("id").and_then(|i| i.as_str()) != Some(exclude)
                })
                .count()
        })
        .unwrap_or(0))
}

/// The short suffix on a send ack. Empty when the probe failed, so a broker
/// hiccup never turns a delivered message into a visible failure.
async fn delivery_note(transport: &Transport, port: u16, room: &str, agent_id: &str) -> String {
    match other_agents_online(transport, port, room, agent_id).await {
        Ok(0) => "no agent online".into(),
        Ok(1) => "1 agent online".into(),
        Ok(n) => format!("{n} agents online"),
        Err(_) => String::new(),
    }
}

fn ack_line(kind: &str, seq: i64, note: &str) -> String {
    if note.is_empty() {
        format!("→ {kind} seq {seq}")
    } else {
        format!("→ {kind} seq {seq} · {note}")
    }
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
        let note = delivery_note(transport, port, room, &agent.id).await;
        tg.send_message(chat_id, &ack_line("chat", seq, &note))
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
            let note = delivery_note(transport, port, room, &agent.id).await;
            tg.send_message(chat_id, &ack_line("chat", seq, &note))
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
    let had_chat = state.chat_id;
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
    let newly_bound = state.chat_id != had_chat;
    if newly_bound {
        let text = if chat_id < 0 {
            format!(
                "agent-bus connected to this group.\nRoom: {room}\nHumans write as {}.\nAgents answer when mentioned by name; send /who to list them. A message with no mention reaches the default agent.",
                agent.id
            )
        } else {
            format!(
                "agent-bus connected.\nRoom: {room}\nIdentity: {}\nSend /help for commands.",
                agent.id
            )
        };
        let _ = tg
            .send_message(chat_id, &text)
            .await
            .map_err(|e| eprintln!("telegram: greeting failed: {e:#}"));
    } else {
        eprintln!("telegram: bound to chat {chat_id}");
    }

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
                            // A group wake-word (`@bot`) is not part of the
                            // question; strip it before the bus sees the text.
                            let text = strip_self_mention(&text, username);
                            if text.is_empty() {
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
    fn markdown_renders_to_telegram_html() {
        let html = render_telegram_html(
            "## HK weather\n\n**Sat** is *wet* with `rain`.\n\n[Source](https://hko.gov.hk)",
        );
        assert!(html.contains("<b>HK weather</b>"), "{html}");
        assert!(html.contains("<b>Sat</b>"), "{html}");
        assert!(html.contains("<i>wet</i>"), "{html}");
        assert!(html.contains("<code>rain</code>"), "{html}");
        assert!(
            html.contains("<a href=\"https://hko.gov.hk\">Source</a>"),
            "{html}"
        );
    }

    #[test]
    fn html_is_escaped_before_it_reaches_telegram() {
        assert_eq!(
            render_telegram_html("a < b & c > d"),
            "a &lt; b &amp; c &gt; d"
        );
    }

    #[test]
    fn snake_case_is_not_emphasis() {
        let html = render_inline("file_name and *really*");
        assert!(html.contains("file_name"), "{html}");
        assert!(html.contains("<i>really</i>"), "{html}");
    }

    #[test]
    fn pipe_tables_render_as_an_aligned_block() {
        let html = render_telegram_html("| Day | Temp |\n| --- | --- |\n| Sat | 24 |\n| Sun | 22 |");
        assert!(html.starts_with("<pre>"), "{html}");
        assert!(html.contains("Day  Temp"), "{html}");
        assert!(html.contains("Sat  24"), "{html}");
        assert!(html.contains("Sun  22"), "{html}");
    }

    #[test]
    fn fenced_code_is_not_reformatted() {
        let html = render_telegram_html("```rust\nlet x = 1 < 2;\n```");
        assert_eq!(
            html,
            "<pre><code class=\"language-rust\">let x = 1 &lt; 2;</code></pre>"
        );
    }

    #[test]
    fn markdown_chunks_split_on_lines_under_the_limit() {
        let text = (0..40)
            .map(|i| format!("line {i} padding padding"))
            .collect::<Vec<_>>()
            .join("\n");
        let chunks = split_markdown_chunks(&text, 120);
        assert!(chunks.len() > 1, "{chunks:?}");
        for chunk in &chunks {
            assert!(
                chunk.encode_utf16().count() <= 120,
                "chunk over limit: {chunk:?}"
            );
            assert!(!chunk.starts_with('\n') && !chunk.ends_with('\n'));
        }
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

    #[test]
    fn acks_say_whether_anyone_is_listening() {
        assert_eq!(ack_line("chat", 7, ""), "→ chat seq 7");
        assert_eq!(
            ack_line("chat", 7, "no agent online"),
            "→ chat seq 7 · no agent online"
        );
        assert_eq!(
            ack_line("chat", 7, "2 agents online"),
            "→ chat seq 7 · 2 agents online"
        );
    }

    #[test]
    fn bot_mention_is_stripped_before_the_bus() {
        assert_eq!(
            strip_self_mention("@opencodetototofubot weather in hk", "opencodetototofubot"),
            "weather in hk"
        );
        assert_eq!(
            strip_self_mention("@Assistant look", "opencodetototofubot"),
            "@Assistant look"
        );
        assert_eq!(
            strip_self_mention("hey @opencodetototofubot, hi", "opencodetototofubot"),
            "hey , hi"
        );
        // A longer username that merely starts the same must not be cut.
        assert_eq!(
            strip_self_mention("@opencodetototofubot2 hi", "opencodetototofubot"),
            "@opencodetototofubot2 hi"
        );
    }
}
