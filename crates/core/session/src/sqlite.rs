//! Sessions in one SQLite database.
//!
//! WAL journal, so a reader (`/resume`, `/recall`, another `lrs`) never waits
//! for a turn that is writing. Every message of every turn is one row, in
//! order, with its tool calls; a message and its session's `updated_at`,
//! title and count change in one transaction. User messages and assistant
//! answers are also indexed in FTS5 for recall.

use crate::recall::Recall;
use crate::{new_session_id, now_ms, title_of};
use protocol::{Role, SessionMessage, SessionRow, ToolCall};
use rusqlite::{params, Connection, OptionalExtension};
use sdk::{Error, Result, SessionStore};
use std::path::Path;
use std::sync::Mutex;

/// Bumped when the schema changes; `open` migrates from any older value.
const SCHEMA_VERSION: i64 = 1;

pub struct SqliteSessionStore {
    db: Mutex<Connection>,
}

fn unavailable(e: rusqlite::Error) -> Error {
    Error::Unavailable(format!("session store: {e}"))
}

fn role_name(role: Role) -> &'static str {
    match role {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::System => "system",
        Role::Tool => "tool",
    }
}

fn role_of(name: &str) -> Role {
    match name {
        "user" => Role::User,
        "system" => Role::System,
        "tool" => Role::Tool,
        _ => Role::Assistant,
    }
}

impl SqliteSessionStore {
    /// Open (creating it and its directory if needed) the database at `path`.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| Error::Unavailable(format!("session store directory {}: {e}", dir.display())))?;
        }
        Self::init(Connection::open(path).map_err(unavailable)?)
    }

    /// A database that lives as long as the store; for tests.
    pub fn in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory().map_err(unavailable)?)
    }

    fn init(db: Connection) -> Result<Self> {
        db.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;
             PRAGMA busy_timeout = 5000;",
        ).map_err(unavailable)?;
        let version: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0)).map_err(unavailable)?;
        if version < 1 {
            db.execute_batch(
                "BEGIN;
                 CREATE TABLE IF NOT EXISTS sessions (
                     id         TEXT PRIMARY KEY,
                     cwd        TEXT NOT NULL,
                     title      TEXT,
                     created_at INTEGER NOT NULL,
                     updated_at INTEGER NOT NULL,
                     message_count INTEGER NOT NULL DEFAULT 0
                 );
                 CREATE INDEX IF NOT EXISTS sessions_by_cwd ON sessions (cwd, updated_at DESC);
                 CREATE TABLE IF NOT EXISTS messages (
                     seq        INTEGER PRIMARY KEY AUTOINCREMENT,
                     session_id TEXT NOT NULL REFERENCES sessions (id) ON DELETE CASCADE,
                     id         TEXT NOT NULL,
                     role       TEXT NOT NULL,
                     content    TEXT NOT NULL,
                     tool_calls TEXT,
                     created_at INTEGER NOT NULL
                 );
                 CREATE INDEX IF NOT EXISTS messages_by_session ON messages (session_id, seq);
                 CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5 (
                     content,
                     session_id UNINDEXED,
                     role UNINDEXED,
                     tokenize = 'unicode61 remove_diacritics 2'
                 );
                 PRAGMA user_version = 1;
                 COMMIT;",
            ).map_err(unavailable)?;
        }
        debug_assert_eq!(SCHEMA_VERSION, 1, "add the migration from the previous version above");
        Ok(Self { db: Mutex::new(db) })
    }

    /// Past user messages and assistant answers that match `query`, best first.
    /// Every word must appear (in any order); accents and case are ignored.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<Recall>> {
        let terms: Vec<String> = query.split_whitespace()
            // Each word quoted: the user's text is never FTS5 syntax.
            .map(|w| format!("\"{}\"", w.replace('"', "\"\"")))
            .collect();
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        let db = self.db.lock().unwrap_or_else(|e| e.into_inner());
        let mut stmt = db.prepare(
            "SELECT f.session_id, s.title, s.cwd, f.role, s.updated_at,
                    snippet(messages_fts, 0, '«', '»', '…', 16)
             FROM messages_fts f JOIN sessions s ON s.id = f.session_id
             WHERE messages_fts MATCH ?1
             ORDER BY bm25(messages_fts)
             LIMIT ?2",
        ).map_err(unavailable)?;
        let rows = stmt.query_map(params![terms.join(" "), limit as i64], |r| {
            Ok(Recall {
                session_id: r.get(0)?,
                title: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                cwd: r.get(2)?,
                role: role_of(&r.get::<_, String>(3)?),
                updated_at: r.get(4)?,
                snippet: r.get(5)?,
            })
        }).map_err(unavailable)?;
        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(unavailable)
    }
}

