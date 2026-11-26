//! Multi-provider `ModelGateway`: live catalog, credential probe, persistent
//! defaults and SSE streaming. All provider traffic goes through `ureq`.
//!
//! Host dependencies (`SecretVault`, `ConfigStore`) arrive through
//! `ctx.get(...)`; the package never imports a concrete persistence component.

use protocol::{CommandResult, CommandSpec, SessionMessage};
use sdk::{
    Chunk, Config, ConfigStore, Contribution, Ctx, Disposer, Error, FunctionPlugin,
    ModelGateway, ModelInfo, PluginForm, PluginManifest, ProviderInfo, RegistrationDecl,
    ResolverManifest, Result, SecretVault, SlashCommandHandler, Stream,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

const PROVIDER_KEY_PREFIX: &str = "gateway.key.";
const DEFAULT_MODEL_KEY: &str = "gateway.default_model";
/// Where a local llama.cpp server listens, overridable in the config store.
const LLAMACPP_URL_KEY: &str = "gateway.llamacpp.base_url";
const LLAMACPP_DEFAULT_URL: &str = "http://127.0.0.1:8080/v1";
const CACHE_TTL: Duration = Duration::from_secs(300);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// The most one reply may take in all. It is only there so that a connection the
/// server never answers does not keep its reader thread for ever; how long the
/// model may be silent is [`STREAM_IDLE_SECS`].
const STREAM_CEILING: Duration = Duration::from_secs(3600);
/// How long a reply may go without a single byte before it is given up on. The
/// first byte of a reply from a local model can be minutes away, while it reads a
/// long prompt, so this is a setting: `gateway.stream_idle_secs`.
const STREAM_IDLE_SECS: u64 = 600;
const STREAM_IDLE_KEY: &str = "gateway.stream_idle_secs";

#[derive(Debug, Clone)]
struct Endpoint {
    name: String,
    base_url: String,
    env: String,
    /// `anthropic` changes the wire shape; everything else is OpenAI-compatible.
    dialect: Dialect,
    /// A local server needs no key; a hosted provider does.
    key_required: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dialect {
    OpenAi,
    Anthropic,
}

pub struct Gateway {
    secrets: Arc<dyn SecretVault>,
    config: Arc<dyn ConfigStore>,
    endpoints: Vec<Endpoint>,
    catalog: RwLock<Option<(Instant, Vec<ModelInfo>)>>,
    /// What a local server reported as its window, per endpoint, with the time
    /// it was asked: a server can be restarted with another size.
    windows: Mutex<HashMap<String, (Instant, u64)>>,
    keys: Mutex<HashMap<String, String>>,
}

impl Gateway {
    pub fn new(secrets: Arc<dyn SecretVault>, config: Arc<dyn ConfigStore>) -> Self {
        let endpoints = vec![
            Endpoint {
                name: "openrouter".into(),
                base_url: "https://openrouter.ai/api/v1".into(),
                env: "OPENROUTER_API_KEY".into(),
                dialect: Dialect::OpenAi,
                key_required: true,
            },
            Endpoint {
                name: "openai".into(),
                base_url: "https://api.openai.com/v1".into(),
                env: "OPENAI_API_KEY".into(),
                dialect: Dialect::OpenAi,
                key_required: true,
            },
            Endpoint {
                name: "anthropic".into(),
                base_url: "https://api.anthropic.com/v1".into(),
                env: "ANTHROPIC_API_KEY".into(),
                dialect: Dialect::Anthropic,
                key_required: true,
            },
            // A llama.cpp `llama-server` (or any OpenAI-compatible local server).
            Endpoint {
                name: "llamacpp".into(),
                base_url: config.get(LLAMACPP_URL_KEY).ok().flatten()
                    .unwrap_or_else(|| LLAMACPP_DEFAULT_URL.into()),
                env: "LLAMACPP_API_KEY".into(),
                dialect: Dialect::OpenAi,
                key_required: false,
            },
        ];
        Self {
            secrets,
            config,
            endpoints,
            catalog: RwLock::new(None),
            windows: Mutex::new(HashMap::new()),
            keys: Mutex::new(HashMap::new()),
        }
    }

    fn endpoint(&self, provider: &str) -> Result<&Endpoint> {
        self.endpoints.iter().find(|e| e.name == provider)
            .ok_or_else(|| Error::InvalidInput(format!("unknown provider {provider}")))
    }

    fn stream_idle_limit(&self) -> Duration {
        let secs = self.config.get(STREAM_IDLE_KEY).ok().flatten()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(STREAM_IDLE_SECS);
        Duration::from_secs(secs)
    }

    /// The window a running llama.cpp server was started with. Its model list
    /// carries no sizes, but `/props` says it, and it is the size in use, not the
    /// one the model was trained for.
    fn llamacpp_window(&self, endpoint: &Endpoint) -> u64 {
        if let Some((at, n)) = self.windows.lock().unwrap().get(&endpoint.name) {
            if at.elapsed() < CACHE_TTL {
                return *n;
            }
        }
        let root = endpoint.base_url.trim_end_matches('/').trim_end_matches("/v1");
        let n = self.agent(CONNECT_TIMEOUT).ok()
            .and_then(|agent| agent.get(&format!("{root}/props")).call().ok())
            .and_then(|mut r| r.body_mut().read_to_string().ok())
            .and_then(|body| serde_json::from_str::<Value>(&body).ok())
            .map_or(0, |props| props_window(&props));
        self.windows.lock().unwrap().insert(endpoint.name.clone(), (Instant::now(), n));
        n
    }

    fn key(&self, provider: &str) -> Result<String> {
        if let Some(k) = self.keys.lock().unwrap().get(provider) {
            return Ok(k.clone());
        }
        match self.secrets.get(&format!("{PROVIDER_KEY_PREFIX}{provider}"))? {
            Some(k) => Ok(k),
            // A keyless endpoint is called without an Authorization header.
            None if self.endpoint(provider).is_ok_and(|e| !e.key_required) => Ok(String::new()),
            None => Err(Error::AuthRequired { provider: format!("{provider} (set {})", self.endpoint(provider).map(|e| e.env.as_str()).unwrap_or("API_KEY")) }),
        }
    }

    /// `openai/x` and `anthropic/x` are also OpenRouter ids: they go to the
    /// provider directly only when that provider has a key, else to OpenRouter.
    fn resolve_provider(&self, model: &str) -> Result<&Endpoint> {
        if let Some((provider, _)) = model.split_once('/') {
            if self.endpoints.iter().any(|e| e.name == provider) && self.key(provider).is_ok() {
                return self.endpoint(provider);
            }
        }
        self.endpoint("openrouter")
    }

    fn agent(&self, timeout: Duration) -> Result<ureq::Agent> {
        Ok(ureq::Agent::new_with_config(
            ureq::Agent::config_builder()
                .timeout_global(Some(timeout))
                .build(),
        ))
    }
}

impl ModelGateway for Gateway {
    fn id(&self) -> &str { "gateway" }

    fn providers(&self) -> Result<Vec<ProviderInfo>> {
        self.endpoints.iter().map(|e| {
            let configured = !e.key_required || self.secrets
                .get(&format!("{PROVIDER_KEY_PREFIX}{}", e.name))?
                .is_some();
            Ok(ProviderInfo {
                id: e.name.clone(),
                name: e.name.clone(),
                base_url: e.base_url.clone(),
                configured,
            })
        }).collect()
    }

    fn models(&self) -> Result<Vec<ModelInfo>> {
        if let Some((at, cached)) = self.catalog.read().unwrap().as_ref() {
            if at.elapsed() < CACHE_TTL {
                return Ok(cached.clone());
            }
        }
        let mut out = Vec::new();
        let mut last_error = None;
        for endpoint in &self.endpoints {
            let key = match self.key(&endpoint.name) {
                Ok(k) => k,
                Err(Error::AuthRequired { .. }) => continue,
                Err(e) => return Err(e),
            };
            let url = format!("{}/models", endpoint.base_url);
            let response = authorized(self.agent(CONNECT_TIMEOUT)?.get(&url), &key).call();
            match response {
                Ok(mut r) => match r.body_mut().read_to_string() {
                    Ok(body) => {
                        if let Ok(list) = serde_json::from_str::<ModelsResponse>(&body) {
                            for m in list.data {
                                let id = if endpoint.name == "openrouter" {
                                    m.id.clone()
                                } else {
                                    format!("{}/{}", endpoint.name, m.id)
                                };
                                out.push(ModelInfo {
                                    id,
                                    provider: endpoint.name.clone(),
                                    context_limit: m.context_window.unwrap_or(0),
                                });
                            }
                        }
                    }
                    Err(e) => last_error = Some(Error::Unavailable(e.to_string())),
                },
                Err(e) => last_error = Some(http_error(e)),
            }
        }
        if out.is_empty() {
            return Err(last_error.unwrap_or(Error::Unavailable("no model catalog".into())));
        }
        *self.catalog.write().unwrap() = Some((Instant::now(), out.clone()));
        Ok(out)
    }

    fn connect(&mut self, provider: &str, key: &str) -> Result<()> {
        self.connect_shared(provider, key)
    }

    fn connect_shared(&self, provider: &str, key: &str) -> Result<()> {
        let endpoint = self.endpoint(provider)?.clone();
        let url = format!("{}/models", endpoint.base_url);
        let response = authorized(self.agent(CONNECT_TIMEOUT)?.get(&url), key).call();
        if let Err(e) = response {
            let err = http_error(e);
            return match err {
                Error::AuthRequired { .. } => Err(Error::AuthRequired { provider: provider.into() }),
                other => Err(other),
            };
        }
        self.secrets.set(&format!("{PROVIDER_KEY_PREFIX}{provider}"), key)?;
        self.keys.lock().unwrap().insert(provider.into(), key.into());
        *self.catalog.write().unwrap() = None;
        Ok(())
    }

    fn default_model(&self) -> Option<String> {
        self.config.get(DEFAULT_MODEL_KEY).ok().flatten()
    }

    fn context_limit(&self, model: &str) -> u64 {
        let Ok(endpoint) = self.resolve_provider(model) else { return 0 };
        if endpoint.name == "llamacpp" {
            return self.llamacpp_window(endpoint);
        }
        self.models().ok()
            .and_then(|list| list.into_iter().find(|m| m.id == model))
            .map_or(0, |m| m.context_limit)
    }

    fn stream(
        &self,
        model: &str,
        messages: &[SessionMessage],
        tools: &[Value],
    ) -> Result<Stream> {
        let endpoint = self.resolve_provider(model)?.clone();
        let key = self.key(&endpoint.name)?;
        // OpenRouter ids keep their vendor prefix; a direct provider drops it.
        let wire_model = if endpoint.name == "openrouter" {
            model
        } else {
            model.split_once('/').map(|(_, m)| m).unwrap_or(model)
        };
        let body = match endpoint.dialect {
            Dialect::OpenAi => openai_body(wire_model, messages, tools),
            // Anthropic takes system instructions as a top-level field, not a message.
            Dialect::Anthropic => json!({
                "model": wire_model,
                "stream": true,
                "max_tokens": 8192,
                "system": messages.iter().filter(|m| m.role == protocol::Role::System)
                    .map(|m| m.content.as_str()).collect::<Vec<_>>().join("\n\n"),
                "messages": messages.iter().filter(|m| m.role != protocol::Role::System)
                    .map(|m| json!({"role": role_name(m.role), "content": m.content})).collect::<Vec<_>>(),
            }),
        };
        let url = format!("{}/chat/completions", endpoint.base_url);
        let agent = self.agent(STREAM_CEILING)?;
        let idle_limit = self.stream_idle_limit();
        let dialect = endpoint.dialect;
        Background::spawn(idle_limit, move |send| {
            let response = authorized(agent.post(&url), &key)
                .header("Content-Type", "application/json")
                .send_json(&body);
            // A failed request is the stream's only item; a good one is read
            // line by line as the provider sends it, so the runner (and the
            // screen) get each token when it is generated.
            let items: Box<dyn Iterator<Item = Result<Chunk>>> = match response {
                Ok(response) => Box::new(SseStream::new(
                    std::io::BufReader::new(response.into_body().into_reader()),
                    dialect,
                )),
                Err(e) => Box::new(std::iter::once(Err(http_error(e)))),
            };
            for item in items {
                if !send(item) {
                    break;
                }
            }
        })
    }
}

#[derive(Deserialize)]
struct ModelsResponse {
    #[serde(default)]
    data: Vec<ModelEntry>,
}

#[derive(Deserialize)]
struct ModelEntry {
    id: String,
    #[serde(default)]
    context_window: Option<u64>,
}

/// `n_ctx` out of a llama.cpp `/props` reply, 0 when it is not there.
fn props_window(props: &Value) -> u64 {
    props["default_generation_settings"]["n_ctx"].as_u64().unwrap_or(0)
}

/// An OpenAI-compatible chat request. Tool definitions are wrapped as
/// functions and left out when there are none; an assistant message carries its
/// tool calls, and a tool message names the call it answers.
fn openai_body(model: &str, messages: &[SessionMessage], tools: &[Value]) -> Value {
    let messages: Vec<Value> = messages.iter().map(|m| {
        let mut out = json!({"role": role_name(m.role), "content": m.content});
        if !m.tool_calls.is_empty() {
            out["tool_calls"] = m.tool_calls.iter().map(|c| json!({
                "id": c.id,
                "type": "function",
                "function": {"name": c.name, "arguments": c.arguments},
            })).collect();
        }
        if m.role == protocol::Role::Tool {
            out["tool_call_id"] = json!(m.id);
        }
        out
    }).collect();
    let mut body = json!({
        "model": model,
        "stream": true,
        "stream_options": {"include_usage": true},
        "messages": messages,
    });
    if !tools.is_empty() {
        body["tools"] = tools.iter().map(|t| json!({"type": "function", "function": t})).collect();
    }
    body
}

/// Adds the bearer key, or nothing at all for a keyless local server.
fn authorized<B>(request: ureq::RequestBuilder<B>, key: &str) -> ureq::RequestBuilder<B> {
    if key.is_empty() {
        request
    } else {
        request.header("Authorization", &format!("Bearer {key}"))
    }
}

fn role_name(role: protocol::Role) -> &'static str {
    match role {
        protocol::Role::User => "user",
        protocol::Role::Assistant => "assistant",
        protocol::Role::System => "system",
        protocol::Role::Tool => "tool",
    }
}

/// SSE lines → SDK chunks, one line at a time. Tool arguments arrive as
/// partial JSON strings and are forwarded verbatim; the runner joins them per
/// call id.
struct SseParser {
    dialect: Dialect,
    /// OpenAI sends a call's id only in its first delta; later deltas carry
    /// just the call's `index`. Every chunk is given the id of its index.
    call_ids: HashMap<u64, String>,
    /// `Done` was produced: nothing after it counts.
    done: bool,
    /// Inside an inline `<think…>` block of the answer text.
    in_think: bool,
    /// The start of what may be a think tag split across chunks.
    carry: String,
}

impl SseParser {
    fn new(dialect: Dialect) -> Self {
        Self { dialect, call_ids: HashMap::new(), done: false, in_think: false, carry: String::new() }
    }

    /// The chunks one line of the body carries.
    fn line(&mut self, line: &str) -> Vec<Result<Chunk>> {
        let mut out = Vec::new();
        if self.done {
            return out;
        }
        let Some(data) = line.strip_prefix("data:") else { return out };
        let data = data.trim();
        if data == "[DONE]" {
            out.extend(self.end());
            return out;
        }
        let Ok(value) = serde_json::from_str::<Value>(data) else { return out };
        if value.get("error").is_some() {
            out.push(Err(map_provider_error(&value)));
            return out;
        }
        match self.dialect {
            Dialect::OpenAi => {
                if let Some(usage) = value.get("usage").filter(|u| u.is_object()) {
                    out.push(Ok(Chunk::Usage {
                        input_tokens: usage["prompt_tokens"].as_u64().unwrap_or(0),
                        output_tokens: usage["completion_tokens"].as_u64().unwrap_or(0),
                    }));
                }
                let Some(choices) = value.get("choices").and_then(Value::as_array) else { return out };
                for choice in choices {
                    let delta = &choice["delta"];
                    if let Some(t) = delta["content"].as_str() {
                        out.extend(self.content(t).into_iter().map(Ok));
                    }
                    if let Some(t) = delta["reasoning_content"].as_str() {
                        if !t.is_empty() { out.push(Ok(Chunk::Thinking(t.into()))); }
                    }
                    if let Some(calls) = delta["tool_calls"].as_array() {
                        for call in calls {
                            let index = call["index"].as_u64().unwrap_or(0);
                            let id = match call["id"].as_str().filter(|id| !id.is_empty()) {
                                Some(id) => {
                                    self.call_ids.insert(index, id.to_string());
                                    id.to_string()
                                }
                                None => self.call_ids.get(&index).cloned()
                                    .unwrap_or_else(|| format!("call-{index}")),
                            };
                            let name = call["function"]["name"].as_str().map(str::to_string);
                            let args = call["function"]["arguments"].as_str().unwrap_or_default().to_string();
                            out.push(Ok(Chunk::ToolCallChunk { id, name, arguments_delta: args }));
                        }
                    }
                    if choice.get("finish_reason").and_then(Value::as_str) == Some("length") {
                        out.extend(self.end());
                    }
                }
            }
            Dialect::Anthropic => {
                match value["type"].as_str().unwrap_or_default() {
                    "content_block_delta" => {
                        let d = &value["delta"];
                        if d["type"].as_str() == Some("text_delta") {
                            if let Some(t) = d["text"].as_str() {
                                out.push(Ok(Chunk::TextDelta(t.into())));
                            }
                        }
                    }
                    "message_delta" => {
                        if let Some(u) = value["usage"]["output_tokens"].as_u64() {
                            out.push(Ok(Chunk::Usage { input_tokens: 0, output_tokens: u }));
                        }
                    }
                    "message_stop" => out.extend(self.end()),
                    _ => {}
                }
            }
        }
        out
    }

    /// The body ended: a stream that never said it was done still is.
    fn finish(&mut self) -> Vec<Result<Chunk>> {
        if self.done { Vec::new() } else { self.end() }
    }

    /// Text held back as a possible tag start, then `Done`.
    fn end(&mut self) -> Vec<Result<Chunk>> {
        self.done = true;
        let mut out = Vec::new();
        let held = std::mem::take(&mut self.carry);
        if !held.is_empty() {
            out.push(Ok(self.text(held)));
        }
        out.push(Ok(Chunk::Done));
        out
    }

    fn text(&self, text: String) -> Chunk {
        if self.in_think { Chunk::Thinking(text) } else { Chunk::TextDelta(text) }
    }

    /// Answer text, with inline reasoning moved out of it. Some models write
    /// their reasoning in the content as `<think>…</think>` (the opening tag
    /// is often in the prompt template, so only the closing one appears), and
    /// some tag variants carry a suffix (`</think:6124c78e>`). Inside a think
    /// block the text is `Thinking`; the tags themselves are dropped; any
    /// other `<` is ordinary text. A tag split across chunks is held back
    /// until it can be told apart.
    fn content(&mut self, delta: &str) -> Vec<Chunk> {
        const OPEN: &str = "<think";
        const CLOSE: &str = "</think";
        // Longest tag kept waiting for its `>`: a suffix like `:6124c78e` fits.
        const MAX_TAG: usize = 64;
        let mut rest = std::mem::take(&mut self.carry);
        rest.push_str(delta);
        let mut out = Vec::new();
        let mut plain = String::new();
        while let Some(lt) = rest.find('<') {
            plain.push_str(&rest[..lt]);
            let tail = &rest[lt..];
            let tag = [OPEN, CLOSE].into_iter().find(|t| tail.starts_with(t));
            let prefix_of_tag = OPEN.starts_with(tail) || CLOSE.starts_with(tail);
            match (tag, tail.find('>')) {
                // `<think…>` or `</think…>` complete: `think`, then `>` or `:suffix>`.
                (Some(name), Some(gt)) if matches!(tail[name.len()..].chars().next(), Some('>' | ':')) => {
                    if !plain.is_empty() {
                        out.push(self.text(std::mem::take(&mut plain)));
                    }
                    self.in_think = name == OPEN;
                    rest = tail[gt + 1..].to_string();
                }
                // Not yet decidable: hold it for the next chunk.
                (Some(_), None) if tail.len() < MAX_TAG => {
                    self.carry = tail.to_string();
                    rest = String::new();
                    break;
                }
                (None, _) if prefix_of_tag => {
                    self.carry = tail.to_string();
                    rest = String::new();
                    break;
                }
                // Any other `<`.
                _ => {
                    plain.push('<');
                    rest = tail[1..].to_string();
                }
            }
        }
        plain.push_str(&rest);
        if !plain.is_empty() {
            out.push(self.text(plain));
        }
        out
    }
}

/// A whole SSE body at once. The streaming path and the tests share the parser.
#[cfg(test)]
fn parse_sse(raw: &str, dialect: Dialect) -> Vec<Result<Chunk>> {
    let mut parser = SseParser::new(dialect);
    let mut out: Vec<Result<Chunk>> = raw.lines().flat_map(|l| parser.line(l)).collect();
    out.extend(parser.finish());
    out
}

/// How long a stream waits for its next chunk before it says so.
const IDLE_POLL: Duration = Duration::from_millis(200);

/// A response read on its own thread, so the consumer is never stuck inside a
/// blocking socket read: the request, the wait for the first byte and every
/// later read happen there, and the consumer sees [`Chunk::Idle`] whenever
/// nothing has arrived for a moment. That is what lets a cancel interrupt a
/// model that is still reading a long prompt.
///
/// Dropping the stream drops the channel. The reader notices at the next chunk
/// it delivers, stops, and closes the connection; a read already in progress
/// ends when the server sends something or the stream ceiling runs out.
///
/// A reply that stays silent for `idle_limit` is an error. Time counts from the
/// last thing the server sent, not from the start, so a long answer that keeps
/// arriving is never cut, and `Idle` chunks, which are this side talking, do not
/// reset it.
struct Background {
    received: std::sync::mpsc::Receiver<Result<Chunk>>,
    idle_limit: Duration,
    last_heard: Instant,
    gave_up: bool,
}

impl Background {
    fn spawn(
        idle_limit: Duration,
        work: impl FnOnce(&dyn Fn(Result<Chunk>) -> bool) + Send + 'static,
    ) -> Result<Stream> {
        let (sender, received) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("gateway-stream".into())
            // `false` tells the reader nobody is listening any more.
            .spawn(move || work(&|item| sender.send(item).is_ok()))
            .map_err(|e| Error::Unavailable(format!("could not start the response reader: {e}")))?;
        Ok(Box::new(Self { received, idle_limit, last_heard: Instant::now(), gave_up: false }))
    }
}

impl Iterator for Background {
    type Item = Result<Chunk>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.gave_up {
            return None;
        }
        match self.received.recv_timeout(IDLE_POLL) {
            Ok(item) => {
                self.last_heard = Instant::now();
                Some(item)
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if self.last_heard.elapsed() >= self.idle_limit {
                    self.gave_up = true;
                    return Some(Err(Error::Unavailable(format!(
                        "no answer from the model for {} s (setting {STREAM_IDLE_KEY})",
                        self.idle_limit.as_secs()
                    ))));
                }
                Some(Ok(Chunk::Idle))
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => None,
        }
    }
}

/// The live body as an iterator of chunks: each `next` reads only as far as
/// the next line that carries one.
struct SseStream<R> {
    reader: R,
    parser: SseParser,
    ready: std::collections::VecDeque<Result<Chunk>>,
    ended: bool,
}

impl<R: std::io::BufRead> SseStream<R> {
    fn new(reader: R, dialect: Dialect) -> Self {
        Self { reader, parser: SseParser::new(dialect), ready: Default::default(), ended: false }
    }
}

impl<R: std::io::BufRead> Iterator for SseStream<R> {
    type Item = Result<Chunk>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(chunk) = self.ready.pop_front() {
                return Some(chunk);
            }
            if self.ended {
                return None;
            }
            let mut line = String::new();
            match self.reader.read_line(&mut line) {
                Ok(0) => {
                    self.ended = true;
                    self.ready.extend(self.parser.finish());
                }
                Ok(_) => {
                    self.ready.extend(self.parser.line(line.trim_end_matches(['\r', '\n'])));
                    self.ended = self.parser.done;
                }
                Err(e) => {
                    self.ended = true;
                    self.ready.push_back(Err(Error::Unavailable(format!("stream interrupted: {e}"))));
                }
            }
        }
    }
}

