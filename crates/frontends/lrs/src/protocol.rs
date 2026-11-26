//! The wire format spoken by `lele serve`.
//!
//! Every variant mirrors an event `serve.py` puts on a job's SSE stream. The
//! `Unknown` catch-all is load-bearing: the Python side will grow events before
//! this crate learns about them, and a frontend that dies on an unrecognised
//! event would make the two repos impossible to release independently.

use serde::{Deserialize, Serialize};

/// Fields the UI doesn't read yet are kept anyway: this enum is the written form
/// of the contract with `serve.py`, and dropping a field here would quietly turn a
/// protocol change into a silent no-op instead of a compile error.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum Event {
    #[serde(rename = "job_started")]
    JobStarted { job_id: String, session_id: String },

    #[serde(rename = "agent_started")]
    AgentStarted {
        #[serde(default)]
        agent_id: String,
        /// Set when the event came from a sub-agent child rather than the
        /// main agent — that is how a sub-agent's work is nested in the UI.
        #[serde(default)]
        parent_id: Option<String>,
        #[serde(default)]
        run_id: Option<String>,
    },

    #[serde(rename = "token")]
    Token {
        text: String,
        #[serde(default)]
        run_id: Option<String>,
    },

    #[serde(rename = "thinking")]
    Thinking {
        text: String,
        #[serde(default)]
        run_id: Option<String>,
    },

    #[serde(rename = "tool_started")]
    ToolStarted {
        name: String,
        #[serde(default)]
        args: serde_json::Value,
        /// Present when the call belongs to a sub-agent child.
        #[serde(default)]
        run_id: Option<String>,
    },

    #[serde(rename = "tool_finished")]
    ToolFinished {
        name: String,
        #[serde(default)]
        args: serde_json::Value,
        #[serde(default)]
        output: String,
        /// The call failed; its output says why.
        #[serde(default)]
        failed: bool,
        #[serde(default)]
        run_id: Option<String>,
    },

    #[serde(rename = "usage")]
    Usage {
        #[serde(default)]
        context: u64,
        #[serde(default)]
        turn_input: u64,
        #[serde(default)]
        turn_output: u64,
        /// The model's window, 0 when the backend does not know it.
        #[serde(default)]
        context_limit: u64,
    },

    /// Narration from an orchestration extension — no single sub-agent to attach
    /// it to, so it gets its own line.
    #[serde(rename = "phase")]
    Phase {
        #[serde(default)]
        title: String,
    },

    #[serde(rename = "log")]
    Log {
        #[serde(default)]
        message: String,
    },

    #[serde(rename = "agent_finished")]
    AgentFinished {
        #[serde(default)]
        result: String,
        #[serde(default)]
        run_id: Option<String>,
    },

    /// A plugin moved one of its panels. Pushed live so a widget updated by a
    /// tool mid-turn appears at once instead of on the next poll.
    #[serde(rename = "widget")]
    Widget {
        id: String,
        #[serde(default)]
        markup: Option<String>,
    },

    #[serde(rename = "job_finished")]
    JobFinished {
        /// Steered messages the turn ended before injecting. They become the
        /// next turn rather than being dropped.
        #[serde(default)]
        leftover_steers: Vec<String>,
    },

    /// A message typed mid-turn reached the server's queue.
    #[serde(rename = "steer_queued")]
    SteerQueued {
        #[serde(default)]
        text: String,
    },

    /// The loop took it off the queue and put it in the conversation. This is the
    /// only honest moment to stop showing it as pending — it can be a whole tool
    /// call after it was queued.
    #[serde(rename = "steer_consumed")]
    SteerConsumed {
        #[serde(default)]
        text: String,
    },

    #[serde(rename = "job_failed")]
    JobFailed {
        #[serde(default)]
        error: String,
    },

    #[serde(other)]
    Unknown,
}

/// Body of `POST /run`.
#[derive(Debug, Serialize)]
pub struct RunRequest {
    /// A plain string, or OpenAI content parts when the turn carries images —
    /// the engine's `step()` accepts either.
    pub task: serde_json::Value,
    pub cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// `Some("")` means effort explicitly off; `None` leaves the server's default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Herdr pane id passed through to serve so the herdr plugin (running in
    /// the shared server, not here) can self-report agent state to the right pane.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RunResponse {
    pub job_id: String,
    pub session_id: String,
}