impl SessionStore for SqliteSessionStore {
    fn create(&self, cwd: &str) -> Result<SessionRow> {
        let now = now_ms();
        let row = SessionRow {
            id: new_session_id(), cwd: cwd.into(), title: None, created_at: now, updated_at: now, message_count: 0,
        };
        self.db.lock().unwrap_or_else(|e| e.into_inner()).execute(
            "INSERT INTO sessions (id, cwd, created_at, updated_at) VALUES (?1, ?2, ?3, ?3)",
            params![row.id, row.cwd, now],
        ).map_err(unavailable)?;
        Ok(row)
    }

    fn push(&self, session_id: &str, role: Role, content: String) -> Result<()> {
        self.append(session_id, SessionMessage {
            id: String::new(), role, content, created_at: 0, tool_calls: Vec::new(),
        })
    }

    /// One transaction: the message, its search entry, and the session's
    /// `updated_at`, count and (first user message) title. An unknown session
    /// is an error, never a silent drop.
    fn append(&self, session_id: &str, message: SessionMessage) -> Result<()> {
        let mut db = self.db.lock().unwrap_or_else(|e| e.into_inner());
        let tx = db.transaction().map_err(unavailable)?;
        let title: Option<Option<String>> = tx.query_row(
            "SELECT title FROM sessions WHERE id = ?1", params![session_id], |r| r.get(0),
        ).optional().map_err(unavailable)?;
        let Some(title) = title else {
            return Err(Error::InvalidInput(format!("unknown session {session_id}")));
        };
        let now = now_ms();
        let created_at = if message.created_at == 0 { now } else { message.created_at };
        let tool_calls = if message.tool_calls.is_empty() {
            None
        } else {
            Some(serde_json::to_string(&message.tool_calls).map_err(|e| Error::Internal(e.to_string()))?)
        };
        tx.execute(
            "INSERT INTO messages (session_id, id, role, content, tool_calls, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![session_id, message.id, role_name(message.role), message.content, tool_calls, created_at],
        ).map_err(unavailable)?;
        if message.id.is_empty() {
            let seq = tx.last_insert_rowid();
            tx.execute("UPDATE messages SET id = ?1 WHERE seq = ?2", params![format!("m-{seq}"), seq])
                .map_err(unavailable)?;
        }
        // Recall covers what people asked and what they were told, not tool
        // output or a request that is only tool calls.
        let searchable = !message.content.trim().is_empty()
            && (message.role == Role::User || (message.role == Role::Assistant && message.tool_calls.is_empty()));
        if searchable {
            tx.execute(
                "INSERT INTO messages_fts (content, session_id, role) VALUES (?1, ?2, ?3)",
                params![message.content, session_id, role_name(message.role)],
            ).map_err(unavailable)?;
        }
        let new_title = match title {
            None if message.role == Role::User => title_of(&message.content),
            other => other,
        };
        tx.execute(
            "UPDATE sessions SET updated_at = ?1, message_count = message_count + 1, title = ?2 WHERE id = ?3",
            params![now, new_title, session_id],
        ).map_err(unavailable)?;
        tx.commit().map_err(unavailable)
    }

