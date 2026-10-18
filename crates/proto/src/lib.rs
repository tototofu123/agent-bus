//! agent-bus wire protocol: envelope, kinds, NDJSON framing, roles, tokens.
//!
//! Everything on the wire is a single `\n`-terminated JSON object (NDJSON),
//! max [`MAX_LINE_BYTES`] per line. See docs/protocol.md.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};

pub const V: u32 = 1;
pub const MAX_LINE_BYTES: usize = 1024 * 1024;

// ---------------------------------------------------------------------------
// Identity

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Agent {
    pub id: String,
    #[serde(default)]
    pub harness: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub session: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub caps: Vec<String>,
}

/// Role assigned by the broker from the declared model + harness.
/// Never self-asserted (see docs/protocol.md, "Roles and kind permissions").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// No model at all (shell script / relay).
    Transport,
    /// ~1B native-FC class: status/presence/routing only.
    Protocol,
    /// ~7B local class: may claim, ask/answer, handoff.
    Conversation,
    /// Frontier: owns tasks end-to-end.
    Peer,
}

/// Rough parameter count in billions parsed from a model name, e.g.
/// "qwen2.5-coder-7b" -> 7.0. None = unknown.
fn params_b(model: &str) -> Option<f64> {
    let m = model.to_ascii_lowercase();
    let bytes = m.as_bytes();
    for i in (0..bytes.len()).rev() {
        if bytes[i] == b'-' || bytes[i] == b'_' || bytes[i] == b'/' || bytes[i] == b':' {
            let tail = &m[i + 1..];
            let num: String = tail
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '.')
                .collect();
            let unit = &tail[num.len()..];
            if let Ok(n) = num.parse::<f64>() {
                if unit.starts_with('b') {
                    return Some(n);
                } else if unit.starts_with('m') {
                    return Some(n / 1000.0);
                }
            }
        }
    }
    // also try a bare trailing number with no separator, e.g. "gpt5codex7b"
    if let Some(pos) = m.rfind(|c: char| c.is_ascii_digit()) {
        // walk back to the start of the digit run
        let mut start = pos;
        let b = m.as_bytes();
        while start > 0 && (b[start - 1].is_ascii_digit() || b[start - 1] == b'.') {
            start -= 1;
        }
        let num: String = m[start..=pos]
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        if pos + 1 < m.len() && (m.as_bytes()[pos + 1] == b'b') {
            if let Ok(n) = num.parse::<f64>() {
                return Some(n);
            }
        }
    }
    None
}

/// Frontier models that publish no parameter count, so `params_b` cannot place
/// them, and whose name alone already pins a specific release.
const FRONTIER_EXACT: &[&str] = &[
    "gpt-5",
    "gpt-4.1",
    "gpt-4o",
    "o1",
    "o3",
    "o4",
    "gemini-1.5-pro",
    "gemini-2",
];

/// Frontier families whose bare alias carries no version. These only earn
/// `peer` once the name pins a release ("claude-sonnet-4-5"), so "claude-sonnet"
/// still floors at `protocol`.
const FRONTIER_FAMILY: &[&str] = &["claude-opus", "claude-sonnet", "grok"];

/// Closed models publish no parameter count, so the numeric floors cannot place
/// them. They are recognised by name rather than guessed at: anything
/// unrecognised, or named only by an unversioned family alias, still floors at
/// `protocol`, because guessing upward is how a mislabelled 1B ends up owning
/// a migration.
fn is_named_frontier(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    if FRONTIER_EXACT.iter().any(|p| m.starts_with(p)) {
        return true;
    }
    FRONTIER_FAMILY.iter().any(|p| {
        m.starts_with(p)
            && m[p.len()..]
                .strip_prefix('-')
                .unwrap_or(&m[p.len()..])
                .starts_with(|c: char| c.is_ascii_digit())
    })
}

impl Role {
    /// Assigned at registration. Unknown model size defaults to the weakest
    /// non-transport role: guessing upward is how a mislabelled 1B ends up
    /// owning a migration.
    pub fn assign(agent: &Agent) -> Role {
        let harness = agent.harness.to_ascii_lowercase();
        let script =
            harness.contains("script") || harness.contains("sh") || harness.contains("curl");
        match (agent.model.as_deref(), script) {
            (None, true) | (Some(""), true) | (Some("none"), true) => Role::Transport,
            (None, _) => Role::Protocol,
            (Some(m), _) => {
                if m.is_empty() || m == "none" {
                    return if script {
                        Role::Transport
                    } else {
                        Role::Protocol
                    };
                }
                match params_b(m) {
                    Some(b) if b >= 30.0 => Role::Peer,
                    Some(b) if b >= 7.0 => Role::Conversation,
                    Some(_) => Role::Protocol,
                    None if is_named_frontier(m) => Role::Peer,
                    None => Role::Protocol,
                }
            }
        }
    }