fn map_provider_error(value: &Value) -> Error {
    let message = value["error"]["message"].as_str()
        .or_else(|| value["message"].as_str())
        .unwrap_or("provider error");
    let status = value["error"]["status"].as_u64().unwrap_or(0);
    if status == 401 || message.contains("authentication") {
        Error::AuthRequired { provider: "provider".into() }
    } else if status == 429 || message.contains("rate limit") {
        Error::RateLimited { provider: "provider".into(), retry_after: None }
    } else {
        Error::Unavailable(message.into())
    }
}

fn http_error(error: ureq::Error) -> Error {
    match error {
        ureq::Error::StatusCode(code) if code == 401 || code == 403 => {
            Error::AuthRequired { provider: "provider".into() }
        }
        ureq::Error::StatusCode(code) if code == 429 => {
            Error::RateLimited { provider: "provider".into(), retry_after: None }
        }
        ureq::Error::StatusCode(code) if code >= 400 && code < 500 => {
            Error::InvalidInput(format!("provider rejected request ({code})"))
        }
        other => Error::Unavailable(other.to_string()),
    }
}

/// `/model <ref>` — validates against the live catalog and persists the choice.
struct ModelCommand {
    gateway: Arc<Gateway>,
}

impl SlashCommandHandler for ModelCommand {
    fn spec(&self) -> CommandSpec {
        CommandSpec {
            name: "model".into(),
            description: "Show or change the active model".into(),
            args_schema: Some(json!({"type": "string"})),
            subcommands: vec![],
            owner: Some("gateway".into()),
        }
    }