    fn history(&self, session_id: &str) -> Result<Vec<SessionMessage>> {
        let db = self.db.lock().unwrap_or_else(|e| e.into_inner());
        let mut stmt = db.prepare(
            "SELECT id, role, content, tool_calls, created_at FROM messages WHERE session_id = ?1 ORDER BY seq",
        ).map_err(unavailable)?;
        let rows = stmt.query_map(params![session_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?, r.get::<_, i64>(4)?))
        }).map_err(unavailable)?;
        let mut out = Vec::new();
        for row in rows {
            let (id, role, content, calls, created_at) = row.map_err(unavailable)?;
            let tool_calls: Vec<ToolCall> = match calls {
                Some(json) => serde_json::from_str(&json)
                    .map_err(|e| Error::Internal(format!("stored tool calls of {id}: {e}")))?,
                None => Vec::new(),
            };
            out.push(SessionMessage { id, role: role_of(&role), content, created_at, tool_calls });
        }
        Ok(out)
    }

    fn list(&self, cwd: Option<&str>) -> Result<Vec<SessionRow>> {
        let db = self.db.lock().unwrap_or_else(|e| e.into_inner());
        let mut stmt = db.prepare(
            "SELECT id, cwd, title, created_at, updated_at, message_count FROM sessions
             WHERE ?1 IS NULL OR cwd = ?1
             ORDER BY updated_at DESC, id DESC",
        ).map_err(unavailable)?;
        let rows = stmt.query_map(params![cwd], |r| {
            Ok(SessionRow {
                id: r.get(0)?, cwd: r.get(1)?, title: r.get(2)?,
                created_at: r.get(3)?, updated_at: r.get(4)?,
                message_count: r.get::<_, i64>(5)?.try_into().unwrap_or(0),
            })
        }).map_err(unavailable)?;
        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(unavailable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(role: Role, content: &str) -> SessionMessage {
        SessionMessage { id: String::new(), role, content: content.into(), created_at: 0, tool_calls: vec![] }
    }

    #[test]
    fn a_whole_trajectory_survives_closing_the_database() {
        let dir = std::env::temp_dir().join(format!("session-sqlite-{}", std::process::id()));
        let path = dir.join("sessions.db");
        if path.exists() {
            std::fs::remove_file(&path).unwrap();
        }
        let call = ToolCall { id: "call_1".into(), name: "read_file".into(), arguments: r#"{"path":"a"}"#.into() };
        let id = {
            let store = SqliteSessionStore::open(&path).unwrap();
            let row = store.create("/p").unwrap();
            store.push(&row.id, Role::User, "read a".into()).unwrap();
            store.append(&row.id, SessionMessage {
                id: String::new(), role: Role::Assistant, content: String::new(), created_at: 0,
                tool_calls: vec![call.clone()],
            }).unwrap();
            store.append(&row.id, SessionMessage {
                id: "call_1".into(), role: Role::Tool, content: "alpha".into(), created_at: 0, tool_calls: vec![],
            }).unwrap();
            store.push(&row.id, Role::Assistant, "it says alpha".into()).unwrap();
            row.id
        };
        // A new process: open the same file again.
        let store = SqliteSessionStore::open(&path).unwrap();
        let history = store.history(&id).unwrap();
        let shape: Vec<(Role, &str)> = history.iter().map(|m| (m.role, m.content.as_str())).collect();
        assert_eq!(shape, [(Role::User, "read a"), (Role::Assistant, ""), (Role::Tool, "alpha"), (Role::Assistant, "it says alpha")]);
        assert_eq!(history[1].tool_calls, [call]);
        assert_eq!(history[2].id, "call_1", "a tool result keeps its call id");
        assert!(history[0].id.starts_with("m-") && history[1].id.starts_with("m-"));
        let rows = store.list(Some("/p")).unwrap();
        assert_eq!((rows[0].title.as_deref(), rows[0].message_count), (Some("read a"), 4));
    }

    #[test]
    fn an_unknown_session_is_refused_and_nothing_is_written() {
        let store = SqliteSessionStore::in_memory().unwrap();
        assert!(matches!(store.push("nope", Role::User, "hi".into()), Err(Error::InvalidInput(_))));
        assert!(store.list(None).unwrap().is_empty());
    }

    #[test]
    fn the_newest_session_of_a_directory_comes_first() {
        let store = SqliteSessionStore::in_memory().unwrap();
        let old = store.create("/p").unwrap();
        let other = store.create("/q").unwrap();
        let new = store.create("/p").unwrap();
        store.push(&old.id, Role::User, "old".into()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        store.push(&new.id, Role::User, "new".into()).unwrap();
        let ids: Vec<String> = store.list(Some("/p")).unwrap().into_iter().map(|r| r.id).collect();
        assert_eq!(ids, [new.id.clone(), old.id.clone()]);
        assert_eq!(store.list(None).unwrap().len(), 3);
        assert!(!ids.contains(&other.id));
    }

    #[test]
    fn recall_finds_questions_and_answers_but_not_tool_output() {
        let store = SqliteSessionStore::in_memory().unwrap();
        let a = store.create("/p").unwrap();
        store.append(&a.id, message(Role::User, "come si configura il parser di Rust?")).unwrap();
        store.append(&a.id, message(Role::Tool, "parser internals dump")).unwrap();
        store.append(&a.id, message(Role::Assistant, "Il parser si configura in config.toml")).unwrap();
        let b = store.create("/q").unwrap();
        store.append(&b.id, message(Role::User, "deploy su Docker")).unwrap();

        let hits = store.search("parser config", 10).unwrap();
        assert!(!hits.is_empty() && hits.iter().all(|h| h.session_id == a.id), "{hits:?}");
        assert!(hits.iter().all(|h| h.role != Role::Tool), "tool output is not indexed");
        assert!(hits.iter().any(|h| h.snippet.contains("«parser»")), "{hits:?}");
        assert_eq!(store.search("docker", 10).unwrap()[0].session_id, b.id, "case-insensitive");
        assert_eq!(store.search("piu", 10).unwrap().len(), 0);
    }

    #[test]
    fn search_text_is_never_query_syntax_and_accents_do_not_matter() {
        let store = SqliteSessionStore::in_memory().unwrap();
        let s = store.create("/p").unwrap();
        store.append(&s.id, message(Role::User, "perché il caffè è amaro")).unwrap();
        assert_eq!(store.search("perche caffe", 5).unwrap().len(), 1, "diacritics ignored");
        for hostile in ["\"", "AND OR NOT", "a*", "(x", "NEAR(", ""] {
            assert!(store.search(hostile, 5).is_ok(), "{hostile:?} must not be parsed as FTS5 syntax");
        }
    }
}
