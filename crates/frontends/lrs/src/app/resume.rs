//! Rebuilding a transcript from a stored session.
//!
//! A session is its event log, `~/.lele/events/<id>.jsonl`, and the messages the
//! model saw are derived from it — the same record the CLI and the Textual TUI
//! resume from. `GET /sessions/<id>` answers with those messages, so `lrs
//! -r` is that read plus this translation from engine messages back into the
//! blocks the UI draws.

use lrs::protocol::SessionMessages;
use crate::ui::block::{self, Block};

/// Turn stored messages into transcript blocks.
///
/// Tool results arrive as their own `role: "tool"` messages *after* the assistant
/// turn that requested them, so calls are queued as they are seen and filled in as
/// their results arrive — the order on the wire is not the order on screen.
pub fn blocks(session: &SessionMessages) -> Vec<Block> {
    let mut out: Vec<Block> = Vec::new();
    // Indices into `out` of tool blocks still waiting for their result, oldest first.
    let mut pending: Vec<usize> = Vec::new();

    for msg in &session.messages {
        match msg.role.as_str() {
            // The system prompt is machinery, not conversation.
            "system" => {}
            "user" => {
                let text = msg.text();
                if !text.is_empty() {
                    out.push(Block::User(text));
                }
            }
            "assistant" => {
                let text = msg.text();
                if !text.trim().is_empty() {
                    out.push(Block::Assistant(text));
                }
                for call in &msg.tool_calls {
                    let name = &call.function.name;
                    if name.is_empty() {
                        continue;
                    }
                    let args: serde_json::Value =
                        serde_json::from_str(&call.function.arguments).unwrap_or_default();
                    // `todo` has no transcript block live and none here either: its
                    // package folds the log when the session opens and republishes
                    // the panel, so a stored call would be a second copy of a list
                    // already on screen. It used to be shown because a resumed
                    // session had no panel to redraw.
                    if name == "todo_write" {
                        continue;
                    }
                    out.push(Block::Tool {
                        title: block::tool_title(name, &args),
                        body: None,
                        kind: block::body_kind(name),
                        failed: false,
                        started: None,
                        took: None,
                    });
                    pending.push(out.len() - 1);
                }
            }
            "tool" => {
                if pending.is_empty() {
                    continue; // a result whose call didn't survive compaction
                }
                let at = pending.remove(0);
                if let Block::Tool { body, .. } = &mut out[at] {
                    let text = msg.text();
                    *body = (!text.is_empty()).then_some(text);
                }
            }
            _ => {}
        }
    }
    out
}

/// The prompt to show when a resumed session is on screen.
pub fn summary(session: &SessionMessages, blocks: &[Block]) -> String {
    let turns = blocks.iter().filter(|b| matches!(b, Block::User(_))).count();
    format!("resumed {} — {turns} turn(s)", session.session_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(json: &str) -> SessionMessages {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn a_plain_exchange_becomes_user_and_assistant_blocks() {
        let s = session(
            r#"{"session_id":"s1","messages":[
                {"role":"system","content":"you are lele"},
                {"role":"user","content":"ciao"},
                {"role":"assistant","content":"ciao a te"}
            ]}"#,
        );
        let b = blocks(&s);
        assert_eq!(b.len(), 2, "the system prompt is not conversation");
        assert!(matches!(&b[0], Block::User(t) if t == "ciao"));
        assert!(matches!(&b[1], Block::Assistant(t) if t == "ciao a te"));
    }

    #[test]
    fn a_tool_result_lands_in_the_call_that_asked_for_it() {
        let s = session(
            r#"{"session_id":"s1","messages":[
                {"role":"user","content":"lista"},
                {"role":"assistant","content":null,"tool_calls":[
                    {"function":{"name":"bash","arguments":"{\"command\":\"ls\"}"}}]},
                {"role":"tool","content":"README.md"},
                {"role":"assistant","content":"c'è un README"}
            ]}"#,
        );
        let b = blocks(&s);
        assert_eq!(b.len(), 3);
        match &b[1] {
            Block::Tool { title, body, .. } => {
                assert!(title.contains("bash"), "got {title:?}");
                assert_eq!(body.as_deref(), Some("README.md"));
            }
            other => panic!("expected a tool block, got {other:?}"),
        }
    }

    #[test]
    fn two_calls_in_one_turn_get_their_own_results_in_order() {
        // Both results arrive after both calls; matching them by position is the
        // only thing the stored format gives us.
        let s = session(
            r#"{"session_id":"s1","messages":[
                {"role":"assistant","content":null,"tool_calls":[
                    {"function":{"name":"read_file","arguments":"{\"path\":\"a\"}"}},
                    {"function":{"name":"read_file","arguments":"{\"path\":\"b\"}"}}]},
                {"role":"tool","content":"contenuto a"},
                {"role":"tool","content":"contenuto b"}
            ]}"#,
        );
        let b = blocks(&s);
        let bodies: Vec<Option<&str>> = b
            .iter()
            .filter_map(|x| match x {
                Block::Tool { body, .. } => Some(body.as_deref()),
                _ => None,
            })
            .collect();
        assert_eq!(bodies, vec![Some("contenuto a"), Some("contenuto b")]);
    }

    #[test]
    fn an_orphan_tool_result_is_dropped_not_misfiled() {
        // Compaction can drop the assistant turn but keep the result; attaching it
        // to an unrelated call would show the wrong output under the wrong command.
        let s = session(
            r#"{"session_id":"s1","messages":[
                {"role":"tool","content":"orfano"},
                {"role":"user","content":"ciao"}
            ]}"#,
        );
        let b = blocks(&s);
        assert_eq!(b.len(), 1);
        assert!(matches!(&b[0], Block::User(_)));
    }

    #[test]
    fn an_assistant_turn_that_only_called_tools_adds_no_empty_bubble() {
        let s = session(
            r#"{"session_id":"s1","messages":[
                {"role":"assistant","content":"   ","tool_calls":[
                    {"function":{"name":"bash","arguments":"{}"}}]}
            ]}"#,
        );
        let b = blocks(&s);
        assert_eq!(b.len(), 1);
        assert!(matches!(&b[0], Block::Tool { .. }));
    }

    #[test]
    fn malformed_tool_arguments_do_not_lose_the_call() {
        // A truncated session file must still show that the tool ran.
        let s = session(
            r#"{"session_id":"s1","messages":[
                {"role":"assistant","content":null,"tool_calls":[
                    {"function":{"name":"bash","arguments":"{not json"}}]}
            ]}"#,
        );
        let b = blocks(&s);
        assert_eq!(b.len(), 1);
        assert!(matches!(&b[0], Block::Tool { .. }));
    }

    #[test]
    fn the_summary_counts_user_turns() {
        let s = session(
            r#"{"session_id":"20250101_120000_abc","messages":[
                {"role":"user","content":"uno"},
                {"role":"assistant","content":"a"},
                {"role":"user","content":"due"}
            ]}"#,
        );
        let b = blocks(&s);
        let line = summary(&s, &b);
        assert!(line.contains("20250101_120000_abc"), "got {line:?}");
        assert!(line.contains('2'), "got {line:?}");
    }
}
