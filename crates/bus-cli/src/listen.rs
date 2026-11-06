//! `bus listen` — answer room traffic by running a command.
//!
//! This is the missing half of the Telegram bridge: the bridge is transport,
//! so something has to sit behind the bus door and reply. `listen` tails one
//! room, feeds each new envelope to a handler, and posts whatever the handler
//! prints back into the room.
//!
//! Contract with the handler:
//!
//! - the message text arrives on stdin (or the whole envelope as JSON when
//!   there is no text);
//! - context arrives as environment variables: `AGENT_BUS_ROOM`,
//!   `AGENT_BUS_SEQ`, `AGENT_BUS_FROM`, `AGENT_BUS_KIND`, `AGENT_BUS_ID`;
//! - stdout becomes the reply text, verbatim, trimmed;
//! - a non-zero exit or empty stdout sends nothing.
//!
//! The handler is a normal process, so any model, script or CLI can answer.

use crate::{ensure_registered, make_agent, read_token, Transport};
use anyhow::{anyhow, Result};
use proto::Envelope;
use serde_json::json;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

/// How much of a handler's stdout is forwarded. The Telegram bridge chunks at
/// 4096 UTF-16 units, so a larger cap only matters for other consumers; 16k
/// keeps a runaway dump from becoming an accidental log flood.
const MAX_REPLY_CHARS: usize = 16_000;

pub struct Config {
    pub room: String,
    pub exec: String,
    pub agent: String,
    pub model: Option<String>,
    pub kinds: Vec<String>,
    pub reply_kind: String,
    pub timeout_s: u64,
    pub once: bool,
}

pub async fn run(cfg: Config, port: u16, transport: Transport) -> Result<()> {
    let Config {
        room,
        exec,
        agent: agent_id,
        model,
        kinds,
        reply_kind,
        timeout_s,
        once,
    } = cfg;
    if !room.starts_with('#') || room.len() > 64 || room.contains(char::is_whitespace) {
        return Err(anyhow!("room must be # prefixed, no whitespace"));
    }
    if exec.trim().is_empty() {
        return Err(anyhow!("--exec must not be empty"));
    }

    let agent = make_agent(
        &agent_id,
        "bus-listen",
        model.as_deref(),
        &["respond".to_string()],
        None,
    );
    ensure_registered(&transport, port, &agent).await?;

    let token = read_token(&agent.id);
    let mut c = transport.connect(port).await?;
    c.hello(Some(&agent), token.as_deref()).await?;
    c.op(json!({"op": "join", "room": room})).await?;

    let mut tail = json!({"op": "tail", "rooms": [room.clone()]});
    if !kinds.is_empty() {
        tail["kinds"] = json!(kinds);
    }
    c.send_raw(&tail).await?;
    let ack = c.read_msg().await?;
    if ack.get("op").and_then(|o| o.as_str()) != Some("tail") {
        return Err(anyhow!("tail rejected: {ack}"));
    }
    eprintln!(
        "listen: {} answering {} in {} with `{}`",
        agent.id,
        if kinds.is_empty() {
            "all kinds".to_string()
        } else {
            kinds.join(",")
        },
        room,
        exec
    );

    let mut ctrl_c = std::pin::pin!(tokio::signal::ctrl_c());
    let mut line = String::new();
    loop {
        tokio::select! {
            _ = &mut ctrl_c => {
                eprintln!("listen: stopping");
                break;
            }
            n = c.read_line_dyn(&mut line) => {
                let n = n?;
                if n == 0 {
                    eprintln!("listen: broker closed the tail");
                    break;
                }
                let msg = match proto::decode_line(line.trim_end_matches('\n').as_bytes()) {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!("listen: bad frame: {e}");
                        line.clear();
                        continue;
                    }
                };
                line.clear();
                if msg.get("op").and_then(|o| o.as_str()) != Some("msg") {
                    continue;
                }
                let Some(value) = msg.get("envelope") else { continue };
                let env: Envelope = match serde_json::from_value(value.clone()) {
                    Ok(env) => env,
                    Err(e) => {
                        eprintln!("listen: bad envelope: {e}");
                        continue;
                    }
                };
                // Never answer ourselves: that is a feedback loop, not a reply.
                if env.from.id == agent.id {
                    continue;
                }
                match answer(&env, &exec, timeout_s, &reply_kind, &agent, &transport, port).await {
                    Ok(Some(seq)) => eprintln!(
                        "listen: [{}] {} -> reply seq {seq}",
                        env.seq.unwrap_or(0),
                        env.from.id
                    ),
                    Ok(None) => eprintln!(
                        "listen: [{}] {} -> (no reply)",
                        env.seq.unwrap_or(0),
                        env.from.id
                    ),
                    Err(e) => eprintln!(
                        "listen: [{}] {} -> handler error: {e:#}",
                        env.seq.unwrap_or(0),
                        env.from.id
                    ),
                }
                if once {
                    break;
                }
            }
        }
    }
    Ok(())
}