    /// May this role send this kind? Enforced on every inbound message.
    pub fn allowed(self, kind: &Kind) -> bool {
        use Kind::*;
        match kind {
            // A transport is a relay: it forwards text and files, and is not
            // trusted to summarise, decide, or ask on anyone's behalf.
            Chat | Artifact => true,
            Status => !matches!(self, Role::Transport),
            Question | Answer => !matches!(self, Role::Transport),
            TaskOffer | TaskClaim | TaskHandoff | TaskResult => {
                matches!(self, Role::Conversation | Role::Peer)
            }
            // Broker-emitted (presence, task.grant/deny) or human-only (policy).
            Presence | TaskGrant | TaskDeny | Policy => false,
        }
    }
}

// ---------------------------------------------------------------------------
// Kinds

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Chat,
    #[serde(rename = "task.offer")]
    TaskOffer,
    #[serde(rename = "task.claim")]
    TaskClaim,
    #[serde(rename = "task.grant")]
    TaskGrant,
    #[serde(rename = "task.deny")]
    TaskDeny,
    #[serde(rename = "task.handoff")]
    TaskHandoff,
    #[serde(rename = "task.result")]
    TaskResult,
    Question,
    Answer,
    Artifact,
    Status,
    Presence,
    Policy,
}

impl Kind {
    pub fn parse(s: &str) -> Option<Kind> {
        use Kind::*;
        Some(match s {
            "chat" => Chat,
            "task.offer" => TaskOffer,
            "task.claim" => TaskClaim,
            "task.grant" => TaskGrant,
            "task.deny" => TaskDeny,
            "task.handoff" => TaskHandoff,
            "task.result" => TaskResult,
            "question" => Question,
            "answer" => Answer,
            "artifact" => Artifact,
            "status" => Status,
            "presence" => Presence,
            "policy" => Policy,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        use Kind::*;
        match self {
            Chat => "chat",
            TaskOffer => "task.offer",
            TaskClaim => "task.claim",
            TaskGrant => "task.grant",
            TaskDeny => "task.deny",
            TaskHandoff => "task.handoff",
            TaskResult => "task.result",
            Question => "question",
            Answer => "answer",
            Artifact => "artifact",
            Status => "status",
            Presence => "presence",
            Policy => "policy",
        }
    }

    /// Durable kinds persist until room close; ephemeral kinds TTL (24h default).
    pub fn is_durable(self) -> bool {
        use Kind::*;
        matches!(
            self,
            TaskOffer
                | TaskClaim
                | TaskGrant
                | TaskDeny
                | TaskHandoff
                | TaskResult
                | Artifact
                | Policy
        )
    }

    /// Backpressure class: durable/control kinds must never be silently dropped.
    pub fn drop_class(self) -> DropClass {
        if self.is_durable() {
            DropClass::Block
        } else {
            DropClass::DropOldest
        }
    }

    /// Minimal payload validation (payload contracts in docs/protocol.md).
    pub fn validate_payload(
        self,
        payload: Option<&Value>,
        reply_to: Option<&str>,
    ) -> Result<(), String> {
        use Kind::*;
        match self {
            Chat => Ok(()),
            TaskOffer => {
                let p = payload.ok_or("task.offer requires payload")?;
                p.get("files")
                    .and_then(|f| f.as_array())
                    .ok_or("task.offer payload.files must be an array")?;
                Ok(())
            }
            TaskClaim => {
                let p = payload.ok_or("task.claim requires payload")?;
                p.get("files")
                    .and_then(|f| f.as_array())
                    .ok_or("task.claim payload.files must be an array")?;
                Ok(())
            }
            TaskHandoff => {
                let p = payload.ok_or("task.handoff requires payload")?;
                p.get("to")
                    .and_then(|t| t.as_str())
                    .ok_or("task.handoff payload.to must be a string")?;
                Ok(())
            }
            TaskResult | TaskGrant | TaskDeny => Ok(()),
            Question => Ok(()),
            Answer => {
                reply_to.ok_or("answer must set reply_to")?;
                Ok(())
            }
            Artifact | Status | Presence => Ok(()),
            Policy => {
                let p = payload.ok_or("policy requires payload")?;
                if p.get("allow").and_then(|v| v.as_array()).is_none()
                    && p.get("deny").and_then(|v| v.as_array()).is_none()
                    && p.get("requires_approval")
                        .and_then(|v| v.as_array())
                        .is_none()
                {
                    return Err("policy payload needs allow/deny/requires_approval".into());
                }
                Ok(())
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropClass {
    Block,
    DropOldest,
}

/// Trust is set by the broker, never the sender.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Trust {
    SelfAgent,
    Peer,
    Human,
    Untrusted,
}

// ---------------------------------------------------------------------------
// Envelope

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    pub v: u32,
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seq: Option<i64>,
    pub room: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    pub from: Agent,
    pub ts: String,
    #[serde(default)]
    pub lamport: u64,
    pub kind: String,
    #[serde(default = "default_priority")]
    pub priority: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trust: Option<Trust>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifacts: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sig: Option<String>,
}

fn default_priority() -> String {
    "normal".into()
}

impl Envelope {
    pub fn new(room: &str, from: Agent, kind: Kind) -> Envelope {
        Envelope {
            v: V,
            id: new_id(),
            seq: None,
            room: room.to_string(),
            thread: None,
            reply_to: None,
            from,
            ts: iso_ts_now(),
            lamport: 0,
            kind: kind.as_str().to_string(),
            priority: "normal".into(),
            trust: None,
            text: None,
            payload: None,
            artifacts: None,
            sig: None,
        }
    }

    pub fn kind(&self) -> Option<Kind> {
        Kind::parse(&self.kind)
    }
}

pub fn new_id() -> String {
    ulid::Ulid::new().to_string()
}

pub fn iso_ts_now() -> String {
    // RFC3339 with milliseconds, local offset folded to +00:00 for determinism.
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    let secs = now.as_secs();
    let millis = now.subsec_millis();
    let days = secs / 86_400;
    let (y, mo, d) = civil_from_days(days as i64);
    let rem = secs % 86_400;
    format!(
        "{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{millis:03}Z",
        h = rem / 3600,
        mi = (rem % 3600) / 60,
        s = rem % 60
    )
}

/// Howard Hinnant's days-to-civil algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

// ---------------------------------------------------------------------------
// NDJSON framing

#[derive(Debug)]
pub enum FrameError {
    Empty,
    TooLarge { len: usize },
    InvalidUtf8,
    NotJson(String),
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::Empty => write!(f, "empty line"),
            FrameError::TooLarge { len } => {
                write!(f, "line exceeds {} bytes (got {})", MAX_LINE_BYTES, len)
            }
            FrameError::InvalidUtf8 => write!(f, "invalid utf-8"),
            FrameError::NotJson(e) => write!(f, "invalid json: {e}"),
        }
    }
}

/// Validate + decode one NDJSON line (without the trailing newline).
pub fn decode_line(line: &[u8]) -> Result<Value, FrameError> {
    if line.is_empty() {
        return Err(FrameError::Empty);
    }
    if line.len() > MAX_LINE_BYTES {
        return Err(FrameError::TooLarge { len: line.len() });
    }
    let s = std::str::from_utf8(line).map_err(|_| FrameError::InvalidUtf8)?;
    serde_json::from_str(s).map_err(|e| FrameError::NotJson(e.to_string()))
}

/// Encode one value as an NDJSON line (always `\n`-terminated).
pub fn encode_line(v: &Value) -> String {
    let mut s = serde_json::to_string(v).unwrap_or_default();
    s.push('\n');
    s
}

// ---------------------------------------------------------------------------
// Tokens

/// 32 random bytes, hex-encoded.
pub fn new_token() -> String {
    use rand::RngCore;
    let mut b = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut b);
    hex::encode(b)
}

/// SHA-256 hex digest of a token; only the hash is stored.
pub fn hash_token(token: &str) -> String {
    let mut h = Sha256::new();
    h.update(token.as_bytes());
    hex::encode(h.finalize())
}

// ---------------------------------------------------------------------------
// Ops (client <-> broker control messages)

pub const OP_HELLO: &str = "hello";
pub const OP_WELCOME: &str = "welcome";
pub const OP_BYE: &str = "bye";
pub const OP_ERROR: &str = "error";
pub const OP_SENT: &str = "sent";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framing_roundtrip() {
        let mut v = serde_json::json!({"op": "hello", "v": 1});
        let line = encode_line(&v);
        assert!(line.ends_with('\n'));
        let decoded = decode_line(line.trim_end_matches('\n').as_bytes()).unwrap();
        assert_eq!(v, decoded);
        v["x"] = 42.into();
        assert_ne!(v, decoded);
    }

