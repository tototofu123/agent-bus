//! SQLite WAL storage. One file the user can delete.
//!
//! The whole envelope JSON is stored (`env_json`) alongside indexed columns;
//! history/replay decode from it. `ts` is kept as the ISO string from the
//! envelope plus `ts_ms` for ordering/TTL math.

use anyhow::{Context, Result};
use proto::{hash_token, iso_ts_now, new_id, Agent, Envelope, Kind, Trust};
use rusqlite::{params, Connection};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

pub struct Store {
    conn: Arc<Mutex<Connection>>,
}

/// Presence window (issue #9): last_seen within this long means online.
/// Live agents touch last_seen on hello and on every frame; the shims'
/// recv loop polls every 20 s, so 120 s separates live from gone with
/// room to spare for one missed poll.
pub const ONLINE_WINDOW_MS: i64 = 120_000;

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

const EPHEMERAL_TTL_MS: i64 = 24 * 3600 * 1000;

impl Store {
    pub fn open(path: &Path) -> Result<Store> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).context("create data dir")?;
        }
        let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA foreign_keys=ON;",
        )?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS rooms (
               name      TEXT PRIMARY KEY,
               acl_json  TEXT NOT NULL DEFAULT '{}',
               opened    INTEGER NOT NULL,
               closed    INTEGER,
               seq       INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE IF NOT EXISTS messages (
               id        TEXT PRIMARY KEY,
               room      TEXT NOT NULL REFERENCES rooms(name),
               seq       INTEGER NOT NULL,
               ts_ms     INTEGER NOT NULL,
               kind      TEXT NOT NULL,
               sender    TEXT NOT NULL,
               expires   INTEGER,
               env_json  TEXT NOT NULL,
               UNIQUE(room, seq)
             );
             CREATE INDEX IF NOT EXISTS messages_room_seq ON messages(room, seq);
             CREATE INDEX IF NOT EXISTS messages_sender  ON messages(sender, ts_ms);
             CREATE TABLE IF NOT EXISTS agents (
               id         TEXT PRIMARY KEY,
               harness    TEXT NOT NULL,
               model      TEXT,
               session    TEXT,
               cwd        TEXT,
               caps       TEXT NOT NULL DEFAULT '[]',
               role       TEXT NOT NULL DEFAULT 'protocol',
               joined     INTEGER NOT NULL,
               last_seen  INTEGER NOT NULL,
               token_hash TEXT
             );
             CREATE TABLE IF NOT EXISTS members (
               agent     TEXT NOT NULL REFERENCES agents(id),
               room      TEXT NOT NULL REFERENCES rooms(name),
               joined    INTEGER NOT NULL,
               PRIMARY KEY (agent, room)
             );
             CREATE TABLE IF NOT EXISTS locks (
               path_glob TEXT PRIMARY KEY,
               holder    TEXT NOT NULL,
               room      TEXT NOT NULL,
               leased_at INTEGER NOT NULL,
               expires   INTEGER NOT NULL,
               task      TEXT
             );
             CREATE INDEX IF NOT EXISTS locks_expires ON locks(expires);
             CREATE TABLE IF NOT EXISTS artifacts (
               ref       TEXT PRIMARY KEY,
               kind      TEXT NOT NULL,
               path      TEXT,
               bytes     INTEGER NOT NULL,
               stored_at INTEGER NOT NULL,
               data      BLOB
             );
             CREATE TABLE IF NOT EXISTS schedules (
               name       TEXT PRIMARY KEY,
               room       TEXT NOT NULL,
               cron       TEXT NOT NULL,
               timezone   TEXT NOT NULL DEFAULT 'local',
               text       TEXT NOT NULL DEFAULT '',
               payload    TEXT NOT NULL DEFAULT '{}',
               enabled    INTEGER NOT NULL DEFAULT 1,
               created_at INTEGER NOT NULL,
               last_fired INTEGER
             );",
        )
        .context("migrations")?;
        Ok(Store {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn handle(&self) -> Arc<Mutex<Connection>> {
        self.conn.clone()
    }

    // -- agents -----------------------------------------------------------

    pub fn upsert_agent(
        &self,
        a: &Agent,
        role: proto::Role,
        token_hash: Option<&str>,
    ) -> Result<()> {
        let now = now_ms();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (id, harness, model, session, cwd, caps, role, joined, last_seen, token_hash)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8, ?9)
             ON CONFLICT(id) DO UPDATE SET
               harness=excluded.harness, model=excluded.model, session=excluded.session,
               cwd=excluded.cwd, caps=excluded.caps, role=excluded.role, last_seen=excluded.last_seen",
            params![
                a.id,
                a.harness,
                a.model,
                a.session,
                a.cwd,
                serde_json::to_string(&a.caps).unwrap(),
                serde_json::to_string(&role).unwrap().trim_matches('"'),
                now,
                token_hash
            ],
        )?;
        Ok(())
    }

    pub fn touch_agent(&self, id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE agents SET last_seen=?1 WHERE id=?2",
            params![now_ms(), id],
        )?;
        Ok(())
    }

    pub fn agent_role(&self, id: &str) -> Result<Option<proto::Role>> {
        let conn = self.conn.lock().unwrap();
        let role: Option<String> = conn
            .query_row("SELECT role FROM agents WHERE id=?1", params![id], |r| {
                r.get(0)
            })
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                e => Err(e),
            })?;
        Ok(role.and_then(|s| serde_json::from_str(&format!("\"{s}\"")).ok()))
    }

    pub fn token_hash(&self, id: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let h: Option<String> = conn
            .query_row(
                "SELECT token_hash FROM agents WHERE id=?1",
                params![id],
                |r| r.get(0),
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                e => Err(e),
            })?;
        Ok(h)
    }

    /// Constant-time-ish token check against the stored hash.
    pub fn token_matches(&self, id: &str, token: &str) -> bool {
        match self.token_hash(id) {
            Ok(Some(h)) => {
                let got = hash_token(token);
                if h.len() != got.len() {
                    return false;
                }
                h.bytes()
                    .zip(got.bytes())
                    .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                    == 0
            }
            _ => false,
        }
    }

    pub fn who(&self, room: Option<&str>) -> Result<Vec<serde_json::Value>> {
        let conn = self.conn.lock().unwrap();
        // The room takes one parameter; the unscoped listing takes none, so the
        // two branches must bind different argument lists.
        let sql = match room {
            Some(_) => {
                "SELECT a.id, a.harness, a.model, a.last_seen, a.role, a.caps FROM agents a
                           JOIN members m ON m.agent = a.id WHERE m.room = ?1 ORDER BY a.id"
            }
            None => "SELECT id, harness, model, last_seen, role, caps FROM agents ORDER BY id",
        };
        let mut stmt = conn.prepare(sql)?;
        let row = |r: &rusqlite::Row| -> rusqlite::Result<(String, serde_json::Value)> {
            let id = r.get::<_, String>(0)?;
            let last_seen = r.get::<_, i64>(3)?;
            let caps: Vec<String> = r
                .get::<_, String>(5)
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default();
            Ok((id.clone(), serde_json::json!({
                "id": id,
                "harness": r.get::<_, String>(1)?,
                "model": r.get::<_, Option<String>>(2)?,
                "last_seen_ms": last_seen,
                // Presence (issue #9): an agent whose recv loop / shim is
                // alive touches last_seen at least every ~20 s (hello and
                // every frame), so 120 s cleanly separates live from gone.
                // A bot that stops is visibly offline within 2 minutes.
                "status": if now_ms() - last_seen <= ONLINE_WINDOW_MS { "online" } else { "offline" },
                "role": r.get::<_, String>(4)?,
                "caps": caps,
            })))
        };
        let mut rows: Vec<(String, serde_json::Value)> = match room {
            Some(room) => {
                let rows = stmt.query_map(params![room], row)?;
                rows.filter_map(|r| r.ok()).collect()
            }
            None => {
                let rows = stmt.query_map([], row)?;
                rows.filter_map(|r| r.ok()).collect()
            }
        };
        // Room subscriptions per agent (issue #9 roster). A second query
        // per agent, not a join: who() is a rare admin read, and the
        // join would duplicate rows for multi-room agents.
        for (id, v) in rows.iter_mut() {
            let mut stmt2 = conn.prepare("SELECT room FROM members WHERE agent=?1 ORDER BY room")?;
            let rooms: Vec<String> = stmt2
                .query_map(params![id.as_str()], |r| r.get::<_, String>(0))?
                .filter_map(|r| r.ok())
                .collect();
            v["rooms"] = serde_json::json!(rooms);
        }
        Ok(rows.into_iter().map(|(_, v)| v).collect())
    }

    // -- rooms / membership -------------------------------------------------

    pub fn ensure_room(&self, name: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO rooms (name, opened) VALUES (?1, ?2)",
            params![name, now_ms()],
        )?;
        Ok(())
    }

    pub fn room_seq(&self, name: &str) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        Ok(
            conn.query_row("SELECT seq FROM rooms WHERE name=?1", params![name], |r| {
                r.get(0)
            })?,
        )
    }

    pub fn rooms(&self) -> Result<Vec<serde_json::Value>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT name, seq, opened FROM rooms WHERE closed IS NULL ORDER BY name")?;
        let rows = stmt.query_map([], |r| {
            Ok(serde_json::json!({
                "name": r.get::<_, String>(0)?,
                "seq": r.get::<_, i64>(1)?,
                "opened_ms": r.get::<_, i64>(2)?,
            }))
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Has this identity ever registered? `members` and `messages` both foreign-key
    /// into `agents`, so an identity that never registered has to be told to
    /// register rather than hitting an opaque constraint failure.
    pub fn is_registered(&self, agent: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM agents WHERE id=?1",
            params![agent],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    pub fn join(&self, agent: &str, room: &str) -> Result<()> {
        self.ensure_room(room)?;
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO members (agent, room, joined) VALUES (?1, ?2, ?3)",
            params![agent, room, now_ms()],
        )?;
        Ok(())
    }

    pub fn leave(&self, agent: &str, room: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM members WHERE agent=?1 AND room=?2",
            params![agent, room],
        )?;
        Ok(())
    }

    pub fn members_of(&self, room: &str) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT agent FROM members WHERE room=?1")?;
        let rows = stmt.query_map(params![room], |r| r.get::<_, String>(0))?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn rooms_of(&self, agent: &str) -> Result<Vec<(String, i64)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT m.room, r.seq FROM members m JOIN rooms r ON r.name = m.room WHERE m.agent=?1",
        )?;
        let rows = stmt.query_map(params![agent], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    // -- schedules ------------------------------------------------------------

    fn row_to_schedule(r: &rusqlite::Row) -> rusqlite::Result<proto::schedule::ScheduleEntry> {
        // A payload that no longer parses as JSON is kept visible (as an
        // empty object) rather than dropping the row from listings: the
        // tick then rejects it as an invalid payload and reports it, and
        // the schedule stays removable.
        let payload_raw: String = r.get(5)?;
        let payload = serde_json::from_str(&payload_raw).unwrap_or_else(|_| serde_json::json!({}));
        Ok(proto::schedule::ScheduleEntry {
            name: r.get(0)?,
            room: r.get(1)?,
            cron: r.get(2)?,
            timezone: r.get(3)?,
            text: r.get(4)?,
            payload,
            enabled: r.get::<_, i64>(6)? != 0,
            created_at_ms: r.get(7)?,
            last_fired_ms: r.get(8)?,
        })
    }

    pub fn add_schedule(&self, e: &proto::schedule::ScheduleEntry) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO schedules (name, room, cron, timezone, text, payload, enabled, created_at, last_fired)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                e.name,
                e.room,
                e.cron,
                e.timezone,
                e.text,
                serde_json::to_string(&e.payload)?,
                e.enabled as i64,
                e.created_at_ms,
                e.last_fired_ms
            ],
        )?;
        Ok(())
    }

    pub fn get_schedule(&self, name: &str) -> Result<Option<proto::schedule::ScheduleEntry>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT name, room, cron, timezone, text, payload, enabled, created_at, last_fired
             FROM schedules WHERE name = ?1",
        )?;
        let mut rows = stmt.query_map(params![name], Self::row_to_schedule)?;
        match rows.next() {
            Some(r) => Ok(Some(r?)),
            None => Ok(None),
        }
    }

    pub fn list_schedules(
        &self,
        room: Option<&str>,
    ) -> Result<Vec<proto::schedule::ScheduleEntry>> {
        let conn = self.conn.lock().unwrap();
        let sql = match room {
            Some(_) => {
                "SELECT name, room, cron, timezone, text, payload, enabled, created_at, last_fired
                 FROM schedules WHERE room = ?1 ORDER BY name"
            }
            None => {
                "SELECT name, room, cron, timezone, text, payload, enabled, created_at, last_fired
                 FROM schedules ORDER BY name"
            }
        };
        let mut stmt = conn.prepare(sql)?;
        let rows = match room {
            Some(room) => {
                let rows = stmt.query_map(params![room], Self::row_to_schedule)?;
                rows.filter_map(|r| r.ok()).collect()
            }
            None => {
                let rows = stmt.query_map([], Self::row_to_schedule)?;
                rows.filter_map(|r| r.ok()).collect()
            }
        };
        Ok(rows)
    }

    pub fn remove_schedule(&self, name: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute("DELETE FROM schedules WHERE name = ?1", params![name])?;
        Ok(n > 0)
    }

    pub fn set_schedule_enabled(&self, name: &str, enabled: bool) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE schedules SET enabled = ?1 WHERE name = ?2",
            params![enabled as i64, name],
        )?;
        Ok(n > 0)
    }

    // -- append (the sequencer) ---------------------------------------------

    /// Assign the next `seq` and insert, in one transaction that commits
    /// immediately. This is the only writer of a room's `seq` (single-writer
    /// rule), and the correctness gates rely on it: a message either survives a
    /// crash or never existed, with no in-between state.
    ///
    /// Batch-of-N amortisation lives in `append_batch`, driven by the
    /// `send_batch` protocol op: folding several inserts into a *per-message*
    /// commit was measured at 6,139 msg/s against 8,759 for the direct path,
    /// because the send path commits each message as it is accepted, so such a
    /// batch always held exactly one message. The amortisation needs the client
    /// to send many envelopes in one frame.
    pub fn append(&self, env: &mut Envelope, kind: Kind) -> Result<i64> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let seq = self.assign_and_insert(&tx, env, kind)?;
        tx.commit()?;
        Ok(seq)
    }

    /// The whole send path in **one** transaction: check the sender is
    /// registered, make sure the room exists, then append.
    ///
    /// `op_send` used to do the membership check, the role lookup and the room
    /// ensure as separate store calls, each locking the connection and each
    /// committing — including an `INSERT OR IGNORE INTO rooms` that ran on
    /// every message to a room that already existed. That is three commits of
    /// work for nothing. Collapsing them is what brought send p99 under the 1 ms
    /// criterion while keeping `seq` assigned inside the same transaction, so
    /// ordering is unchanged.
    ///
    /// The caller still reads the role separately to validate the send *before*
    /// anything is written; only the redundant write work is folded in here.
    ///
    /// Returns `Some(seq)` on success, or `None` if the sender never
    /// registered — callers turn that into the `auth` error, preserving the
    /// previous behaviour with a quarter of the store traffic.
    pub fn append_from_registered(&self, env: &mut Envelope, kind: Kind) -> Result<Option<i64>> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let registered: i64 = tx.query_row(
            "SELECT COUNT(*) FROM agents WHERE id=?1",
            params![env.from.id],
            |r| r.get(0),
        )?;
        if registered == 0 {
            tx.rollback()?;
            return Ok(None);
        }
        tx.execute(
            "INSERT OR IGNORE INTO rooms (name, opened) VALUES (?1, ?2)",
            params![env.room, now_ms()],
        )?;
        let seq = self.assign_and_insert(&tx, env, kind)?;
        tx.commit()?;
        Ok(Some(seq))
    }

    /// Append many envelopes in **one** transaction. Returns the assigned seqs
    /// in input order.
    ///
    /// This is the throughput path for a `send_batch` frame: one commit
    /// amortised over N inserts, so the broker pays one fsync for the batch
    /// instead of N. It is what moves the throughput gate — the same call made
    /// once per message measured 6,139 msg/s against 8,759, because a batch of
    /// one is pure transaction overhead.
    ///
    /// The durability consequence is explicit: a `kill -9` while the batch is
    /// open loses every message in that batch, so `MAX_BATCH` is also the
    /// crash-loss bound. Ordering is unaffected — `seq` is assigned inside the
    /// same transaction that commits, so the batch is atomic either way.
    ///
    /// Validates the sender once and rejects the whole batch if it is not
    /// registered, matching the single-send behaviour.
    pub fn append_batch(&self, envs: &mut [(Envelope, Kind)]) -> Result<Option<Vec<i64>>> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let sender = envs.first().map(|(e, _)| e.from.id.clone());
        if let Some(sender) = sender {
            let registered: i64 = tx.query_row(
                "SELECT COUNT(*) FROM agents WHERE id=?1",
                params![sender],
                |r| r.get(0),
            )?;
            if registered == 0 {
                tx.rollback()?;
                return Ok(None);
            }
        }
        // The room row is ensured once for the batch, not once per envelope:
        // a batch always targets one room, and an `INSERT OR IGNORE` per
        // message is pure per-message overhead inside the transaction.
        if let Some((first, _)) = envs.first() {
            tx.execute(
                "INSERT OR IGNORE INTO rooms (name, opened) VALUES (?1, ?2)",
                params![first.room, now_ms()],
            )?;
        }
        let mut seqs = Vec::with_capacity(envs.len());
        for (env, kind) in envs.iter_mut() {
            seqs.push(self.assign_and_insert(&tx, env, *kind)?);
        }
        tx.commit()?;
        Ok(Some(seqs))
    }

    /// The shared body: assign seq, stamp, insert. Caller owns the transaction.
    fn assign_and_insert(
        &self,
        tx: &rusqlite::Transaction<'_>,
        env: &mut Envelope,
        kind: Kind,
    ) -> Result<i64> {
        let seq: i64 = tx.query_row(
            "UPDATE rooms SET seq = seq + 1 WHERE name = ?1 RETURNING seq",
            params![env.room],
            |r| r.get(0),
        )?;
        let expires = if kind.is_durable() {
            None
        } else {
            Some(now_ms() + EPHEMERAL_TTL_MS)
        };
        // Stamp before serialising. `env_json` is the durable record that
        // `history` replays, so anything set after the INSERT is missing from
        // the replay and reads back as null.
        env.seq = Some(seq);
        if env.trust.is_none() {
            env.trust = Some(Trust::Peer);
        }
        tx.execute(
            "INSERT INTO messages (id, room, seq, ts_ms, kind, sender, expires, env_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                env.id,
                env.room,
                seq,
                now_ms(),
                kind.as_str(),
                env.from.id,
                expires,
                serde_json::to_string(env)?
            ],
        )?;
        Ok(seq)
    }

    /// Replay from `since` (exclusive) onward, bounded.
    pub fn history(&self, room: &str, since: i64, limit: i64) -> Result<Vec<Envelope>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT env_json FROM messages
             WHERE room = ?1 AND seq > ?2 AND (expires IS NULL OR expires > ?3)
             ORDER BY seq ASC LIMIT ?4",
        )?;
        let rows = stmt.query_map(params![room, since, now_ms(), limit], |r| {
            r.get::<_, String>(0)
        })?;
        Ok(rows
            .filter_map(|r| r.ok())
            .filter_map(|s| serde_json::from_str(&s).ok())
            .collect())
    }

    /// One tick: for every enabled schedule whose next occurrence after
    /// its last fire (or creation) is due within the last minute, post its
    /// `task.offer` and record the occurrence as last-fired.
    ///
    /// Atomicity is what makes a tick idempotent within a minute: the
    /// whole tick holds the store lock, and each schedule's due-check +
    /// append + last-fired update runs inside one transaction, so two
    /// ticks (or a tick and a manual send) can never double-fire a
    /// schedule. A malformed schedule (bad cron, bad timezone, bad
    /// payload) is skipped and reported, never crashing the tick and
    /// never blocking the other schedules.
    ///
    /// Returns the fired envelopes — the caller fans them out to room
    /// members and live tails after the commit — and the per-schedule
    /// skip reasons for the tick reply.
    pub fn tick_schedules(&self, agent: &Agent, now_ms: i64) -> Result<TickOutcome> {
        let mut conn = self.conn.lock().unwrap();
        let entries: Vec<proto::schedule::ScheduleEntry> = {
            let mut stmt = conn.prepare(
                "SELECT name, room, cron, timezone, text, payload, enabled, created_at, last_fired
                 FROM schedules WHERE enabled != 0 ORDER BY name",
            )?;
            let rows = stmt.query_map([], Self::row_to_schedule)?;
            rows.filter_map(|r| r.ok()).collect()
        };
        let mut fired = Vec::new();
        let mut skipped = Vec::new();
        for entry in entries {
            let due = match proto::schedule::due_occurrence(&entry, now_ms) {
                Ok(Some(t)) => t,
                Ok(None) => continue, // not due yet
                Err(e) => {
                    skipped.push((entry.name.clone(), format!("bad schedule: {e}")));
                    continue;
                }
            };
            if let Err(e) = proto::schedule::validate_offer_payload(&entry.payload) {
                skipped.push((entry.name.clone(), format!("bad payload: {e}")));
                continue;
            }
            let mut env = Envelope::new(&entry.room, agent.clone(), Kind::TaskOffer);
            if !entry.text.is_empty() {
                env.text = Some(entry.text.clone());
            }
            env.payload = Some(entry.payload.clone());
            // One transaction: ensure the room, append the offer, record
            // the occurrence as last-fired. The store lock is held for
            // the whole tick, so no other tick can interleave.
            let tx = conn.transaction()?;
            tx.execute(
                "INSERT OR IGNORE INTO rooms (name, opened) VALUES (?1, ?2)",
                params![entry.room, now_ms],
            )?;
            let seq = self.assign_and_insert(&tx, &mut env, Kind::TaskOffer)?;
            tx.execute(
                "UPDATE schedules SET last_fired = ?1 WHERE name = ?2",
                params![due, entry.name],
            )?;
            tx.commit()?;
            fired.push(TickFired {
                schedule: entry.name,
                env,
                seq,
            });
        }
        Ok(TickOutcome { fired, skipped })
    }
}