    fn execute(&self, ctx: &sdk::CommandContext) -> Result<CommandResult> {
        if ctx.arg.is_empty() {
            return Ok(CommandResult {
                text: self.gateway.default_model().unwrap_or_else(|| "none".into()),
                session_id: None,
            });
        }
        let ref_ = ctx.arg.trim();
        let valid = self.gateway.models()?.iter().any(|m| m.id == ref_);
        if !valid {
            return Err(Error::InvalidInput(format!("unknown model {ref_}")));
        }
        self.gateway.config.set(DEFAULT_MODEL_KEY, ref_)?;
        Ok(CommandResult { text: ref_.into(), session_id: None })
    }
}

/// Delegating handle so the same instance can back the gateway registration
/// and the `/model` command without duplicating state.
struct SharedGateway(Arc<Gateway>);

impl ModelGateway for SharedGateway {
    fn id(&self) -> &str { self.0.id() }
    fn providers(&self) -> Result<Vec<ProviderInfo>> { self.0.providers() }
    fn models(&self) -> Result<Vec<ModelInfo>> { self.0.models() }
    fn connect(&mut self, p: &str, k: &str) -> Result<()> { self.0.connect_shared(p, k) }
    fn connect_shared(&self, p: &str, k: &str) -> Result<()> { self.0.connect_shared(p, k) }
    fn default_model(&self) -> Option<String> { self.0.default_model() }
    fn context_limit(&self, m: &str) -> u64 { self.0.context_limit(m) }
    fn stream(&self, m: &str, msgs: &[SessionMessage], tools: &[Value]) -> Result<Stream> {
        self.0.stream(m, msgs, tools)
    }
}