/// `GET /health`. Doubles as the capability report the opening banner is built
/// from, so startup costs one round trip rather than two.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct Health {
    #[serde(default)]
    pub model: String,
    /// Auto-compact threshold in tokens. 0 when the server couldn't resolve one,
    /// in which case the gauge shows a raw count instead of inventing a ceiling.
    #[serde(default)]
    pub context_limit: u64,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub plugins: Vec<String>,
    #[serde(default)]
    pub extensions: Vec<String>,
    #[serde(default)]
    pub skills: Vec<String>,
    /// How tools are offered to the model: `ptc` (code mode) or `native`.
    /// Empty when the backend does not say.
    #[serde(default)]
    pub executor: String,
}

/// `GET /models`.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct ModelList {
    #[serde(default)]
    pub models: Vec<String>,
    /// What the server would use for a request that names no model.
    #[serde(default)]
    pub current: String,
}

/// One entry of `GET /providers`. The API key itself is never served.
///
/// `models` is unread today (the picker gets its refs from `/models`) but kept for
/// the same reason as [`Event`]'s spare fields: this file is the written form of
/// the contract, and dropping a field turns a future protocol change into a silent
/// no-op instead of a compile error.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct Provider {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub api_key_env: String,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub connected: bool,
    #[serde(default)]
    pub models: Vec<String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct ProviderList {
    #[serde(default)]
    pub providers: Vec<Provider>,
}

/// `POST /config/model`.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct ModelSet {
    #[serde(default)]
    pub model: String,
    /// The env var the chosen provider still needs. `Some` sends the UI straight
    /// into the connect flow instead of letting the next turn fail on a 401.
    #[serde(default)]
    pub missing_key_env: Option<String>,
    #[serde(default)]
    pub context_limit: u64,
}

/// One row of `GET /sessions`, used to resume the most recent conversation.
/// `title` is what a session picker would show; kept for when there is one.
///
/// `title` can arrive as JSON `null` — serve.py stores `None` when a session's
/// title hasn't been derived yet (old sessions, or a user message too short to
/// title). `#[serde(default)]` alone handles a *missing* field, not an explicit
/// `null`, so `deser_null_to_default` coerces both to an empty string.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct SessionRow {
    #[serde(default)]
    pub id: String,
    #[serde(default, deserialize_with = "deser_null_to_default")]
    pub title: String,
    #[serde(default)]
    pub message_count: u64,
}

/// Deserialize a JSON `null` as the type's default, falling back to the normal
/// deserializer for anything else. Used for `SessionRow::title`.
fn deser_null_to_default<'de, D, T>(deserializer: D) -> std::result::Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    let opt: Option<T> = Option::deserialize(deserializer)?;
    Ok(opt.unwrap_or_default())
}

/// `GET /sessions/<id>`. Messages keep the OpenAI shape the engine stores them in.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct SessionMessages {
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub messages: Vec<Message>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct Message {
    #[serde(default)]
    pub role: String,
    /// Absent on an assistant turn that only made tool calls, and occasionally a
    /// list of content parts rather than a string — hence `Value`, decoded by
    /// [`Message::text`].
    #[serde(default)]
    pub content: serde_json::Value,
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
}