    #[test]
    fn framing_rejects_oversize() {
        let big = vec![b'a'; MAX_LINE_BYTES + 1];
        assert!(matches!(
            decode_line(&big),
            Err(FrameError::TooLarge { .. })
        ));
    }

    #[test]
    fn framing_rejects_garbage() {
        assert!(matches!(
            decode_line(b"{not json"),
            Err(FrameError::NotJson(_))
        ));
        assert!(matches!(decode_line(b""), Err(FrameError::Empty)));
    }

    #[test]
    fn params_b_parses_common_names() {
        assert_eq!(params_b("qwen2.5-coder-7b"), Some(7.0));
        assert_eq!(params_b("Qwen3-4B-Instruct"), Some(4.0));
        assert_eq!(params_b("xLAM-2-1B-fc-r"), Some(1.0));
        assert_eq!(params_b("tinyllama-1.1b"), Some(1.1));
        assert_eq!(params_b("llama-3-70b"), Some(70.0));
        assert_eq!(params_b("gpt-5-codex"), None);
        assert_eq!(params_b("qwen-560m"), Some(0.56));
    }

    #[test]
    fn role_assign_floors() {
        let script = Agent {
            id: "s@x".into(),
            harness: "script".into(),
            model: None,
            session: None,
            cwd: None,
            caps: vec![],
        };
        assert_eq!(Role::assign(&script), Role::Transport);

        let small = Agent {
            model: Some("qwen3-0.6b".into()),
            ..script.clone()
        };
        assert_eq!(Role::assign(&small), Role::Protocol);

        let mid = Agent {
            model: Some("qwen2.5-coder-7b".into()),
            ..script.clone()
        };
        assert_eq!(Role::assign(&mid), Role::Conversation);

        let frontier = Agent {
            model: Some("claude-sonnet".into()),
            ..script.clone()
        };
        assert_eq!(Role::assign(&frontier), Role::Protocol); // unknown size -> weakest

        let big = Agent {
            model: Some("llama-3-70b".into()),
            ..script.clone()
        };
        assert_eq!(Role::assign(&big), Role::Peer);
    }