/// `/connect <provider> <key>` — probes the provider and persists the key.
struct ConnectCommand {
    gateway: Arc<Gateway>,
}

impl SlashCommandHandler for ConnectCommand {
    fn spec(&self) -> CommandSpec {
        CommandSpec {
            name: "connect".into(),
            description: "Connect a provider with an API key".into(),
            args_schema: Some(json!({"type": "object", "properties": {
                "provider": {"type": "string"}, "key": {"type": "string"}
            }})),
            subcommands: vec![],
            owner: Some("gateway".into()),
        }
    }

    fn execute(&self, ctx: &sdk::CommandContext) -> Result<CommandResult> {
        let mut parts = ctx.arg.split_whitespace();
        let provider = parts.next().ok_or_else(|| {
            Error::InvalidInput("usage: /connect <provider> <key>".into())
        })?;
        let key = parts.next().ok_or_else(|| {
            Error::InvalidInput("usage: /connect <provider> <key>".into())
        })?;
        self.gateway.connect_shared(provider, key)?;
        // A successful connection with no explicit model must still leave a
        // usable default: resolve the live catalog and pin its first entry.
        let mut text = format!("connected {provider}");
        if self.gateway.default_model().is_none() {
            match self.gateway.models() {
                Ok(models) if !models.is_empty() => {
                    let first = models[0].id.clone();
                    self.gateway.config.set(DEFAULT_MODEL_KEY, &first)?;
                    text.push_str(&format!("; model {first}"));
                }
                Ok(_) => text.push_str("; no models available, use /model"),
                Err(e) => text.push_str(&format!("; catalog unavailable: {e}")),
            }
        }
        Ok(CommandResult { text, session_id: None })
    }
}

