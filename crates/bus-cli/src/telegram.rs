//! Telegram bridge: Bot API client + Markdown -> Telegram HTML rendering.
//!
//! Two halves, kept separate on purpose:
//! - [`render_markdown`] / [`chunk_html`]: pure functions (issue #12).
//!   No network, fully unit-tested. Telegram has no real tables and no
//!   colours; tables are faked with aligned monospace `<pre>` blocks.
//! - [`TgClient`]: the Bot API over HTTPS (`getUpdates`, `sendMessage`,
//!   `sendChatAction`). One token, one poller: Telegram delivers each
//!   update to a single `getUpdates` consumer, so a second bridge with
//!   the same token would steal the first one's messages. Use a
//!   dedicated bot token for the bridge — do NOT share the Perihel
//!   bot's token while `perihel-telegram` is polling with it.
//!
//! The bridge loop itself lives in `run_bridge` below: it posts human
//! messages from Telegram into a bus room as `chat` envelopes from
//! `human@telegram`, tails the room for agent replies, and sends them
//! back rendered as HTML. It runs next to `busd` (the bus is
//! localhost-only), on the same machine — usually Windows.

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Rendering (issue #12): Markdown -> Telegram HTML, then chunking.
// ---------------------------------------------------------------------------

/// Telegram's per-message cap. Chunks stay under it with margin.
pub const TG_MAX_MESSAGE: usize = 4096;
const CHUNK_TARGET: usize = 4000;

/// Escape the three characters Telegram HTML treats as markup.
fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
    out
}