impl Message {
    /// The message's text, flattening the content-parts form some providers use.
    pub fn text(&self) -> String {
        match &self.content {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Array(parts) => parts
                .iter()
                .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join(""),
            _ => String::new(),
        }
    }
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct ToolCall {
    #[serde(default)]
    pub function: ToolFunction,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct ToolFunction {
    #[serde(default)]
    pub name: String,
    /// A JSON *string*, per the OpenAI contract — not an object.
    #[serde(default)]
    pub arguments: String,
}

/// One plugin panel. `markup` is Rich console markup — see [`crate::ui::markup`].
#[derive(Debug, Default, Clone, Deserialize)]
pub struct WidgetRow {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub markup: String,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct WidgetList {
    #[serde(default)]
    pub widgets: Vec<WidgetRow>,
}

/// A plugin-registered slash command, with the subcommands it declared.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct CommandSpec {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub desc: String,
    /// argument → description. A `<placeholder>` key completes with the
    /// placeholder in place, ready to be typed over.
    #[serde(default)]
    pub sub: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct CommandList {
    #[serde(default)]
    pub commands: Vec<CommandSpec>,
}

/// What running a plugin command produced.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct CommandResult {
    /// Lines the handler printed, in Rich markup.
    #[serde(default)]
    pub output: Vec<String>,
    /// Textual-only calls the handler made that the server could not honour —
    /// a modal, say. Surfaced rather than silently dropped.
    #[serde(default)]
    pub unsupported: Vec<String>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub widgets: Vec<WidgetRow>,
    /// The session the handler's state was filed under — minted by the server if
    /// the caller didn't have one yet.
    #[serde(default)]
    pub session_id: String,
}

/// One discovered plugin or extension.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct RegistryEntry {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub path: String,
    /// What config.yaml says: active | disabled | available.
    #[serde(default)]
    pub state: String,
    /// Whether it is actually loaded right now. Differs from `state` between an
    /// enable and the reload that applies it.
    #[serde(default)]
    pub live: bool,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct Registry {
    #[serde(default)]
    pub plugins: Vec<RegistryEntry>,
    #[serde(default)]
    pub extensions: Vec<RegistryEntry>,
    /// Set when a reload failed; the lists are still the truth as of now.
    #[serde(default)]
    pub error: Option<String>,
}

/// `POST /registry`. `name` and `state` echo the request; only `message` is shown,
/// but they are kept for the same reason as [`Event`]'s spare fields — this file is
/// the written contract, and a dropped field turns a protocol change into a silent
/// no-op instead of a compile error.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct RegistrySet {
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub state: String,
}

/// The `{"error": "..."}` body every failing endpoint answers with.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct ApiError {
    #[serde(default)]
    pub error: String,
}

/// Split a model ref into its provider and the provider's own model id.
///
/// Two spellings name a provider. The engine's is `prefix:model`. The in-process
/// gateway's is `provider/model`, which counts only for a provider that is
/// connected: without a key `openai/gpt-4o` is just an OpenRouter id, which is
/// how the gateway routes it. Anything else belongs to openrouter, whose refs are
/// bare `vendor/model` ids.
pub fn split_ref<'a>(ref_: &'a str, providers: &[Provider]) -> (String, &'a str) {
    if let Some((prov, mid)) = ref_.split_once(':') {
        if providers.iter().any(|p| p.name == prov) {
            return (prov.to_string(), mid);
        }
    }
    if let Some((prov, mid)) = ref_.split_once('/') {
        if prov != "openrouter" && providers.iter().any(|p| p.name == prov && p.connected) {
            return (prov.to_string(), mid);
        }
    }
    ("openrouter".to_string(), ref_)
}