    #[test]
    fn role_permissions() {
        assert!(Role::Protocol.allowed(&Kind::Status));
        assert!(!Role::Protocol.allowed(&Kind::TaskClaim));
        assert!(Role::Conversation.allowed(&Kind::TaskClaim));
        assert!(!Role::Peer.allowed(&Kind::Policy)); // human-only, always
        assert!(Role::Transport.allowed(&Kind::Chat));
        assert!(!Role::Transport.allowed(&Kind::Question));
    }

    #[test]
    fn kind_payload_contracts() {
        let files = serde_json::json!({"files": ["src/**"]});
        assert!(Kind::TaskOffer.validate_payload(Some(&files), None).is_ok());

        let no_files = serde_json::json!({"claim_ttl_s": 300});
        assert!(Kind::TaskOffer
            .validate_payload(Some(&no_files), None)
            .is_err());
        assert!(Kind::TaskClaim.validate_payload(None, None).is_err());

        assert!(Kind::Answer.validate_payload(None, None).is_err());
        assert!(Kind::Answer.validate_payload(None, Some("01J8Y7")).is_ok());

        let pol = serde_json::json!({"deny": ["src/db/**"]});
        assert!(Kind::Policy.validate_payload(Some(&pol), None).is_ok());
        assert!(Kind::Policy
            .validate_payload(Some(&serde_json::json!({})), None)
            .is_err());
    }

    #[test]
    fn durable_and_drop_classes() {
        assert!(Kind::TaskClaim.is_durable());
        assert!(!Kind::Chat.is_durable());
        assert_eq!(Kind::Chat.drop_class(), DropClass::DropOldest);
        assert_eq!(Kind::TaskGrant.drop_class(), DropClass::Block);
    }

    #[test]
    fn token_hash_is_stable_and_salted_by_value() {
        let t = new_token();
        assert_eq!(t.len(), 64);
        assert_eq!(hash_token(&t), hash_token(&t));
        assert_ne!(hash_token(&t), hash_token("other"));
    }

    #[test]
    fn iso_ts_shape() {
        let ts = iso_ts_now();
        assert_eq!(ts.len(), 24);
        assert!(ts.ends_with('Z'));
        assert_eq!(&ts[4..5], "-");
        assert_eq!(&ts[10..11], "T");
    }

    #[test]
    fn envelope_serializes_doc_shape() {
        let mut env = Envelope::new(
            "#task:fix-auth",
            Agent {
                id: "codex@p".into(),
                harness: "codex-cli".into(),
                model: Some("gpt-5-codex".into()),
                session: None,
                cwd: None,
                caps: vec!["edit".into()],
            },
            Kind::TaskOffer,
        );
        env.payload = Some(serde_json::json!({"files": ["src/auth/**"], "claim_ttl_s": 300}));
        let v = serde_json::to_value(&env).unwrap();
        assert_eq!(v["v"], 1);
        assert_eq!(v["kind"], "task.offer");
        assert_eq!(v["room"], "#task:fix-auth");
        assert!(v["id"].as_str().unwrap().len() >= 20);
    }
}