/// One schedule fired by a tick: its name, the posted envelope, and the
/// room seq the offer was assigned.
pub struct TickFired {
    pub schedule: String,
    pub env: Envelope,
    pub seq: i64,
}

/// The result of one tick: what fired, and what was skipped (with reasons).
pub struct TickOutcome {
    pub fired: Vec<TickFired>,
    pub skipped: Vec<(String, String)>,
}

/// Fresh envelope with a server-grade id; used by broker-emitted messages.
pub fn broker_envelope(room: &str, kind: Kind, text: &str) -> Envelope {
    let mut env = Envelope::new(
        room,
        Agent {
            id: "agent-busd".into(),
            harness: "busd".into(),
            model: None,
            session: None,
            cwd: None,
            caps: vec![],
        },
        kind,
    );
    env.text = Some(text.into());
    env.trust = Some(Trust::SelfAgent);
    env.id = new_id();
    env.ts = iso_ts_now();
    env
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use proto::schedule::ScheduleEntry;

    fn ms(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> i64 {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, s)
            .unwrap()
            .timestamp_millis()
    }

    fn temp_store() -> (Store, std::path::PathBuf) {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let db = std::env::temp_dir().join(format!(
            "agent-bus-sched-test-{}-{}-{}.sqlite",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = Store::open(&db).unwrap();
        (store, db)
    }

    fn scheduler_agent() -> Agent {
        Agent {
            id: "scheduler@bus".into(),
            harness: "scheduler".into(),
            model: None,
            session: None,
            cwd: None,
            caps: vec![],
        }
    }

    fn entry(name: &str, cron: &str, created_ms: i64) -> ScheduleEntry {
        ScheduleEntry {
            name: name.into(),
            room: "#sched".into(),
            cron: cron.into(),
            timezone: "UTC".into(),
            text: "do the thing".into(),
            payload: serde_json::json!({"files": []}),
            enabled: true,
            created_at_ms: created_ms,
            last_fired_ms: None,
        }
    }

    #[test]
    fn schedule_crud_roundtrip() {
        let (store, db) = temp_store();
        let e = entry("digest", "0 8 * * *", ms(2026, 11, 8, 7, 59, 0));
        store.add_schedule(&e).unwrap();

        // Duplicate name is rejected.
        assert!(store.add_schedule(&e).is_err());

        let got = store.get_schedule("digest").unwrap().unwrap();
        assert_eq!(got, e);
        assert!(store.get_schedule("nope").unwrap().is_none());

        let list = store.list_schedules(None).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "digest");
        assert_eq!(store.list_schedules(Some("#other")).unwrap().len(), 0);
        assert_eq!(store.list_schedules(Some("#sched")).unwrap().len(), 1);

        assert!(store.set_schedule_enabled("digest", false).unwrap());
        assert!(!store.get_schedule("digest").unwrap().unwrap().enabled);
        assert!(store.set_schedule_enabled("digest", true).unwrap());
        assert!(!store.set_schedule_enabled("nope", true).unwrap());

        assert!(store.remove_schedule("digest").unwrap());
        assert!(!store.remove_schedule("digest").unwrap());
        assert!(store.list_schedules(None).unwrap().is_empty());
        let _ = std::fs::remove_file(db);
    }

    #[test]
    fn tick_fires_once_per_minute_skips_malformed_and_respects_disabled() {
        let (store, db) = temp_store();
        let agent = scheduler_agent();
        // Daily 08:00 UTC, created 07:59.
        store
            .add_schedule(&entry("digest", "0 8 * * *", ms(2026, 11, 8, 7, 59, 0)))
            .unwrap();
        // Malformed cron: must be skipped, never block the good schedule.
        store
            .add_schedule(&entry("broken", "not a cron", ms(2026, 11, 8, 7, 59, 0)))
            .unwrap();
        // Disabled schedule due at the same time: must not fire.
        let mut off = entry("off", "0 8 * * *", ms(2026, 11, 8, 7, 59, 0));
        off.enabled = false;
        store.add_schedule(&off).unwrap();

        // First tick at 08:00:30: digest fires once, broken is reported.
        let out = store
            .tick_schedules(&agent, ms(2026, 11, 8, 8, 0, 30))
            .unwrap();
        assert_eq!(out.fired.len(), 1);
        assert_eq!(out.fired[0].schedule, "digest");
        assert_eq!(out.fired[0].seq, 1);
        assert_eq!(out.fired[0].env.kind, "task.offer");
        assert_eq!(out.fired[0].env.from.id, "scheduler@bus");
        assert_eq!(out.fired[0].env.text.as_deref(), Some("do the thing"));
        assert_eq!(out.skipped.len(), 1);
        assert_eq!(out.skipped[0].0, "broken");
        // The offer is durable in history.
        let hist = store.history("#sched", 0, 10).unwrap();
        assert_eq!(hist.len(), 1);
        assert_eq!(hist[0].kind, "task.offer");
        // last_fired recorded as the occurrence time.
        assert_eq!(
            store.get_schedule("digest").unwrap().unwrap().last_fired_ms,
            Some(ms(2026, 11, 8, 8, 0, 0))
        );

        // Second tick in the same minute: nothing fires again.
        let out = store
            .tick_schedules(&agent, ms(2026, 11, 8, 8, 0, 45))
            .unwrap();
        assert!(out.fired.is_empty());
        assert_eq!(store.history("#sched", 0, 10).unwrap().len(), 1);

        // Tick 91 s after the occurrence: no catch-up.
        let out = store
            .tick_schedules(&agent, ms(2026, 11, 8, 8, 1, 31))
            .unwrap();
        assert!(out.fired.is_empty());
        assert_eq!(store.history("#sched", 0, 10).unwrap().len(), 1);

        // Next day 08:00:30: fires again (new occurrence).
        let out = store
            .tick_schedules(&agent, ms(2026, 11, 9, 8, 0, 30))
            .unwrap();
        assert_eq!(out.fired.len(), 1);
        assert_eq!(out.fired[0].seq, 2);
        let _ = std::fs::remove_file(db);
    }

    #[test]
    fn who_reports_status_caps_and_rooms() {
        // Presence (issue #9): who() marks live vs stale agents and
        // shows caps and room subscriptions.
        let (store, db) = temp_store();
        let mut live = scheduler_agent();
        live.id = "live@bus".into();
        live.harness = "codex".into();
        live.model = Some("gpt-x".into());
        live.caps = vec!["search".into(), "code".into()];
        store
            .upsert_agent(&live, proto::Role::Conversation, None)
            .unwrap();
        store.join("live@bus", "#repo:agent-bus").unwrap();
        store.join("live@bus", "#general").unwrap();

        let mut stale = scheduler_agent();
        stale.id = "stale@bus".into();
        stale.harness = "omp".into();
        store
            .upsert_agent(&stale, proto::Role::Protocol, None)
            .unwrap();
        // Backdate last_seen past the 120 s online window.
        {
            let conn = store.handle();
            let conn = conn.lock().unwrap();
            conn.execute(
                "UPDATE agents SET last_seen = ?1 WHERE id = 'stale@bus'",
                params![now_ms() - ONLINE_WINDOW_MS - 1000],
            )
            .unwrap();
        }

        let all = store.who(None).unwrap();
        assert_eq!(all.len(), 2);
        let live_row = all.iter().find(|a| a["id"] == "live@bus").unwrap();
        assert_eq!(live_row["status"], "online");
        assert_eq!(live_row["harness"], "codex");
        assert_eq!(live_row["model"], "gpt-x");
        assert_eq!(live_row["caps"], serde_json::json!(["search", "code"]));
        assert_eq!(
            live_row["rooms"],
            serde_json::json!(["#general", "#repo:agent-bus"])
        );
        let stale_row = all.iter().find(|a| a["id"] == "stale@bus").unwrap();
        assert_eq!(stale_row["status"], "offline");
        assert_eq!(stale_row["rooms"], serde_json::json!([]));

        // Room-scoped listing only returns members of that room.
        let scoped = store.who(Some("#general")).unwrap();
        assert_eq!(scoped.len(), 1);
        assert_eq!(scoped[0]["id"], "live@bus");

        // A heartbeat brings the stale agent back online.
        store.touch_agent("stale@bus").unwrap();
        let again = store.who(None).unwrap();
        let stale_row = again.iter().find(|a| a["id"] == "stale@bus").unwrap();
        assert_eq!(stale_row["status"], "online");
        let _ = std::fs::remove_file(db);
    }
}