impl PluginManifest for Gateway {
    fn manifest() -> ResolverManifest {
        ResolverManifest {
            id: "gateway".into(),
            name: "gateway".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            form: PluginForm::Function,
            registrations: vec![
                RegistrationDecl::Gateway { name: "gateway".into() },
                RegistrationDecl::Command { name: "model".into() },
                RegistrationDecl::Command { name: "connect".into() },
            ],
            config_schema: None,
        }
    }
}

pub struct GatewayPlugin;

impl FunctionPlugin for GatewayPlugin {
    fn name(&self) -> &str { "gateway" }
    fn registrations(&self) -> Vec<RegistrationDecl> { Gateway::manifest().registrations }

    fn apply(&self, _config: &Config, ctx: &mut dyn Ctx) -> Result<Disposer> {
        let vault = ctx.get(sdk::SECRET_VAULT)
            .ok_or_else(|| Error::Unavailable("gateway needs SecretVault injection".into()))?;
        let secrets: Arc<dyn SecretVault> = vault
            .downcast_ref::<Arc<dyn SecretVault>>()
            .cloned()
            .ok_or_else(|| Error::InvalidInput("secrets dependency has wrong type".into()))?;
        let store = ctx.get(sdk::CONFIG_STORE)
            .ok_or_else(|| Error::Unavailable("gateway needs ConfigStore injection".into()))?;
        let config: Arc<dyn ConfigStore> = store
            .downcast_ref::<Arc<dyn ConfigStore>>()
            .cloned()
            .ok_or_else(|| Error::InvalidInput("config dependency has wrong type".into()))?;

        let gateway = Arc::new(Gateway::new(secrets, config));
        let mut d = Disposer::default();
        d.push(ctx.effect(Contribution::Gateway {
            gateway: Box::new(SharedGateway(gateway.clone())),
        })?);
        d.push(ctx.effect(Contribution::Command {
            handler: Box::new(ModelCommand { gateway: gateway.clone() }),
        })?);
        d.push(ctx.effect(Contribution::Command {
            handler: Box::new(ConnectCommand { gateway }),
        })?);
        Ok(d)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_split_openai_tool_arguments_and_content() {
        let raw = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
                   data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"id\":\"c1\",\"function\":{\"name\":\"echo\",\"arguments\":\"{\\\"te\"}}]}}]}\n\n\
                   data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"id\":\"c1\",\"function\":{\"arguments\":\"xt\\\":\\\"yo\\\"}\"}}]}}]}\n\n\
                   data: [DONE]";
        let chunks = parse_sse(raw, Dialect::OpenAi);
        assert!(chunks.iter().any(|c| matches!(c, Ok(Chunk::TextDelta(t)) if t == "hi")));
        let mut accumulated = String::new();
        for c in chunks {
            if let Ok(Chunk::ToolCallChunk { arguments_delta, .. }) = c {
                accumulated.push_str(&arguments_delta);
            }
        }
        assert_eq!(serde_json::from_str::<Value>(&accumulated).unwrap(), json!({"text":"yo"}));
    }

    #[test]
    fn later_tool_call_deltas_get_the_id_of_their_index() {
        let raw = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_a\",\"function\":{\"name\":\"run_js\",\"arguments\":\"\"}}]}}]}\n\n\
                   data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"code\\\":\"}}]}}]}\n\n\
                   data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"call_b\",\"function\":{\"name\":\"list_dir\",\"arguments\":\"{}\"}}]}}]}\n\n\
                   data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"return 1\\\"}\"}}]}}]}\n\n\
                   data: [DONE]";
        let ids: Vec<String> = parse_sse(raw, Dialect::OpenAi).into_iter().filter_map(|c| match c {
            Ok(Chunk::ToolCallChunk { id, .. }) => Some(id),
            _ => None,
        }).collect();
        assert_eq!(ids, ["call_a", "call_a", "call_b", "call_a"]);
    }

    #[test]
    fn the_window_comes_out_of_the_props_of_a_llama_server() {
        assert_eq!(props_window(&json!({"default_generation_settings": {"n_ctx": 32768}})), 32768);
        assert_eq!(props_window(&json!({})), 0, "a server that does not say has no window");
    }

    #[test]
    fn a_local_servers_window_is_read_from_its_props_once() {
        // A one-shot HTTP server standing in for llama-server.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let served = std::thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0u8; 1024];
            let n = socket.read(&mut request).unwrap();
            let body = r#"{"default_generation_settings":{"n_ctx":16384}}"#;
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            String::from_utf8_lossy(&request[..n]).to_string()
        });
        let config = Mem::default();
        ConfigStore::set(&config, LLAMACPP_URL_KEY, &format!("http://127.0.0.1:{port}/v1")).unwrap();
        let gw = Gateway::new(Arc::new(Mem::default()), Arc::new(config));

