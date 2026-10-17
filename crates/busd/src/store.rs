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

    pub fn upsert_agent(&self, a: &Agent, role: proto::Role, token_hash: Option<&str>) -> Result<()> {
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
        conn.execute("UPDATE agents SET last_seen=?1 WHERE id=?2", params![now_ms(), id])?;
        Ok(())
    }

    pub fn agent_role(&self, id: &str) -> Result<Option<proto::Role>> {
        let conn = self.conn.lock().unwrap();
        let role: Option<String> = conn
            .query_row("SELECT role FROM agents WHERE id=?1", params![id], |r| r.get(0))
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
            .query_row("SELECT token_hash FROM agents WHERE id=?1", params![id], |r| r.get(0))
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
                h.bytes().zip(got.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
            }
            _ => false,
        }
    }

    pub fn who(&self, room: Option<&str>) -> Result<Vec<serde_json::Value>> {
        let conn = self.conn.lock().unwrap();
        let sql = match room {
            Some(room) => "SELECT a.id, a.harness, a.model, a.last_seen, a.role FROM agents a
                           JOIN members m ON m.agent = a.id WHERE m.room = ?1 ORDER BY a.id",
            None => "SELECT id, harness, model, last_seen, role FROM agents ORDER BY id",
        };
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map(params![room], |r| {
            Ok(serde_json::json!({
                "id": r.get::<_, String>(0)?,
                "harness": r.get::<_, String>(1)?,
                "model": r.get::<_, Option<String>>(2)?,
                "last_seen_ms": r.get::<_, i64>(3)?,
                "role": r.get::<_, String>(4)?,
            }))
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
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
        Ok(conn.query_row("SELECT seq FROM rooms WHERE name=?1", params![name], |r| r.get(0))?)
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
        conn.execute("DELETE FROM members WHERE agent=?1 AND room=?2", params![agent, room])?;
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

    // -- append (the sequencer) ---------------------------------------------

    /// Assign the next `seq` and insert, in one transaction, under the store
    /// lock. This is the only writer of a room's `seq` (single-writer rule).
    pub fn append(&self, env: &mut Envelope, kind: Kind) -> Result<i64> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
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
        tx.commit()?;
        env.seq = Some(seq);
        if env.trust.is_none() {
            env.trust = Some(Trust::Peer);
        }
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
