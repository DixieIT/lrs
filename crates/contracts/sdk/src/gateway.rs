//! Resolves providers, validates credentials and streams model chunks.

use crate::{Error, Result};
use protocol::SessionMessage;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderInfo {
    pub id: String,
    pub name: String,
    pub base_url: String,
    /// Never carries an API key.
    pub configured: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub provider: String,
    pub context_limit: u64,
}

/// One chunk of an LLM response.
#[derive(Debug, Clone, PartialEq)]
pub enum Chunk {
    TextDelta(String),
    Thinking(String),
    ToolCallChunk {
        id: String,
        name: Option<String>,
        arguments_delta: String,
    },
    Usage {
        input_tokens: u64,
        output_tokens: u64,
    },
    Done,
}

/// Minimal stream abstraction: synchronous iterator of chunks. Component
/// adapters may implement it over HTTP/SSE; the core only pulls from it.
pub type Stream = Box<dyn Iterator<Item = Result<Chunk>> + Send>;

pub trait ModelGateway: Send + Sync {
    /// Stable registration id, used by undo and by select_gateway().
    fn id(&self) -> &str;
    fn providers(&self) -> Result<Vec<ProviderInfo>>;
    fn models(&self) -> Result<Vec<ModelInfo>>;
    /// Validate credentials against the provider before storing them.
    fn connect(&mut self, provider: &str, key: &str) -> Result<()>;

    /// Connection through a shared handle. Hosts that hold the gateway in an
    /// `Arc` override this with interior mutability; the default delegates to
    /// `connect` only for exclusive owners.
    fn connect_shared(&self, provider: &str, key: &str) -> Result<()> {
        Err(Error::Unavailable(format!(
            "shared connect unavailable for {provider}: {key_len} bytes",
            key_len = key.len()
        )))
    }
    fn default_model(&self) -> Option<String>;

    fn stream(
        &self,
        model: &str,
        messages: &[SessionMessage],
        tools: &[serde_json::Value],
    ) -> Result<Stream>;
}