/// Inline formatting on already-escaped text: `` `code` ``, links,
/// `**bold**`, `*italic*` / `_italic_`. A tiny scanner, not a regex:
/// code spans are consumed first so markup inside them stays literal.
fn render_inline(escaped: &str) -> String {
    let chars: Vec<char> = escaped.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        // `code`
        if chars[i] == '`' {
            if let Some(end) = chars[i + 1..].iter().position(|&c| c == '`') {
                out.push_str("<code>");
                out.extend(&chars[i + 1..i + 1 + end]);
                out.push_str("</code>");
                i += end + 2;
                continue;
            }
        }
        // [label](url)
        if chars[i] == '[' {
            if let Some(close) = chars[i + 1..].iter().position(|&c| c == ']') {
                let after = i + 1 + close + 1;
                if after < chars.len() && chars[after] == '(' {
                    if let Some(end) = chars[after + 1..].iter().position(|&c| c == ')') {
                        let label: String = chars[i + 1..i + 1 + close].iter().collect();
                        let url: String =
                            chars[after + 1..after + 1 + end].iter().collect();
                        out.push_str(&format!("<a href=\"{url}\">{}</a>", render_inline(&label)));
                        i = after + 1 + end + 1;
                        continue;
                    }
                }
            }
        }
        // **bold**
        if chars[i] == '*' && i + 1 < chars.len() && chars[i + 1] == '*' {
            if let Some(end) = chars[i + 2..]
                .windows(2)
                .position(|w| w == ['*', '*'])
            {
                let inner: String = chars[i + 2..i + 2 + end].iter().collect();
                out.push_str(&format!("<b>{}</b>", render_inline(&inner)));
                i += end + 4;
                continue;
            }
        }
        // *italic* or _italic_
        if chars[i] == '*' || chars[i] == '_' {
            let mark = chars[i];
            if let Some(end) = chars[i + 1..].iter().position(|&c| c == mark) {
                let inner: String = chars[i + 1..i + 1 + end].iter().collect();
                // A lone * between words (e.g. 2*3) has no closing pair on
                // the same run; the position search above already requires
                // a real closer, so this is safe.
                out.push_str(&format!("<i>{}</i>", render_inline(&inner)));
                i += end + 2;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Render one pipe table (consecutive `| a | b |` lines) as aligned
/// monospace text. The `|---|---|` separator row is dropped.
fn render_table(lines: &[&str]) -> String {
    let mut rows: Vec<Vec<String>> = Vec::new();
    for line in lines {
        let cells: Vec<String> = line
            .trim()
            .trim_matches('|')
            .split('|')
            .map(|c| c.trim().to_string())
            .collect();
        if cells.iter().all(|c| c.chars().all(|ch| ch == '-' || ch == ':')) {
            continue; // separator row
        }
        rows.push(cells);
    }
    if rows.is_empty() {
        return String::new();
    }
    let cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    let mut widths = vec![0usize; cols];
    for row in &rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }
    let mut out = String::new();
    for row in &rows {
        for i in 0..cols {
            let cell = row.get(i).map(|s| s.as_str()).unwrap_or("");
            out.push_str(cell);
            for _ in cell.chars().count()..widths[i] {
                out.push(' ');
            }
            if i + 1 < cols {
                out.push_str("  ");
            }
        }
        out.push('\n');
    }
    format!("<pre>{}</pre>", escape_html(out.trim_end()))
}

/// Render a whole Markdown message to Telegram HTML. Headings become
/// bold, fenced code becomes `<pre>`, tables become aligned `<pre>`,
/// everything else is a paragraph with inline formatting.
pub fn render_markdown(md: &str) -> String {
    let lines: Vec<&str> = md.lines().collect();
    let mut blocks: Vec<String> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if line.trim().is_empty() {
            i += 1;
            continue;
        }
        // Fenced code block.
        if line.trim_start().starts_with("```") {
            let mut code = String::new();
            i += 1;
            while i < lines.len() && !lines[i].trim_start().starts_with("```") {
                code.push_str(lines[i]);
                code.push('\n');
                i += 1;
            }
            i += 1; // closing fence (or EOF)
            blocks.push(format!("<pre>{}</pre>", escape_html(code.trim_end())));
            continue;
        }
        // Pipe table: run of lines starting with '|'.
        if line.trim_start().starts_with('|') {
            let mut table = vec![line];
            i += 1;
            while i < lines.len() && lines[i].trim_start().starts_with('|') {
                table.push(lines[i]);
                i += 1;
            }
            blocks.push(render_table(&table));
            continue;
        }
        // Heading: '# ...' -> bold.
        let hashes = line.trim_start().chars().take_while(|&c| c == '#').count();
        if hashes > 0 && hashes < 7 && line.trim_start()[hashes..].starts_with(' ') {
            let text = line.trim_start()[hashes..].trim();
            blocks.push(format!("<b>{}</b>", render_inline(&escape_html(text))));
            i += 1;
            continue;
        }
        // Paragraph: run of lines until a blank line or a block start.
        let mut para = String::new();
        while i < lines.len() {
            let l = lines[i];
            if l.trim().is_empty()
                || l.trim_start().starts_with("```")
                || l.trim_start().starts_with('|')
            {
                break;
            }
            let trimmed = l.trim_start();
            let h = trimmed.chars().take_while(|&c| c == '#').count();
            if h > 0 && h < 7 && trimmed[h..].starts_with(' ') {
                break;
            }
            if !para.is_empty() {
                para.push('\n');
            }
            para.push_str(l);
            i += 1;
        }
        blocks.push(render_inline(&escape_html(&para)));
    }
    blocks.retain(|b| !b.is_empty());
    blocks.join("\n\n")
}

/// Split rendered HTML into messages of at most [`TG_MAX_MESSAGE`]
/// characters, on block boundaries, so tags are never split. A single
/// block that is still too long is split on newlines; a `<pre>` block
/// is re-wrapped per piece so every chunk stays balanced.
pub fn chunk_html(html: &str) -> Vec<String> {
    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();
    let push_block = |current: &mut String, chunks: &mut Vec<String>, block: &str| {
        if current.is_empty() {
            if block.chars().count() <= CHUNK_TARGET {
                *current = block.to_string();
            } else {
                split_long_block(block, chunks);
            }
        } else if current.chars().count() + 2 + block.chars().count() <= CHUNK_TARGET {
            current.push_str("\n\n");
            current.push_str(block);
        } else {
            chunks.push(std::mem::take(current));
            if block.chars().count() <= CHUNK_TARGET {
                *current = block.to_string();
            } else {
                split_long_block(block, chunks);
            }
        }
    };
    for block in html.split("\n\n") {
        if block.is_empty() {
            continue;
        }
        push_block(&mut current, &mut chunks, block);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    if chunks.is_empty() {
        chunks.push(String::new());
    }
    chunks
}

fn split_long_block(block: &str, chunks: &mut Vec<String>) {
    // A <pre>…</pre> block: split its inner lines across fresh <pre> tags.
    if let Some(inner) = block
        .strip_prefix("<pre>")
        .and_then(|b| b.strip_suffix("</pre>"))
    {
        let mut current = String::from("<pre>");
        for line in inner.split('\n') {
            if current.chars().count() + line.chars().count() + 1 > CHUNK_TARGET
                && current.len() > "<pre>".len()
            {
                current.push_str("</pre>");
                chunks.push(std::mem::take(&mut current));
                current = String::from("<pre>");
            }
            if !current.ends_with("<pre>") {
                current.push('\n');
            }
            current.push_str(line);
        }
        current.push_str("</pre>");
        chunks.push(current);
        return;
    }
    // Anything else: hard-split on lines; a line longer than the
    // target is split by characters. Content is preserved exactly:
    // concatenating the chunks gives back the block.
    let mut current = String::new();
    for (i, line) in block.split('\n').enumerate() {
        if i > 0 {
            if current.chars().count() + 1 > CHUNK_TARGET {
                chunks.push(std::mem::take(&mut current));
            } else {
                current.push('\n');
            }
        }
        let mut rest = line;
        while !rest.is_empty() {
            let room = CHUNK_TARGET - current.chars().count();
            if room == 0 {
                chunks.push(std::mem::take(&mut current));
                continue;
            }
            let take: String = rest.chars().take(room).collect();
            let n = take.len();
            current.push_str(&take);
            rest = &rest[n..];
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
}

// ---------------------------------------------------------------------------
// Bot API client.
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct TgResponse<T> {
    ok: bool,
    result: Option<T>,
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct TgUpdate {
    pub update_id: i64,
    pub message: Option<TgMessage>,
}

#[derive(Debug, Deserialize)]
pub struct TgMessage {
    pub message_id: i64,
    pub chat: TgChat,
    pub from: Option<TgUser>,
    pub text: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct TgChat {
    pub id: i64,
}

#[derive(Debug, Deserialize)]
pub struct TgUser {
    pub is_bot: bool,
    pub first_name: String,
}

pub struct TgClient {
    base: String,
    token: String,
    http: reqwest::Client,
}

impl TgClient {
    pub fn new(base: &str, token: &str) -> Self {
        TgClient {
            base: base.trim_end_matches('/').to_string(),
            token: token.to_string(),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(40))
                .build()
                .expect("reqwest client builds"),
        }
    }

    fn url(&self, method: &str) -> String {
        // The token is part of the URL path by Telegram's design; it is
        // never printed or logged anywhere in this module.
        format!("{}/bot{}/{}", self.base, self.token, method)
    }

    async fn post<T: for<'de> Deserialize<'de>>(
        &self,
        method: &str,
        body: serde_json::Value,
    ) -> Result<T> {
        let resp = self
            .http
            .post(self.url(method))
            .json(&body)
            .send()
            .await
            .with_context(|| format!("telegram {method} request failed"))?;
        let parsed: TgResponse<T> = resp
            .json()
            .await
            .with_context(|| format!("telegram {method} returned bad JSON"))?;
        if !parsed.ok {
            return Err(anyhow!(
                "telegram {method} rejected: {}",
                parsed.description.unwrap_or_else(|| "unknown error".into())
            ));
        }
        parsed
            .result
            .ok_or_else(|| anyhow!("telegram {method} returned ok with no result"))
    }

    /// Long-poll for updates. `offset` skips everything before it;
    /// pass `None` on the first call to get the latest backlog.
    pub async fn get_updates(
        &self,
        offset: Option<i64>,
        timeout_secs: u64,
    ) -> Result<Vec<TgUpdate>> {
        let mut body = serde_json::json!({ "timeout": timeout_secs });
        if let Some(o) = offset {
            body["offset"] = o.into();
        }
        self.post("getUpdates", body).await
    }

    pub async fn send_message(
        &self,
        chat_id: i64,
        html: &str,
        reply_to: Option<i64>,
    ) -> Result<()> {
        let mut body = serde_json::json!({
            "chat_id": chat_id,
            "text": html,
            "parse_mode": "HTML",
            // No link preview card: the bridge forwards agent text, and a
            // preview for every URL in it is noise, not signal.
            "disable_web_page_preview": true,
        });
        if let Some(id) = reply_to {
            body["reply_to_message_id"] = id.into();
        }
        let _: serde_json::Value = self.post("sendMessage", body).await?;
        Ok(())
    }

    pub async fn send_chat_action(&self, chat_id: i64) -> Result<()> {
        let _: serde_json::Value = self
            .post(
                "sendChatAction",
                serde_json::json!({ "chat_id": chat_id, "action": "typing" }),
            )
            .await?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Bridge loop (issue #8).
// ---------------------------------------------------------------------------

use crate::{make_agent, read_token, token_path, Client, Transport};

/// Everything the bridge needs. The token comes from `--token`,
/// `TELEGRAM_BOT_TOKEN`, or `--token-file` (first one wins); it is
/// never printed. `api_base` exists so tests can point the client at
/// a mock server; production is always https://api.telegram.org.
pub struct BridgeConfig {
    pub room: String,
    pub chat_id: i64,
    pub token: String,
    pub api_base: String,
    pub bridge_agent: String,
    pub human_agent: String,
    /// Long-poll seconds per getUpdates call (Telegram caps at 50).
    pub poll_secs: u64,
    /// Seconds to wait for an agent reply before the "no reply" ack.
    pub reply_timeout_secs: u64,
}

/// Register an agent id if needed and return a connected, hello'd
/// bus client for it. Mirrors the `bus register` flow (same token
/// file, same 0600 permissions on unix).
async fn connect_as(
    transport: &Transport,
    port: u16,
    id: &str,
) -> Result<Client> {
    let agent = make_agent(id, "telegram-bridge", None, &[], None);
    let mut c = transport.connect(port).await?;
    let existing = read_token(id);
    if existing.is_none() {
        // First hello is anonymous so we can register.
        c.hello(None, None).await?;
        let reply = c
            .op(serde_json::json!({"op": "register", "agent": agent}))
            .await?;
        c.bye().await;
        if let Some(token) = reply.get("token").and_then(|t| t.as_str()) {
            let path = token_path(id);
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&path, token)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(
                    &path,
                    std::fs::Permissions::from_mode(0o600),
                );
            }
        }
        let mut c2 = transport.connect(port).await?;
        let token = read_token(id);
        c2.hello(Some(&agent), token.as_deref()).await?;
        return Ok(c2);
    }
    c.hello(Some(&agent), existing.as_deref()).await?;
    Ok(c)
}

/// Run the bridge until Ctrl-C. One process, one token, one poller.
pub async fn run_bridge(cfg: BridgeConfig, transport: Transport, port: u16) -> Result<()> {
    let tg = TgClient::new(&cfg.api_base, &cfg.token);
    // Two bus connections: the bridge agent reads (who/history) and the
    // human agent posts. The bus takes the sender from hello, so one
    // connection cannot post as both.
    let mut bus = connect_as(&transport, port, &cfg.bridge_agent).await?;
    let mut human = connect_as(&transport, port, &cfg.human_agent).await?;
    bus.op(serde_json::json!({"op": "join", "room": cfg.room})).await?;
    human
        .op(serde_json::json!({"op": "join", "room": cfg.room}))
        .await?;

    // Skip the backlog on start: without this a restart re-posts every
    // unconfirmed update Telegram still holds.
    let mut offset: Option<i64> = match tg.get_updates(None, 0).await {
        Ok(updates) => updates.iter().map(|u| u.update_id).max().map(|m| m + 1),
        Err(e) => {
            eprintln!("bus telegram: backlog skip failed ({e:#}); starting fresh");
            None
        }
    };

    println!(
        "bus telegram: bridging chat {} <-> room {} (poll {}s, reply window {}s)",
        cfg.chat_id, cfg.room, cfg.poll_secs, cfg.reply_timeout_secs
    );
    loop {
        let updates = match tg.get_updates(offset, cfg.poll_secs).await {
            Ok(u) => u,
            Err(e) => {
                eprintln!("bus telegram: getUpdates failed ({e:#}); retrying");
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };
        for update in updates {
            offset = Some(update.update_id + 1);
            let Some(msg) = update.message else { continue };
            if msg.chat.id != cfg.chat_id {
                continue; // another chat talking to the same bot
            }
            if msg.from.as_ref().map(|f| f.is_bot).unwrap_or(false) {
                continue; // never echo another bot (and never ourselves)
            }
            let Some(text) = msg.text.as_ref().filter(|t| !t.trim().is_empty()) else {
                continue;
            };
            if let Err(e) =
                handle_message(&tg, &mut bus, &mut human, &cfg, &msg, &text).await
            {
                eprintln!("bus telegram: message {} failed ({e:#})", msg.message_id);
            }
        }
    }
}

async fn handle_message(
    tg: &TgClient,
    bus: &mut Client,
    human: &mut Client,
    cfg: &BridgeConfig,
    msg: &TgMessage,
    text: &str,
) -> Result<()> {
    let _ = tg.send_chat_action(msg.chat.id).await; // best effort
    // Post the human message as a chat envelope from human@telegram.
    let reply = human
        .op(serde_json::json!({
            "op": "send", "room": cfg.room, "kind": "chat", "text": text,
        }))
        .await?;
    let seq = reply.get("seq").and_then(|s| s.as_i64()).unwrap_or(0);

    // Is anyone else in the room to answer? (#8 acceptance: if not,
    // the ack says so plainly instead of looking like a reply.)
    let who = bus
        .op(serde_json::json!({"op": "who", "room": cfg.room}))
        .await?;
    let listeners = who
        .get("agents")
        .and_then(|a| a.as_array())
        .map(|agents| {
            agents
                .iter()
                .filter(|a| {
                    let id = a.get("id").and_then(|i| i.as_str()).unwrap_or("");
                    id != cfg.bridge_agent && id != cfg.human_agent
                })
                .count()
        })
        .unwrap_or(0);
    if listeners == 0 {
        let ack = format!(
            "Delivered to {} (seq {seq}) — no agent is listening in that room right now.",
            cfg.room
        );
        tg.send_message(msg.chat.id, &escape_html(&ack), Some(msg.message_id))
            .await?;
        return Ok(());
    }

    // Wait for agent replies, forwarding each one as it arrives.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(cfg.reply_timeout_secs);
    let mut since = seq;
    let mut replied = false;
    loop {
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        let hist = bus
            .op(serde_json::json!({
                "op": "history", "room": cfg.room, "since": since, "limit": 50,
            }))
            .await?;
        let mut advanced = false;
        if let Some(msgs) = hist.get("messages").and_then(|m| m.as_array()) {
            for env in msgs {
                let env_seq = env.get("seq").and_then(|s| s.as_i64()).unwrap_or(0);
                since = since.max(env_seq);
                advanced = true;
                let from = env
                    .get("from")
                    .and_then(|f| f.get("id"))
                    .and_then(|i| i.as_str())
                    .unwrap_or("?");
                if from == cfg.bridge_agent || from == cfg.human_agent {
                    continue; // never echo our own traffic
                }
                if env.get("kind").and_then(|k| k.as_str()) != Some("chat") {
                    continue;
                }
                let Some(body) = env.get("text").and_then(|t| t.as_str()) else {
                    continue;
                };
                // Replies render with the agent as a bold heading (#12).
                let rendered = render_markdown(&format!("**{from}**\n\n{body}"));
                for chunk in chunk_html(&rendered) {
                    tg.send_message(msg.chat.id, &chunk, Some(msg.message_id))
                        .await?;
                }
                replied = true;
            }
        }
        if replied && !advanced {
            // A reply already went out and nothing new arrived; keep
            // waiting for more until the deadline — agents often answer
            // in a burst. (The loop still ends at the deadline.)
        }
        tokio::time::sleep(Duration::from_millis(2000)).await;
    }
    if !replied {
        let ack = format!(
            "Delivered to {} (seq {seq}) — no agent replied within {} s.",
            cfg.room, cfg.reply_timeout_secs
        );
        tg.send_message(msg.chat.id, &escape_html(&ack), Some(msg.message_id))
            .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_headings_bold_italic_code_links() {
        let html = render_markdown(
            "## Weather\n\nIt is **sunny** and *warm* today, see `code` and [source](https://example.com/x?a=1&b=2).",
        );
        assert_eq!(
            html,
            "<b>Weather</b>\n\nIt is <b>sunny</b> and <i>warm</i> today, see <code>code</code> and <a href=\"https://example.com/x?a=1&amp;b=2\">source</a>."
        );
    }

    #[test]
    fn render_escapes_html_and_keeps_code_literal() {
        let html = render_markdown("a < b & c > d, use `x < y` here");
        assert!(html.contains("a &lt; b &amp; c &gt; d"));
        assert!(html.contains("<code>x &lt; y</code>"));
    }

    #[test]
    fn render_table_aligned_monospace() {
        let html = render_markdown("| name | cost |\n|---|---|\n| bread | 2 |\n| salad bowl | 10 |");
        assert!(html.starts_with("<pre>"));
        assert!(html.contains("name        cost"));
        assert!(html.contains("bread       2"));
        assert!(html.contains("salad bowl  10"));
        assert!(!html.contains("---"));
    }

    #[test]
    fn render_fenced_code_block() {
        let html = render_markdown("before\n\n```\nfn main() {\n    println!(\"hi\");\n}\n```\n\nafter");
        assert!(html.contains("<pre>fn main() {\n    println!(\"hi\");\n}</pre>"));
        assert!(html.contains("before"));
        assert!(html.contains("after"));
    }

    #[test]
    fn chunk_splits_on_blocks_and_never_a_tag() {
        let big = render_markdown(&"word ".repeat(3000));
        let chunks = chunk_html(&big);
        assert!(chunks.len() > 1);
        for c in &chunks {
            assert!(c.chars().count() <= TG_MAX_MESSAGE);
        }
        // Single block in, so concatenating the chunks is the input.
        assert_eq!(chunks.concat(), big);
    }

    #[test]
    fn chunk_long_pre_stays_balanced() {
        let long = format!("<pre>{}</pre>", "x\n".repeat(3000));
        let chunks = chunk_html(&long);
        assert!(chunks.len() > 1);
        for c in &chunks {
            assert!(c.starts_with("<pre>") && c.ends_with("</pre>"));
            assert!(c.chars().count() <= TG_MAX_MESSAGE);
        }
    }

    #[test]
    fn chunk_short_message_is_one_chunk() {
        let html = render_markdown("hi there");
        let chunks = chunk_html(&html);
        assert_eq!(chunks, vec!["hi there".to_string()]);
    }
}

#[cfg(test)]
mod bridge_tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;

    type Record = Arc<Mutex<Vec<(String, serde_json::Value)>>>;

    /// Minimal HTTP/1.1 server: records (path, JSON body) per POST and
    /// answers from a handler closure.
    async fn mock_telegram(
        listener: TcpListener,
        record: Record,
        delivered: Arc<Mutex<u32>>,
    ) {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                return;
            };
            let record = record.clone();
            let delivered = delivered.clone();
            tokio::spawn(async move {
                let (reader, mut writer) = socket.into_split();
                let mut reader = BufReader::new(reader);
                let mut line = String::new();
                let mut path = String::new();
                let mut content_length = 0usize;
                loop {
                    line.clear();
                    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    let trimmed = line.trim_end();
                    if trimmed.is_empty() {
                        break;
                    }
                    if let Some(p) = trimmed.strip_prefix("POST ") {
                        path = p.split(' ').next().unwrap_or("").to_string();
                    }
                    if let Some(v) = trimmed
                        .to_ascii_lowercase()
                        .strip_prefix("content-length: ")
                    {
                        content_length = v.trim().parse().unwrap_or(0);
                    }
                }
                let mut buf = vec![0u8; content_length];
                if content_length > 0 {
                    use tokio::io::AsyncReadExt;
                    let _ = reader.read_exact(&mut buf).await;
                }
                let body: serde_json::Value =
                    serde_json::from_slice(&buf).unwrap_or(serde_json::Value::Null);
                record.lock().unwrap().push((path.clone(), body));
                let result = if path.ends_with("/getUpdates") {
                    // Call 1 is the bridge's backlog skip (timeout=0) and
                    // must return nothing; call 2 delivers the update.
                    let mut n = delivered.lock().unwrap();
                    *n += 1;
                    if *n == 2 {
                        serde_json::json!([{
                            "update_id": 42,
                            "message": {
                                "message_id": 7,
                                "chat": {"id": -100},
                                "from": {"is_bot": false, "first_name": "Man To"},
                                "text": "hi, what is the weather?"
                            }
                        }])
                    } else {
                        serde_json::json!([])
                    }
                } else {
                    serde_json::json!({})
                };
                let payload = serde_json::json!({"ok": true, "result": result}).to_string();
                let _ = writer
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            payload.len(),
                            payload
                        )
                        .as_bytes(),
                    )
                    .await;
            });
        }
    }

    /// Minimal fake busd: answers hello/register/join/send/who/history
    /// with canned replies, and records every `send` body.
    async fn fake_bus(listener: TcpListener, sends: Record, history_calls: Arc<Mutex<u32>>) {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                return;
            };
            let sends = sends.clone();
            let history_calls = history_calls.clone();
            tokio::spawn(async move {
                let (reader, mut writer) = socket.into_split();
                let mut reader = BufReader::new(reader);
                let mut line = String::new();
                loop {
                    line.clear();
                    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    let msg: serde_json::Value =
                        serde_json::from_str(line.trim()).unwrap_or(serde_json::Value::Null);
                    let op = msg.get("op").and_then(|o| o.as_str()).unwrap_or("");
                    let reply = match op {
                        "hello" => serde_json::json!({"op": "welcome"}),
                        "register" => serde_json::json!({
                            "op": "registered", "role": "conversation", "token": "test-token"
                        }),
                        "join" => serde_json::json!({"op": "joined", "seq": 0}),
                        "send" => {
                            sends.lock().unwrap().push(("send".into(), msg.clone()));
                            serde_json::json!({"op": "sent", "seq": 7})
                        }
                        "who" => serde_json::json!({
                            "op": "who",
                            "agents": [{"id": "codex@demo", "status": "online"}],
                            "count": 1
                        }),
                        "history" => {
                            let mut n = history_calls.lock().unwrap();
                            *n += 1;
                            if *n == 1 {
                                serde_json::json!({
                                    "op": "history",
                                    "messages": [{
                                        "seq": 8, "kind": "chat",
                                        "from": {"id": "codex@demo"},
                                        "text": "**Weather**\n\nIt is sunny today."
                                    }],
                                    "count": 1
                                })
                            } else {
                                serde_json::json!({"op": "history", "messages": [], "count": 0})
                            }
                        }
                        "bye" => serde_json::json!({"op": "bye"}),
                        _ => serde_json::json!({"op": "error", "code": "op", "message": "unknown"}),
                    };
                    let out = format!("{}\n", serde_json::to_string(&reply).unwrap());
                    if writer.write_all(out.as_bytes()).await.is_err() {
                        return;
                    }
                    let _ = writer.flush().await;
                }
            });
        }
    }

    #[tokio::test]
    async fn bridge_forwards_human_message_and_agent_reply() {
        // Isolated token dir so the test never touches the real home.
        let home = std::env::temp_dir().join(format!(
            "agent-bus-bridge-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::env::set_var("AGENT_BUS_HOME", &home);

        let tg_listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let tg_port = tg_listener.local_addr().unwrap().port();
        let tg_record: Record = Arc::new(Mutex::new(Vec::new()));
        let delivered = Arc::new(Mutex::new(0u32));
        tokio::spawn(mock_telegram(tg_listener, tg_record.clone(), delivered));

        let bus_listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let bus_port = bus_listener.local_addr().unwrap().port();
        let sends: Record = Arc::new(Mutex::new(Vec::new()));
        let history_calls = Arc::new(Mutex::new(0u32));
        tokio::spawn(fake_bus(bus_listener, sends.clone(), history_calls));

        let cfg = BridgeConfig {
            room: "#general".into(),
            chat_id: -100,
            token: "test-token".into(),
            api_base: format!("http://127.0.0.1:{tg_port}"),
            bridge_agent: "telegram@bridge".into(),
            human_agent: "human@telegram".into(),
            poll_secs: 1,
            reply_timeout_secs: 10,
        };
        let transport = Transport {
            prefer: "tcp".into(),
            pipe: None,
        };
        let handle = tokio::spawn(run_bridge(cfg, transport, bus_port));

        // Wait until the mock Telegram server saw the forwarded reply.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
        let mut forwarded = false;
        while tokio::time::Instant::now() < deadline {
            let rec = tg_record.lock().unwrap().clone();
            if rec.iter().any(|(p, b)| {
                p.ends_with("/sendMessage")
                    && b.get("text")
                        .and_then(|t| t.as_str())
                        .map(|t| t.contains("sunny") && t.contains("<b>codex@demo</b>"))
                        .unwrap_or(false)
            }) {
                forwarded = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        handle.abort();

        // The human message reached the bus room as a chat envelope.
        let sends = sends.lock().unwrap();
        assert!(
            sends.iter().any(|(_, b)| b.get("kind").and_then(|k| k.as_str()) == Some("chat")
                && b.get("text").and_then(|t| t.as_str()) == Some("hi, what is the weather?")),
            "human message not posted to room: {sends:?}"
        );
        // The bridge typed, then forwarded the rendered agent reply.
        let rec = tg_record.lock().unwrap();
        assert!(
            rec.iter().any(|(p, _)| p.ends_with("/sendChatAction")),
            "no typing indicator sent: {rec:?}"
        );
        assert!(forwarded, "agent reply never forwarded to Telegram: {rec:?}");
        let _ = std::fs::remove_dir_all(home);
    }
}
