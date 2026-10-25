//! Room routing: per-agent inboxes (for one-shot `bus recv`), live tail
//! subscriptions, per-agent rate limiting, and fan-out with kind-based drop
//! policy (`drop_oldest` for ephemeral kinds, never-drop for durable kinds).

use anyhow::Result;
use proto::{DropClass, Envelope, Kind};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Notify};

const RATE_PER_SEC: usize = 60;
const EPHEMERAL_INBOX_CAP: usize = 1000;
const DURABLE_INBOX_WARN_CAP: usize = 10_000;

/// One agent's mailbox. One-shot `bus recv` connections drain this without
/// holding a persistent socket open.
pub struct Inbox {
    q: Mutex<VecDeque<Envelope>>,
    notify: Notify,
}

impl Inbox {
    fn new() -> Inbox {
        Inbox {
            q: Mutex::new(VecDeque::new()),
            notify: Notify::new(),
        }
    }

    fn push(&self, env: Envelope) {
        let mut q = self.q.lock().unwrap();
        let class = env
            .kind()
            .map(|k| k.drop_class())
            .unwrap_or(DropClass::DropOldest);
        if matches!(class, DropClass::DropOldest) && q.len() >= EPHEMERAL_INBOX_CAP {
            // drop_oldest: a slow chat consumer must not stall the room
            q.pop_front();
        }
        if matches!(class, DropClass::Block) && q.len() >= DURABLE_INBOX_WARN_CAP {
            // rate limits upstream make this pathological; keep the message anyway
            eprintln!(
                "busd: durable inbox over cap ({}), retaining message",
                q.len()
            );
        }
        q.push_back(env);
        drop(q);
        self.notify.notify_one();
    }

    /// Discard everything queued for `room`.
    fn drop_room(&self, room: &str) {
        self.q.lock().unwrap().retain(|e| e.room != room);
    }

    /// Pop the next message matching `room_filter`, waiting up to `timeout`.
    /// Non-matching messages stay in the queue.
    async fn recv(&self, room_filter: Option<&str>, timeout: Duration) -> Option<Envelope> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            {
                let mut q = self.q.lock().unwrap();
                if let Some(pos) = q
                    .iter()
                    .position(|e| room_filter.is_none_or(|room| e.room == room))
                {
                    return q.remove(pos);
                }
            }
            /* Bound the wait by the deadline. Awaiting `notified()` bare can
             * block forever: a permit stored by an earlier push is consumed by
             * one iteration, and if nothing arrives after that the await never
             * returns, so the caller's timeout is silently ignored. */
            if tokio::time::timeout_at(deadline, self.notify.notified())
                .await
                .is_err()
            {
                return None;
            }
        }
    }
}

/// A live `bus tail` connection.
pub struct TailSub {
    pub rooms: Option<HashSet<String>>,
    pub kinds: Option<HashSet<String>>,
    pub tx: mpsc::UnboundedSender<Envelope>,
}

impl TailSub {
    fn matches(&self, env: &Envelope) -> bool {
        if let Some(rooms) = &self.rooms {
            if !rooms.contains(&env.room) {
                return false;
            }
        }
        if let Some(kinds) = &self.kinds {
            if !kinds.contains(&env.kind) {
                return false;
            }
        }
        true
    }
}

pub struct Router {
    inboxes: Mutex<HashMap<String, Arc<Inbox>>>,
    tails: Mutex<Vec<u32>>,
    tail_registry: Mutex<HashMap<u32, TailSub>>,
    next_tail_id: std::sync::atomic::AtomicU32,
    /// agent -> send timestamps in the current window
    rate: Mutex<HashMap<String, VecDeque<Instant>>>,
    /// room -> members (mirror of the members table, updated on join/leave)
    members: std::sync::RwLock<HashMap<String, HashSet<String>>>,
    /// id prefixes whose rate is measured rather than limited
    exempt: std::sync::RwLock<HashSet<String>>,
}

impl Default for Router {
    fn default() -> Self {
        Self::new()
    }
}

impl Router {
    pub fn new() -> Router {
        Router {
            inboxes: Mutex::new(HashMap::new()),
            tails: Mutex::new(Vec::new()),
            tail_registry: Mutex::new(HashMap::new()),
            next_tail_id: std::sync::atomic::AtomicU32::new(1),
            rate: Mutex::new(HashMap::new()),
            members: std::sync::RwLock::new(HashMap::new()),
            exempt: std::sync::RwLock::new(HashSet::new()),
        }
    }

    // -- rate limiting -----------------------------------------------------

    pub fn rate_check(&self, agent: &str) -> bool {
        if self.is_rate_exempt(&self.exempt.read().unwrap(), agent) {
            return true;
        }
        let mut rate = self.rate.lock().unwrap();
        let q = rate.entry(agent.to_string()).or_default();
        let now = Instant::now();
        while q
            .front()
            .is_some_and(|t| now.duration_since(*t) > Duration::from_secs(1))
        {
            q.pop_front();
        }
        if q.len() >= RATE_PER_SEC {
            return false;
        }
        q.push_back(now);
        true
    }

