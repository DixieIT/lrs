//! Pure data structures exchanged between the runtime and its frontends.
//!
//! This crate knows nothing about transports, storage, or the SDK. Everything
//! here is serializable: in-memory for a desktop host, JSON/SSE for a TUI host.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Runs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunRequest {
    pub session_id: String,
    pub cwd: String,
    pub text: String,
    /// Model override for this turn. `None` uses the active gateway default.
    pub model: Option<String>,
    /// Effort override for this turn.
    pub effort: Option<String>,
    /// Images attached to the prompt, already encoded by the frontend.
    pub images: Vec<ImagePart>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImagePart {
    pub media_type: String,
    pub data: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunResponse {
    pub session_id: String,
    pub job_id: String,
}

// ---------------------------------------------------------------------------
// Event stream
// ---------------------------------------------------------------------------

/// One item of the stream a frontend consumes while a job runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    TextDelta { text: String },
    Thinking { text: String },
    ToolCallStart { id: String, name: String, args: serde_json::Value },
    ToolCallEnd { id: String, output: String, failed: bool },
    WidgetUpdated { widget: WidgetRow },
    JobFinished { session_id: String, job_id: String },
    JobFailed { session_id: String, job_id: String, error: String },
    ApprovalRequired { id: String, description: String },
    /// A steer typed mid-turn reached the running job's queue.
    Steered { text: String },
    /// A message the turn added to the conversation, exactly as the model
    /// sees it: an assistant reply or tool request, a tool result, a steer.
    /// The runtime persists these, so the next turn starts from the whole
    /// trajectory instead of the final text alone.
    MessageAdded { message: SessionMessage },
    Usage { input_tokens: u64, output_tokens: u64, context_limit: u64 },
    Heartbeat,
}

// ---------------------------------------------------------------------------
// Sessions
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionRow {
    pub id: String,
    pub cwd: String,
    /// From the first user message; `None` until there is one.
    pub title: Option<String>,
    /// Milliseconds since the Unix epoch.
    pub created_at: i64,
    pub updated_at: i64,
    #[serde(default)]
    pub message_count: u64,
}

/// One message of a conversation. For `Role::Tool`, `id` is the id of the
/// tool call this message answers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionMessage {
    pub id: String,
    pub role: Role,
    pub content: String,
    pub created_at: i64,
    /// Calls the assistant requested in this message; empty otherwise.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
}

/// A tool call as the model requested it. `arguments` is the raw JSON text
/// the model produced, kept verbatim so a provider receives exactly its own
/// call back on the next request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
    System,
    Tool,
}

// ---------------------------------------------------------------------------
// Commands and widgets
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommandSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema for the command's arguments, when it takes any.
    pub args_schema: Option<serde_json::Value>,
    /// Subcommand names, for menus that open on a space.
    pub subcommands: Vec<String>,
    /// Plugin that owns this command. `None` for a built-in.
    pub owner: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommandResult {
    pub text: String,
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WidgetRow {
    pub session_id: String,
    pub id: String,
    pub title: String,
    pub body: String,
}

// ---------------------------------------------------------------------------
// Plugins
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PluginDescriptor {
    pub id: String,
    pub name: String,
    pub version: String,
    pub category: String,
    pub enabled: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_round_trip_as_tagged_json() {
        let e = Event::ToolCallStart {
            id: "c1".into(),
            name: "read_file".into(),
            args: serde_json::json!({"path": "src/main.rs"}),
        };
        let json = serde_json::to_string(&e).unwrap();
        assert!(json.contains("\"type\":\"tool_call_start\""), "{json}");
        assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), e);
    }
}

// ---------------------------------------------------------------------------
// Host-facing snapshots (shared by runtime and frontends)
// ---------------------------------------------------------------------------

/// Capabilities reported at startup, used for the opening banner and gauges.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct Health {
    #[serde(default)]
    pub model: String,
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
}

/// One provider entry. Never carries a credential.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
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

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderList {
    #[serde(default)]
    pub providers: Vec<Provider>,
}

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelList {
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default)]
    pub current: String,
}

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelSet {
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub missing_key_env: Option<String>,
    #[serde(default)]
    pub context_limit: u64,
}

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct RegistryEntry {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub live: bool,
}

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct RegistrySnapshot {
    #[serde(default)]
    pub plugins: Vec<RegistryEntry>,
    #[serde(default)]
    pub extensions: Vec<RegistryEntry>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub content: serde_json::Value,
    #[serde(default)]
    pub tool_calls: Vec<serde_json::Value>,
}

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionMessages {
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub messages: Vec<Message>,
}
