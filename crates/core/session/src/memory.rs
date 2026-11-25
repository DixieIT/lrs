//! Sessions in memory: the same behaviour as the SQLite store, nothing kept
//! after the process ends. For tests and throwaway runs.

use crate::{new_session_id, now_ms, title_of};
use protocol::{Role, SessionMessage, SessionRow};
use sdk::{Error, Result, SessionStore};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;

#[derive(Default)]
pub struct InMemorySessionStore {
    sessions: RwLock<HashMap<String, (SessionRow, Vec<SessionMessage>)>>,
    next_message: AtomicU64,
}

impl SessionStore for InMemorySessionStore {
    fn create(&self, cwd: &str) -> Result<SessionRow> {
        let now = now_ms();
        let row = SessionRow {
            id: new_session_id(), cwd: cwd.into(), title: None, created_at: now, updated_at: now, message_count: 0,
        };
        self.sessions.write().unwrap().insert(row.id.clone(), (row.clone(), Vec::new()));
        Ok(row)
    }

    /// Appending to a session that was never created is an error: dropping the
    /// message silently would start a turn with no prompt in its history.
    fn push(&self, session_id: &str, role: Role, content: String) -> Result<()> {
        self.append(session_id, SessionMessage {
            id: String::new(), role, content, created_at: 0, tool_calls: Vec::new(),
        })
    }

    /// A message without an id gets one; a tool result keeps the id of the
    /// call it answers, which the next request sends back to the provider.
    fn append(&self, session_id: &str, mut message: SessionMessage) -> Result<()> {
        let mut sessions = self.sessions.write().unwrap();
        let (row, messages) = sessions.get_mut(session_id)
            .ok_or_else(|| Error::InvalidInput(format!("unknown session {session_id}")))?;
        if message.id.is_empty() {
            message.id = format!("m-{}", self.next_message.fetch_add(1, Ordering::Relaxed));
        }
        let now = now_ms();
        if message.created_at == 0 {
            message.created_at = now;
        }
        if row.title.is_none() && message.role == Role::User {
            row.title = title_of(&message.content);
        }
        row.updated_at = now;
        messages.push(message);
        row.message_count = messages.len() as u64;
        Ok(())
    }

    fn history(&self, session_id: &str) -> Result<Vec<SessionMessage>> {
        Ok(self.sessions.read().unwrap().get(session_id).map(|(_, m)| m.clone()).unwrap_or_default())
    }

    fn list(&self, cwd: Option<&str>) -> Result<Vec<SessionRow>> {
        let mut rows: Vec<SessionRow> = self.sessions.read().unwrap().values()
            .filter(|(r, _)| cwd.is_none_or(|c| r.cwd == c))
            .map(|(r, _)| r.clone())
            .collect();
        rows.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then_with(|| b.id.cmp(&a.id)));
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_to_an_unknown_session_is_an_error_not_a_silent_drop() {
        let store = InMemorySessionStore::default();
        assert!(store.push("missing", Role::User, "hi".into()).is_err());
        let row = store.create(".").unwrap();
        store.push(&row.id, Role::User, "hi".into()).unwrap();
        assert_eq!(store.history(&row.id).unwrap().len(), 1);
    }

    #[test]
    fn append_keeps_tool_calls_and_the_answered_call_id() {
        let store = InMemorySessionStore::default();
        let row = store.create(".").unwrap();
        let call = protocol::ToolCall { id: "call_a".into(), name: "bash".into(), arguments: "{}".into() };
        store.append(&row.id, SessionMessage {
            id: String::new(), role: Role::Assistant, content: String::new(), created_at: 0,
            tool_calls: vec![call.clone()],
        }).unwrap();
        store.append(&row.id, SessionMessage {
            id: "call_a".into(), role: Role::Tool, content: "ok".into(), created_at: 0, tool_calls: vec![],
        }).unwrap();
        let history = store.history(&row.id).unwrap();
        assert!(history[0].id.starts_with("m-"), "an assistant message gets an id");
        assert_eq!(history[0].tool_calls, [call]);
        assert_eq!(history[1].id, "call_a", "a tool result keeps the id of its call");
    }

    #[test]
    fn the_row_tracks_title_count_and_order() {
        let store = InMemorySessionStore::default();
        let first = store.create("/p").unwrap();
        store.push(&first.id, Role::User, "fix the parser\nplease".into()).unwrap();
        store.push(&first.id, Role::User, "and the lexer".into()).unwrap();
        let rows = store.list(Some("/p")).unwrap();
        assert_eq!(rows[0].title.as_deref(), Some("fix the parser"), "the first user message names it");
        assert_eq!(rows[0].message_count, 2);
        assert!(store.list(Some("/other")).unwrap().is_empty());
    }
}