/// One `data:` payload from an SSE stream. Returns `None` for comments
/// (heartbeats) and for anything that fails to parse — a malformed frame must
/// not tear down a live turn.
pub fn parse_sse_line(line: &str) -> Option<Event> {
    let payload = line.strip_prefix("data:")?.trim();
    if payload.is_empty() || payload == "[DONE]" {
        return None;
    }
    serde_json::from_str(payload).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_token_frame() {
        let ev = parse_sse_line(r#"data: {"type":"token","text":"ciao","agent_id":"main"}"#);
        assert!(matches!(ev, Some(Event::Token { text, .. }) if text == "ciao"));
    }

    #[test]
    fn heartbeats_and_junk_are_skipped_not_fatal() {
        assert!(parse_sse_line(": heartbeat").is_none());
        assert!(parse_sse_line("data: {not json").is_none());
        assert!(parse_sse_line("data: [DONE]").is_none());
    }

    #[test]
    fn an_unknown_event_type_degrades_instead_of_failing() {
        // Python grows a new event; this build must keep streaming.
        let ev = parse_sse_line(r#"data: {"type":"something_new","x":1}"#);
        assert!(matches!(ev, Some(Event::Unknown)));
    }

    #[test]
    fn a_ref_without_a_known_prefix_belongs_to_openrouter() {
        let provs = vec![
            Provider { name: "anthropic".into(), ..Default::default() },
            Provider { name: "opencode-go".into(), ..Default::default() },
        ];
        assert_eq!(split_ref("anthropic:claude-opus-5", &provs), ("anthropic".into(), "claude-opus-5"));
        // openrouter refs are bare and full of slashes; the ':' rule must not fire
        assert_eq!(split_ref("z-ai/glm-4.6", &provs), ("openrouter".into(), "z-ai/glm-4.6"));
        // a prefix that isn't a configured provider is part of the model id
        assert_eq!(split_ref("weird:thing", &provs), ("openrouter".into(), "weird:thing"));
    }

    #[test]
    fn a_slash_ref_belongs_to_its_provider_only_while_that_provider_is_connected() {
        let provs = vec![
            Provider { name: "llamacpp".into(), connected: true, ..Default::default() },
            Provider { name: "openai".into(), connected: false, ..Default::default() },
        ];
        assert_eq!(
            split_ref("llamacpp/inkling/Inkling-2-Mini-UD-Q4_K_M.gguf", &provs),
            ("llamacpp".into(), "inkling/Inkling-2-Mini-UD-Q4_K_M.gguf"),
        );
        // No key for openai, so this is the OpenRouter id it has always been.
        assert_eq!(split_ref("openai/gpt-4o", &provs), ("openrouter".into(), "openai/gpt-4o"));
        let keyed = vec![Provider { name: "openai".into(), connected: true, ..Default::default() }];
        assert_eq!(split_ref("openai/gpt-4o", &keyed), ("openai".into(), "gpt-4o"));
    }

    #[test]
    fn a_message_flattens_content_parts_to_text() {
        let m: Message = serde_json::from_str(
            r#"{"role":"user","content":[{"type":"text","text":"ciao "},{"type":"text","text":"mondo"}]}"#,
        )
        .unwrap();
        assert_eq!(m.text(), "ciao mondo");
        // and an assistant turn that only called tools has no text at all
        let m: Message = serde_json::from_str(
            r#"{"role":"assistant","content":null,"tool_calls":[{"function":{"name":"bash","arguments":"{}"}}]}"#,
        )
        .unwrap();
        assert_eq!(m.text(), "");
        assert_eq!(m.tool_calls[0].function.name, "bash");
    }

    #[test]
    fn tool_finished_carries_args_for_diff_rendering() {
        let ev = parse_sse_line(
            r#"data: {"type":"tool_finished","name":"write_file","args":{"path":"a.rs"},"output":"ok"}"#,
        );
        match ev {
            Some(Event::ToolFinished { name, args, .. }) => {
                assert_eq!(name, "write_file");
                assert_eq!(args["path"], "a.rs");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn missing_optional_fields_still_parse() {
        // serve.py omits `args` on some paths; that must not drop the event.
        let ev = parse_sse_line(r#"data: {"type":"tool_started","name":"bash"}"#);
        assert!(matches!(ev, Some(Event::ToolStarted { name, .. }) if name == "bash"));
    }

    #[test]
    fn session_rows_decode_serve_payload_with_extra_fields() {
        // serve.py's /sessions returns rows carrying directory, timestamps, and
        // token counts that SessionRow doesn't model — serde must ignore them.
        let raw = r#"[{"id":"20260802_173504_1feab9","title":"ciao","directory":"/x","time_created":1785684904,"time_updated":1785684975,"message_count":12,"tokens_input":385157,"tokens_output":783}]"#;
        let rows: Vec<SessionRow> = serde_json::from_str(raw).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "20260802_173504_1feab9");
        assert_eq!(rows[0].message_count, 12);
    }
    #[test]
    fn session_row_title_null_decodes_to_empty() {
        // serve.py emits `"title": null` for sessions whose title was never
        // derived (old sessions, or a first user message too short to title).
        // SessionRow.title is a String — a bare #[serde(default)] only covers a
        // *missing* field, not an explicit null, so this would fail to decode.
        let raw = r#"[{"id":"20260713_122911_a662d4","title":null,"directory":"/x","time_created":1783940599,"time_updated":1783940599,"message_count":4,"tokens_input":0,"tokens_output":0}]"#;
        let rows: Vec<SessionRow> = serde_json::from_str(raw).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "20260713_122911_a662d4");
        assert_eq!(rows[0].title, "");
        assert_eq!(rows[0].message_count, 4);
    }
}