async fn answer(
    env: &Envelope,
    exec: &str,
    timeout_s: u64,
    reply_kind: &str,
    agent: &proto::Agent,
    transport: &Transport,
    port: u16,
) -> Result<Option<i64>> {
    let input = match &env.text {
        Some(t) => t.clone(),
        None => serde_json::to_string(env).unwrap_or_default(),
    };
    let output = tokio::time::timeout(
        Duration::from_secs(timeout_s),
        run_handler(exec, &input, env),
    )
    .await
    .map_err(|_| anyhow!("handler exceeded {timeout_s}s"))??;

    let reply = cap_reply(output.trim());
    if reply.is_empty() {
        return Ok(None);
    }

    let token = read_token(&agent.id);
    let mut c = transport.connect(port).await?;
    c.hello(Some(agent), token.as_deref()).await?;
    let mut v = json!({
        "op": "send",
        "room": env.room,
        "kind": reply_kind,
        "text": reply,
        "reply_to": env.id,
    });
    if env.thread.is_some() {
        v["thread"] = env.thread.clone().into();
    }
    let sent = c.op(v).await?;
    c.bye().await;
    Ok(sent.get("seq").and_then(|s| s.as_i64()))
}

/// Run `cmdline` through the platform shell with the envelope as context.
async fn run_handler(cmdline: &str, input: &str, env: &Envelope) -> Result<String> {
    let mut cmd = shell_command(cmdline);
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("AGENT_BUS_ROOM", &env.room)
        .env("AGENT_BUS_SEQ", env.seq.unwrap_or(0).to_string())
        .env("AGENT_BUS_FROM", &env.from.id)
        .env("AGENT_BUS_KIND", &env.kind)
        .env("AGENT_BUS_ID", &env.id)
        .spawn()?;

    if let Some(mut stdin) = child.stdin.take() {
        // Dropping the handle after the write closes the pipe, so a handler
        // that reads to EOF does not wait for a second message.
        stdin.write_all(input.as_bytes()).await?;
    }
    let out = child.wait_with_output().await?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(anyhow!(
            "handler exited with {}: {}",
            out.status,
            err.trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

#[cfg(windows)]
fn shell_command(cmdline: &str) -> tokio::process::Command {
    let mut c = tokio::process::Command::new("cmd");
    c.arg("/C").arg(cmdline);
    c
}

#[cfg(not(windows))]
fn shell_command(cmdline: &str) -> tokio::process::Command {
    let mut c = tokio::process::Command::new("sh");
    c.arg("-c").arg(cmdline);
    c
}

/// Trim to the forwarding cap. Truncation is visible, not silent: a handler
/// that dumps a novel should see that the tail was cut.
fn cap_reply(text: &str) -> String {
    if text.chars().count() <= MAX_REPLY_CHARS {
        return text.to_string();
    }
    let mut out: String = text.chars().take(MAX_REPLY_CHARS).collect();
    out.push_str("\n[truncated]");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Client;

    #[test]
    fn short_replies_pass_through() {
        assert_eq!(cap_reply("pong"), "pong");
    }

    #[test]
    fn long_replies_are_capped_and_marked() {
        let long = "x".repeat(MAX_REPLY_CHARS + 10);
        let capped = cap_reply(&long);
        assert!(capped.ends_with("[truncated]"));
        assert_eq!(capped.chars().count(), MAX_REPLY_CHARS + "\n[truncated]".chars().count());
    }

    #[test]
    fn capping_counts_characters_not_bytes() {
        // Multi-byte text must not be cut mid-codepoint.
        let long = "é".repeat(MAX_REPLY_CHARS + 1);
        let capped = cap_reply(&long);
        assert!(capped.chars().next().is_some());
        assert!(capped.ends_with("[truncated]"));
    }

    #[test]
    fn client_type_is_visible_to_the_handler_module() {
        // A compile-time guard: `listen` depends on the shared client type, so
        // a future refactor that hides it must update this module too.
        fn _takes(_: &Client, _: &Transport) {}
    }
}