        assert_eq!(gw.context_limit("llamacpp/anything"), 16384);
        // Asked again, it answers from memory: the server accepted one connection.
        assert_eq!(gw.context_limit("llamacpp/anything"), 16384);
        assert!(served.join().unwrap().starts_with("GET /props"), "the root, not /v1/props");
    }

    #[test]
    fn a_silent_response_yields_idle_until_the_first_chunk_arrives() {
        let (go, wait) = std::sync::mpsc::channel::<()>();
        let mut stream = Background::spawn(Duration::from_secs(60), move |send| {
            // The provider is still reading the prompt: nothing to send yet.
            wait.recv().ok();
            send(Ok(Chunk::TextDelta("hi".into())));
        }).unwrap();
        assert!(matches!(stream.next(), Some(Ok(Chunk::Idle))), "a silent wait must not block");
        assert!(matches!(stream.next(), Some(Ok(Chunk::Idle))));
        go.send(()).unwrap();
        loop {
            match stream.next() {
                Some(Ok(Chunk::Idle)) => continue,
                Some(Ok(Chunk::TextDelta(t))) => { assert_eq!(t, "hi"); break; }
                other => panic!("unexpected {other:?}"),
            }
        }
        assert!(stream.next().is_none(), "the stream ends when the reader does");
    }

    #[test]
    fn a_reply_that_stays_silent_past_the_limit_is_an_error_naming_the_setting() {
        let (_keep_open, wait) = std::sync::mpsc::channel::<()>();
        let mut stream = Background::spawn(Duration::from_millis(300), move |_send| {
            wait.recv().ok();
        }).unwrap();
        let outcome = loop {
            match stream.next() {
                Some(Ok(Chunk::Idle)) => continue,
                other => break other,
            }
        };
        match outcome {
            Some(Err(Error::Unavailable(why))) => assert!(why.contains(STREAM_IDLE_KEY), "{why}"),
            other => panic!("unexpected {other:?}"),
        }
        assert!(stream.next().is_none(), "it gives up once");
    }

    #[test]
    fn a_reply_that_keeps_arriving_is_never_cut_however_long_it_runs() {
        // Longer in all than the limit, but never silent for as long as it.
        let mut stream = Background::spawn(Duration::from_millis(450), |send| {
            for n in 0..6 {
                std::thread::sleep(Duration::from_millis(150));
                if !send(Ok(Chunk::TextDelta(format!("{n}")))) {
                    return;
                }
            }
        }).unwrap();
        let mut got = 0;
        loop {
            match stream.next() {
                Some(Ok(Chunk::Idle)) => continue,
                Some(Ok(Chunk::TextDelta(_))) => got += 1,
                Some(other) => panic!("unexpected {other:?}"),
                None => break,
            }
        }
        assert_eq!(got, 6);
    }

    #[test]
    fn the_silence_limit_is_a_setting_with_a_default() {
        let config = Mem::default();
        let gw = Gateway::new(Arc::new(Mem::default()), Arc::new(config));
        assert_eq!(gw.stream_idle_limit(), Duration::from_secs(STREAM_IDLE_SECS));
        let config = Mem::default();
        ConfigStore::set(&config, STREAM_IDLE_KEY, "1800").unwrap();
        let gw = Gateway::new(Arc::new(Mem::default()), Arc::new(config));
        assert_eq!(gw.stream_idle_limit(), Duration::from_secs(1800));
        let config = Mem::default();
        ConfigStore::set(&config, STREAM_IDLE_KEY, "soon").unwrap();
        let gw = Gateway::new(Arc::new(Mem::default()), Arc::new(config));
        assert_eq!(gw.stream_idle_limit(), Duration::from_secs(STREAM_IDLE_SECS), "a bad value falls back");
    }

    #[test]
    fn dropping_the_stream_stops_the_reader() {
        let (stopped, observed) = std::sync::mpsc::channel::<bool>();
        let (go, wait) = std::sync::mpsc::channel::<()>();
        let stream = Background::spawn(Duration::from_secs(60), move |send| {
            wait.recv().ok();
            // Once the consumer is gone, delivering fails and the reader quits.
            stopped.send(send(Ok(Chunk::TextDelta("late".into())))).ok();
        }).unwrap();
        drop(stream);
        go.send(()).unwrap();
        assert_eq!(observed.recv_timeout(Duration::from_secs(5)), Ok(false));
    }

    #[test]
    fn a_failed_request_reaches_the_consumer_as_the_first_item() {
        let mut stream = Background::spawn(Duration::from_secs(60), |send| {
            send(Err(Error::AuthRequired { provider: "p".into() }));
        }).unwrap();
        loop {
            match stream.next() {
                Some(Ok(Chunk::Idle)) => continue,
                Some(Err(Error::AuthRequired { .. })) => break,
                other => panic!("unexpected {other:?}"),
            }
        }
    }

    #[test]
    fn the_openai_body_carries_tools_calls_and_results() {
        let messages = vec![
            SessionMessage { id: "m1".into(), role: protocol::Role::User, content: "go".into(), created_at: 0, tool_calls: vec![] },
            SessionMessage {
                id: String::new(), role: protocol::Role::Assistant, content: String::new(), created_at: 0,
                tool_calls: vec![protocol::ToolCall { id: "call_a".into(), name: "list_dir".into(), arguments: "{}".into() }],
            },
            SessionMessage { id: "call_a".into(), role: protocol::Role::Tool, content: "a.txt".into(), created_at: 0, tool_calls: vec![] },
        ];
        let tools = [json!({"name": "list_dir", "description": "List.", "parameters": {"type": "object"}})];
        let body = openai_body("deepseek/deepseek-chat", &messages, &tools);
        assert_eq!(body["model"], "deepseek/deepseek-chat");
        assert_eq!(body["tools"], json!([{"type": "function", "function": tools[0]}]));
        assert_eq!(body["messages"][1]["tool_calls"], json!([{
            "id": "call_a", "type": "function", "function": {"name": "list_dir", "arguments": "{}"},
        }]));
        assert_eq!(body["messages"][2], json!({"role": "tool", "content": "a.txt", "tool_call_id": "call_a"}));
        assert!(body["messages"][0].get("tool_calls").is_none());
        assert!(openai_body("m", &messages, &[]).get("tools").is_none(), "no empty tools array");
    }

    /// A body that arrives in pieces, as a socket delivers it. A read that
    /// waits too long fails, so a reader that wants the whole body first fails
    /// the test instead of hanging it.
    struct Arriving {
        pieces: std::sync::mpsc::Receiver<Vec<u8>>,
        current: Vec<u8>,
        at: usize,
    }
    impl std::io::Read for Arriving {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            if self.at == self.current.len() {
                match self.pieces.recv_timeout(Duration::from_secs(2)) {
                    Ok(piece) => { self.current = piece; self.at = 0; }
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return Ok(0),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "no more data yet"));
                    }
                }
            }
            let n = out.len().min(self.current.len() - self.at);
            out[..n].copy_from_slice(&self.current[self.at..self.at + n]);
            self.at += n;
            Ok(n)
        }
    }

    fn arriving() -> (std::sync::mpsc::Sender<Vec<u8>>, SseStream<std::io::BufReader<Arriving>>) {
        let (tx, rx) = std::sync::mpsc::channel();
        let body = Arriving { pieces: rx, current: Vec::new(), at: 0 };
        (tx, SseStream::new(std::io::BufReader::new(body), Dialect::OpenAi))
    }

    fn token(text: &str) -> Vec<u8> {
        format!("data: {{\"choices\":[{{\"delta\":{{\"content\":\"{text}\"}}}}]}}\n\n").into_bytes()
    }

    #[test]
    fn each_token_reaches_the_caller_while_the_body_is_still_arriving() {
        let (tx, mut stream) = arriving();
        tx.send(token("Hel")).unwrap();
        // "lo" has not been sent yet: only a lazy reader can answer now.
        assert!(matches!(stream.next(), Some(Ok(Chunk::TextDelta(t))) if t == "Hel"));
        tx.send(token("lo")).unwrap();
        assert!(matches!(stream.next(), Some(Ok(Chunk::TextDelta(t))) if t == "lo"));
        tx.send(b"data: [DONE]\n\n".to_vec()).unwrap();
        assert!(matches!(stream.next(), Some(Ok(Chunk::Done))));
        assert!(stream.next().is_none(), "nothing after Done");
    }

    #[test]
    fn a_body_that_ends_without_done_still_ends_and_a_broken_one_says_so() {
        let (tx, mut stream) = arriving();
        tx.send(token("hi")).unwrap();
        drop(tx);
        let rest: Vec<_> = stream.by_ref().collect();
        assert!(matches!(rest.as_slice(), [Ok(Chunk::TextDelta(_)), Ok(Chunk::Done)]), "{rest:?}");

        let (tx, mut broken) = arriving();
        tx.send(token("half")).unwrap();
        assert!(matches!(broken.next(), Some(Ok(Chunk::TextDelta(_)))));
        // The sender stays alive but silent: the read fails.
        assert!(matches!(broken.next(), Some(Err(Error::Unavailable(m))) if m.starts_with("stream interrupted")));
        assert!(broken.next().is_none());
        drop(tx);
    }

    /// Feed content deltas through the parser; text and thinking, each joined.
    fn split_content(deltas: &[&str]) -> (String, String) {
        let mut parser = SseParser::new(Dialect::OpenAi);
        let mut chunks: Vec<Chunk> = deltas.iter().flat_map(|d| parser.content(d)).collect();
        chunks.extend(parser.finish().into_iter().flatten());
        let (mut text, mut thinking) = (String::new(), String::new());
        for chunk in chunks {
            match chunk {
                Chunk::TextDelta(t) => text.push_str(&t),
                Chunk::Thinking(t) => thinking.push_str(&t),
                _ => {}
            }
        }
        (text, thinking)
    }

    #[test]
    fn a_stray_closing_think_tag_never_reaches_the_answer() {
        let (text, _) = split_content(&["</think:6124c78e>Verifico che l'ambiente sia pronto:"]);
        assert_eq!(text, "Verifico che l'ambiente sia pronto:");
        let (text, _) = split_content(&["ok</think>", " fatto"]);
        assert_eq!(text, "ok fatto");
    }

    #[test]
    fn an_inline_think_block_becomes_thinking() {
        let (text, thinking) = split_content(&["<think>devo leggere i file</think>Leggo i file."]);
        assert_eq!((text.as_str(), thinking.as_str()), ("Leggo i file.", "devo leggere i file"));
    }

    #[test]
    fn a_tag_split_across_chunks_is_still_recognised() {
        let (text, thinking) = split_content(&["<thi", "nk>pen", "so</thi", "nk:ab12", ">ecco"]);
        assert_eq!((text.as_str(), thinking.as_str()), ("ecco", "penso"));
    }

    #[test]
    fn ordinary_angle_brackets_pass_through_untouched() {
        let (text, thinking) = split_content(&["if a < b && v.len() <", "4 { <thinking> }", " <t"]);
        assert_eq!(text, "if a < b && v.len() <4 { <thinking> } <t", "a held `<t` is released at the end");
        assert!(thinking.is_empty());
    }

    /// Secrets and config in memory.
    #[derive(Default)]
    struct Mem(Mutex<HashMap<String, String>>);
    impl SecretVault for Mem {
        fn get(&self, k: &str) -> Result<Option<String>> { Ok(self.0.lock().unwrap().get(k).cloned()) }
        fn set(&self, k: &str, v: &str) -> Result<()> { self.0.lock().unwrap().insert(k.into(), v.into()); Ok(()) }
    }
    impl ConfigStore for Mem {
        fn get(&self, k: &str) -> Result<Option<String>> { Ok(self.0.lock().unwrap().get(k).cloned()) }
        fn set(&self, k: &str, v: &str) -> Result<()> { self.0.lock().unwrap().insert(k.into(), v.into()); Ok(()) }
    }

    #[test]
    fn a_local_llama_cpp_server_needs_no_key_and_owns_its_ids() {
        let gw = Gateway::new(Arc::new(Mem::default()), Arc::new(Mem::default()));
        assert_eq!(gw.key("llamacpp").unwrap(), "", "keyless, so no Authorization header");
        assert!(matches!(gw.key("openrouter"), Err(Error::AuthRequired { .. })), "hosted ones still need a key");
        let local = gw.providers().unwrap().into_iter().find(|p| p.id == "llamacpp").unwrap();
        assert!(local.configured);
        assert_eq!(local.base_url, "http://127.0.0.1:8080/v1");
        assert_eq!(gw.resolve_provider("llamacpp/tiel-coder-35b-a3b").unwrap().name, "llamacpp");
    }

    #[test]
    fn the_local_server_address_comes_from_the_config() {
        let config = Mem::default();
        ConfigStore::set(&config, LLAMACPP_URL_KEY, "http://10.0.0.5:9000/v1").unwrap();
        let gw = Gateway::new(Arc::new(Mem::default()), Arc::new(config));
        assert_eq!(gw.endpoint("llamacpp").unwrap().base_url, "http://10.0.0.5:9000/v1");
    }

    #[test]
    fn https_transport_is_available() {
        // A real TLS handshake must be attempted. A 401/403 from the provider
        // is success for this assertion; "TLS required" means a broken build.
        let agent = ureq::Agent::new_with_config(
            ureq::Agent::config_builder().timeout_global(Some(CONNECT_TIMEOUT)).build(),
        );
        let result = agent.get("https://openrouter.ai/api/v1/models")
            .header("Authorization", "Bearer sk-invalid-probe")
            .call();
        match result {
            Ok(_) => {}
            Err(ureq::Error::StatusCode(code)) => assert!(
                code == 401 || code == 403, "unexpected status {code}"
            ),
            Err(e) => {
                let text = e.to_string();
                assert!(!text.contains("TLS required"), "TLS missing: {text}");
            }
        }
    }

    #[test]
    fn maps_rate_limit_and_auth_errors() {
        assert!(matches!(
            map_provider_error(&json!({"error":{"status":429,"message":"slow down"}})),
            Error::RateLimited { .. }
        ));
        assert!(matches!(
            map_provider_error(&json!({"error":{"status":401,"message":"bad key"}})),
            Error::AuthRequired { .. }
        ));
    }
}