    /// Trade `n` rate-limit slots for one check, so a `send_batch` frame counts
    /// as `n` messages for policy purposes rather than one. This is what stops
    /// batching from being a way to bypass the 60 msg/s limit in bulk.
    pub fn rate_check_batch(&self, agent: &str, n: usize) -> bool {
        for _ in 0..n {
            if !self.rate_check(agent) {
                return false;
            }
        }
        true
    }

    /// Agents whose send rate is being measured rather than governed.
    ///
    /// The 60 msg/s limit exists to keep one agent from starving a room, so it
    /// has to stay off when a bench is measuring what the append path can do.
    /// Matching is a prefix on a leading `~`-free id, e.g. `bench:` exempts
    /// `bench:w0`.
    pub fn exempt_from_rate_limit(&self, prefix: &str) {
        self.exempt.write().unwrap().insert(prefix.to_string());
    }

    fn is_rate_exempt(&self, prefixes: &HashSet<String>, agent: &str) -> bool {
        prefixes.iter().any(|p| agent.starts_with(p.as_str()))
    }

    // -- inboxes -------------------------------------------------------------

    pub fn inbox(&self, agent: &str) -> Arc<Inbox> {
        let mut inboxes = self.inboxes.lock().unwrap();
        inboxes
            .entry(agent.to_string())
            .or_insert_with(|| Arc::new(Inbox::new()))
            .clone()
    }

    pub async fn recv(
        &self,
        agent: &str,
        room: Option<&str>,
        timeout: Duration,
    ) -> Option<Envelope> {
        let inbox = self.inbox(agent);
        inbox.recv(room, timeout).await
    }

    // -- tails ---------------------------------------------------------------

    pub fn subscribe_tail(
        &self,
        rooms: Option<HashSet<String>>,
        kinds: Option<HashSet<String>>,
    ) -> (u32, mpsc::UnboundedReceiver<Envelope>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let id = self
            .next_tail_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.tail_registry
            .lock()
            .unwrap()
            .insert(id, TailSub { rooms, kinds, tx });
        self.tails.lock().unwrap().push(id);
        (id, rx)
    }

    pub fn unsubscribe_tail(&self, id: u32) {
        self.tail_registry.lock().unwrap().remove(&id);
        self.tails.lock().unwrap().retain(|t| *t != id);
    }

    // -- membership mirror -------------------------------------------------

    pub fn load_members(&self, room: &str, members: Vec<String>) {
        let mut m = self.members.write().unwrap();
        m.entry(room.to_string()).or_default().extend(members);
    }

    pub fn member_joined(&self, room: &str, agent: &str) {
        self.members
            .write()
            .unwrap()
            .entry(room.to_string())
            .or_default()
            .insert(agent.to_string());
    }

    pub fn member_left(&self, room: &str, agent: &str) {
        if let Some(set) = self.members.write().unwrap().get_mut(room) {
            set.remove(agent);
        }
        /* Leaving means "done with this room", so undelivered messages addressed to it are dropped rather than resurfacing on a later join. Nothing is lost: the inbox is only a delivery queue, and `history` replays the durable record. */
        if let Some(inbox) = self.inboxes.lock().unwrap().get(agent) {
            inbox.drop_room(room);
        }
    }

    pub fn members_of(&self, room: &str) -> Vec<String> {
        self.members
            .read()
            .unwrap()
            .get(room)
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default()
    }

    // -- fan-out -------------------------------------------------------------

    /// Deliver an appended envelope to room members' inboxes (minus the
    /// sender) and to matching live tails.
    pub fn fan_out(&self, env: &Envelope) {
        for agent in self.members_of(&env.room) {
            if agent == env.from.id {
                continue;
            }
            self.inbox(&agent).push(env.clone());
        }
        let registry = self.tail_registry.lock().unwrap();
        for tail in registry.values() {
            if tail.matches(env) {
                // dead tail consumers are dropped by the session on send failure
                let _ = tail.tx.send(env.clone());
            }
        }
    }
}

/// Validation shared by every `send` path.
pub fn validate_send(
    env: &Envelope,
    kind: Kind,
    role: proto::Role,
) -> Result<(), (String, String)> {
    if !role.allowed(&kind) {
        return Err((
            "role".into(),
            format!(
                "role {:?} may not send {} ({})",
                role,
                kind.as_str(),
                if kind == proto::Kind::Policy {
                    "policy is trust=human only"
                } else {
                    "role floor"
                }
            ),
        ));
    }
    if let Err(e) = kind.validate_payload(env.payload.as_ref(), env.reply_to.as_deref()) {
        return Err(("payload".into(), e.to_string()));
    }
    Ok(())
}
