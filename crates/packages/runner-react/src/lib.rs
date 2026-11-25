//! Pure implementation of `sdk::AgentRunner`.
//!
//! Every capability comes from `TurnContext`: the model from `gateway`, the
//! executors from `tools`, cancellation and steering from `controller`.
//! The runner never constructs a filesystem, a permission policy, a provider
//! payload or a concrete component.

use protocol::{Event, Role, SessionMessage};
use sdk::{
    AgentRunner, Chunk, Config, Contribution, Ctx, Disposer, Error, EventSink,
    FunctionPlugin, PluginForm, PluginManifest, RegistrationDecl, ResolverManifest,
    Result, ToolOutput, TurnContext,
};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tracing::debug;

// ---------------------------------------------------------------------------
// Pure state machine (domain facts, no I/O)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CancelCause {
    User,
    Parent,
    Disposed,
    Hook { reason: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelOptions {
    Default,
    KeepInbox,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboxTarget {
    NextTurn,
    NextStep,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UserMessage {
    pub text: String,
    pub source: SourceKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    User,
    RuntimeContext,
}

impl UserMessage {
    pub fn user(text: impl Into<String>) -> Self {
        Self { text: text.into(), source: SourceKind::User }
    }
    pub fn context(text: impl Into<String>) -> Self {
        Self { text: text.into(), source: SourceKind::RuntimeContext }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentStatus {
    Idle,
    Running,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TurnEndReason {
    Completed,
    Blocked,
    MaxTokens,
    Aborted { cause: CancelCause },
    Error { message: String, code: String },
}

#[derive(Debug, Clone, Default)]
struct InboxState {
    next_turn: VecDeque<UserMessage>,
    next_step: VecDeque<UserMessage>,
}

impl InboxState {
    fn clear(&mut self) {
        self.next_turn.clear();
        self.next_step.clear();
    }
    fn has_pending(&self) -> bool {
        !self.next_turn.is_empty() || !self.next_step.is_empty()
    }
    fn next_step_len(&self) -> usize {
        self.next_step.len()
    }
    fn splice(&mut self, target: InboxTarget, message: UserMessage) {
        match target {
            InboxTarget::NextTurn => self.next_turn.push_back(message),
            InboxTarget::NextStep => self.next_step.push_back(message),
        }
    }
    fn claim(&mut self, turn: u64, limit: usize) -> Vec<UserMessage> {
        let mut out = Vec::new();
        if turn > 0 {
            out.extend(self.next_turn.drain(..).take(limit));
        }
        let remaining = limit.saturating_sub(out.len());
        out.extend(self.next_step.drain(..).take(remaining));
        out
    }
}

#[derive(Clone, Default)]
pub struct Abort {
    cancelled: Arc<AtomicBool>,
    cause: Arc<Mutex<Option<CancelCause>>>,
}

impl Abort {
    pub fn signal(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
    fn abort(&self, cause: CancelCause) {
        if !self.cancelled.swap(true, Ordering::SeqCst) {
            *self.cause.lock().unwrap() = Some(cause);
        }
    }
    fn cause(&self) -> Option<CancelCause> {
        self.signal().then(|| self.cause.lock().unwrap().clone()).flatten()
    }
    fn throw_if_aborted(&self) -> Result<()> {
        if self.signal() { Err(Error::Cancelled) } else { Ok(()) }
    }
}

enum Phase {
    Idle { last_turn: u64 },
    Maintenance { abort: Abort, last_turn: u64, wake_requested: bool },
    Running { abort: Abort, turn: u64, step: u64, wake_requested: bool },
}

impl Phase {
    fn status(&self) -> AgentStatus {
        match self {
            Phase::Idle { .. } | Phase::Maintenance { .. } => AgentStatus::Idle,
            Phase::Running { .. } => AgentStatus::Running,
        }
    }
    fn any_abort(&self) -> Option<&Abort> {
        match self {
            Phase::Running { abort, .. } | Phase::Maintenance { abort, .. } => Some(abort),
            Phase::Idle { .. } => None,
        }
    }
}

struct AgentState {
    phase: Phase,
    inbox: InboxState,
    /// Turn-scoped conversation context, persisted across steps.
    turn_messages: Vec<SessionMessage>,
    /// The incoming `TurnContext::history` seeds only the first turn. Without
    /// this guard a completed answer would be re-sent on every driver turn.
    history_seeded: bool,
    status_listeners: Vec<Arc<dyn Fn(AgentStatus) + Send + Sync>>,
}

#[derive(Debug, Clone, PartialEq)]
struct ToolCall {
    id: String,
    name: String,
    /// Raw JSON fragment accumulated across streamed chunks. Providers split
    /// arguments mid-token, so parsing must wait until the stream is finished.
    raw_args: String,
}

enum StepEnd {
    Completed,
    ToolRound,
}

// ---------------------------------------------------------------------------
// Runner
// ---------------------------------------------------------------------------

/// The agent itself implements `sdk::AgentRunner`; no extra adapter is needed.
pub struct ReactLoopAgent {
    id: String,
    state: Mutex<AgentState>,
}

impl ReactLoopAgent {
    pub fn new(id: impl Into<String>, last_turn: u64) -> Self {
        Self {
            id: id.into(),
            state: Mutex::new(AgentState {
                phase: Phase::Idle { last_turn },
                inbox: InboxState::default(),
                turn_messages: Vec::new(),
                history_seeded: false,
                status_listeners: Vec::new(),
            }),
        }
    }

    pub fn status(&self) -> AgentStatus {
        self.state.lock().unwrap().phase.status()
    }

    pub fn on_status(&mut self, f: impl Fn(AgentStatus) + Send + Sync + 'static) {
        self.state.lock().unwrap().status_listeners.push(Arc::new(f));
    }

    fn set_phase(state: &mut AgentState, next: Phase) {
        let before = state.phase.status();
        state.phase = next;
        let after = state.phase.status();
        if before != after {
            for l in &state.status_listeners {
                l(after);
            }
        }
    }

    pub fn send(&self, message: UserMessage, target: InboxTarget, wakeup: bool) {
        let mut s = self.state.lock().unwrap();
        let waking_after_abort = wakeup
            && !matches!(s.phase, Phase::Idle { .. })
            && s.phase.any_abort().is_some_and(Abort::signal);
        let resolved = if waking_after_abort { InboxTarget::NextTurn } else { target };
        s.inbox.splice(resolved, message);
        if wakeup {
            Self::wake_driver(&mut s, waking_after_abort);
        }
    }

    pub fn followup(&self, input: UserMessage) {
        self.send(input, InboxTarget::NextTurn, true);
    }
    pub fn steer(&self, input: UserMessage) {
        self.send(input, InboxTarget::NextStep, true);
    }
    pub fn inject(&self, input: UserMessage) {
        self.send(input, InboxTarget::NextStep, false);
    }

    pub fn cancel(&self, cause: CancelCause, options: CancelOptions) {
        let mut s = self.state.lock().unwrap();
        if options == CancelOptions::Default {
            s.inbox.clear();
        }
        match &mut s.phase {
            Phase::Idle { .. } => {}
            Phase::Maintenance { abort, wake_requested, .. }
            | Phase::Running { abort, wake_requested, .. } => {
                if options == CancelOptions::Default {
                    *wake_requested = false;
                }
                abort.abort(cause);
            }
        }
    }

    pub fn run_maintenance<T>(&self, job: impl FnOnce(&Abort) -> T) -> T {
        let mut s = self.state.lock().unwrap();
        let Phase::Idle { last_turn } = s.phase else {
            panic!("agent already has active work");
        };
        let abort = Abort::default();
        Self::set_phase(&mut s, Phase::Maintenance {
            abort: abort.clone(), last_turn, wake_requested: false,
        });
        let out = job(&abort);
        let (wake, turn) = match s.phase {
            Phase::Maintenance { wake_requested, last_turn, .. } => (wake_requested, last_turn),
            _ => (false, last_turn),
        };
        Self::set_phase(&mut s, Phase::Idle { last_turn: turn });
        if wake && s.inbox.has_pending() {
            Self::wake_driver(&mut s, false);
        }
        out
    }

    fn wake_driver(s: &mut AgentState, wake_after_abort: bool) {
        match &mut s.phase {
            Phase::Idle { .. } => {}
            Phase::Maintenance { abort, wake_requested, .. }
            | Phase::Running { abort, wake_requested, .. } => {
                let disposed = matches!(abort.cause(), Some(CancelCause::Disposed));
                if !disposed && wake_after_abort {
                    *wake_requested = true;
                }
                return;
            }
        }
        let Phase::Idle { last_turn } = s.phase else { unreachable!() };
        Self::set_phase(s, Phase::Running {
            abort: Abort::default(), turn: last_turn, step: 0, wake_requested: false,
        });
    }

    fn sync_controller(s: &mut AgentState, tc: &TurnContext<'_>, sink: &dyn EventSink) {
        if tc.controller.is_cancelled() {
            if let Some(abort) = s.phase.any_abort() {
                abort.abort(CancelCause::User);
            }
        }
        for text in tc.controller.take_steers() {
            sink.emit(Event::Steered { text: text.clone() });
            s.inbox.splice(InboxTarget::NextStep, UserMessage::user(text));
        }
    }

    fn model_for(&self, tc: &TurnContext<'_>) -> Result<String> {
        tc.model
            .map(str::to_string)
            .or_else(|| tc.gateway.default_model())
            .ok_or_else(|| Error::Unavailable(format!("agent \"{}\" has no model", self.id)))
    }

    fn turn(
        &self,
        tc: &TurnContext<'_>,
        sink: &dyn EventSink,
        abort: &Abort,
        turn: u64,
    ) -> Result<bool> {
        let mut target = InboxTarget::NextTurn;
        let mut ending: Option<TurnEndReason> = None;

        loop {
            abort.throw_if_aborted()?;
            {
                let mut s = self.state.lock().unwrap();
                Self::sync_controller(&mut s, tc, sink);
            }
            let step = {
                let s = self.state.lock().unwrap();
                match s.phase { Phase::Running { step, .. } => step + 1, _ => unreachable!() }
            };
            let claimed = {
                let mut s = self.state.lock().unwrap();
                let claimed = s.inbox.claim(turn, 16);
                if step == 1 && !s.history_seeded {
                    s.turn_messages = tc.history.to_vec();
                    s.history_seeded = true;
                    for m in &claimed {
                        add(&mut s.turn_messages, sink, SessionMessage {
                            id: String::new(),
                            role: match m.source {
                                SourceKind::User => Role::User,
                                SourceKind::RuntimeContext => Role::System,
                            },
                            content: m.text.clone(),
                            created_at: 0,
                            tool_calls: Vec::new(),
                        });
                    }
                }
                claimed
            };
            let selection = self.model_for(tc)?;

            if claimed.is_empty() && tc.history.is_empty() && step == 1 {
                ending = Some(TurnEndReason::Completed);
                break;
            }
            if ending.is_some() && claimed.is_empty() {
                break;
            }

            debug!(agent = %self.id, turn, step, "step start");
            {
                let mut s = self.state.lock().unwrap();
                if let Phase::Running { step: x, .. } = &mut s.phase { *x = step; }
            }

            match self.step(tc, sink, abort, &selection) {
                Ok(StepEnd::Completed) => ending = Some(TurnEndReason::Completed),
                Ok(StepEnd::ToolRound) => {}
                Err(e) => {
                    let reason = match abort.cause() {
                        Some(cause) => TurnEndReason::Aborted { cause },
                        None => TurnEndReason::Error {
                            message: e.to_string(), code: error_code(&e).into(),
                        },
                    };
                    debug!(agent = %self.id, turn, ?reason, "turn failed");
                    return Err(e);
                }
            }

            abort.throw_if_aborted()?;
            let pending = self.state.lock().unwrap().inbox.next_step_len();
            if ending.is_some() && pending == 0 {
                break;
            }
            debug!(agent = %self.id, turn, ?target, "next step target");
            target = InboxTarget::NextStep;
        }

        debug!(agent = %self.id, turn, ?ending, "turn end");
        let more = self.state.lock().unwrap().inbox.has_pending();
        if more {
            let mut s = self.state.lock().unwrap();
            if let Phase::Running { step, wake_requested, .. } = &mut s.phase {
                *wake_requested = false;
                *step = 0;
            }
        }
        Ok(more)
    }

    fn step(
        &self,
        tc: &TurnContext<'_>,
        sink: &dyn EventSink,
        abort: &Abort,
        selection: &str,
    ) -> Result<StepEnd> {
        // The executor decides what the model is offered: every tool (native)
        // or one code tool that calls them (PTC).
        let definitions: Vec<Value> = tc.executor.definitions(tc.tools);
        let mut messages = {
            let s = self.state.lock().unwrap();
            s.turn_messages.clone()
        };
        let mut step_guard = 0usize;
        loop {
            step_guard += 1;
            debug_assert!(step_guard < 1024, "step loop did not converge");
            abort.throw_if_aborted()?;
            let stream = tc.gateway.stream(selection, &messages, &definitions)?;
            let mut calls: Vec<ToolCall> = Vec::new();
            let mut text = String::new();
            let mut usage = None;

            for chunk in stream {
                abort.throw_if_aborted()?;
                match chunk? {
                    Chunk::TextDelta(t) => {
                        text.push_str(&t);
                        sink.emit(Event::TextDelta { text: t });
                    }
                    Chunk::Thinking(t) => sink.emit(Event::Thinking { text: t }),
                    Chunk::ToolCallChunk { id, name, arguments_delta } => {
                        match calls.iter_mut().find(|c| c.id == id) {
                            Some(call) => {
                                if let Some(n) = name { call.name = n; }
                                call.raw_args.push_str(&arguments_delta);
                            }
                            None => calls.push(ToolCall {
                                id,
                                name: name.unwrap_or_default(),
                                raw_args: arguments_delta,
                            }),
                        }
                    }
                    Chunk::Usage { input_tokens, output_tokens } => {
                        usage = Some((input_tokens, output_tokens));
                    }
                    Chunk::Done => break,
                }
            }

            if let Some((input_tokens, output_tokens)) = usage {
                sink.emit(Event::Usage {
                    input_tokens, output_tokens, context_limit: 0,
                });
            }
            if calls.is_empty() {
                if !text.is_empty() {
                    add(&mut messages, sink, SessionMessage {
                        id: String::new(), role: Role::Assistant, content: text, created_at: 0,
                        tool_calls: Vec::new(),
                    });
                }
                self.state.lock().unwrap().turn_messages = messages;
                return Ok(StepEnd::Completed);
            }

            // The assistant's request goes into the conversation before the
            // results, so the model sees its own calls on the next request.
            add(&mut messages, sink, SessionMessage {
                id: String::new(), role: Role::Assistant, content: text, created_at: 0,
                tool_calls: calls.iter().map(|c| protocol::ToolCall {
                    id: c.id.clone(), name: c.name.clone(), arguments: c.raw_args.clone(),
                }).collect(),
            });

            for call in calls {
                abort.throw_if_aborted()?;
                let parsed = parse_arguments(&call.raw_args);
                sink.emit(Event::ToolCallStart {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    args: parsed.clone().unwrap_or(Value::Null),
                });
                // Arguments that are not JSON are the model's mistake: it is
                // told so, and nothing runs. Permission is enforced inside the
                // executor, on the deciding operation.
                let out: ToolOutput = match parsed {
                    Ok(args) => tc.executor.dispatch(
                        &call.name, args, tc.tools, &tc.tool_context, tc.permission,
                    )?,
                    Err(reason) => ToolOutput::err(reason),
                };
                // The frontend gets the whole output; the model gets it as the
                // presentation renders it (failure marked, size bounded).
                let for_model = tc.presentation.render_result(&out);
                sink.emit(Event::ToolCallEnd {
                    id: call.id.clone(), output: out.content, failed: out.is_error,
                });
                add(&mut messages, sink, SessionMessage {
                    id: call.id, role: Role::Tool, content: for_model, created_at: 0,
                    tool_calls: Vec::new(),
                });
            }

            for text in tc.controller.take_steers() {
                sink.emit(Event::Steered { text: text.clone() });
                add(&mut messages, sink, SessionMessage {
                    id: String::new(), role: Role::User, content: text, created_at: 0,
                    tool_calls: Vec::new(),
                });
            }
            {
                let mut s = self.state.lock().unwrap();
                s.turn_messages = messages;
            }
            debug!(agent = %self.id, "tool round completed, continuing step");
            return Ok(StepEnd::ToolRound);
        }
    }
}

impl ReactLoopAgent {
    pub fn id(&self) -> &str { &self.id }
}

impl AgentRunner for ReactLoopAgent {
    fn id(&self) -> &str { &self.id }
    fn run(&self, tc: &TurnContext<'_>, sink: &dyn EventSink) -> Result<()> {
        let turn = {
            let mut s = self.state.lock().unwrap();
            // Every job hands over the whole session history, so each run
            // reseeds from it; the guard only spans the turns of one run.
            s.history_seeded = false;
            Self::sync_controller(&mut s, tc, sink);
            if !matches!(s.phase, Phase::Running { .. }) {
                Self::wake_driver(&mut s, false);
            }
            match &s.phase {
                Phase::Running { abort, turn, .. } => (abort.clone(), *turn + 1),
                _ => return Err(Error::Internal("agent failed to enter running phase".into())),
            }
        };
        let (abort, turn) = turn;

        let mut guard = 0usize;
        let outcome = loop {
            guard += 1;
            debug_assert!(guard < 1024, "run loop did not converge");
            match self.turn(tc, sink, &abort, turn) {
                Ok(true) => continue,
                Ok(false) => break Ok(()),
                Err(e) => break Err(e),
            }
        };

        // Return to Idle on every path, so a failed turn never leaves the
        // agent stuck in Running for the next job.
        let mut s = self.state.lock().unwrap();
        if let Phase::Running { turn, wake_requested, .. } = s.phase {
            Self::set_phase(&mut s, Phase::Idle { last_turn: turn });
            if wake_requested && s.inbox.has_pending() {
                Self::wake_driver(&mut s, false);
            }
        }
        outcome
    }
}

/// Add a message to the conversation and announce it, so the runtime keeps
/// it for the next turn. The only way a message enters the conversation.
fn add(messages: &mut Vec<SessionMessage>, sink: &dyn EventSink, message: SessionMessage) {
    sink.emit(Event::MessageAdded { message: message.clone() });
    messages.push(message);
}

/// The model's argument text as JSON. Empty text means no arguments.
fn parse_arguments(raw: &str) -> std::result::Result<Value, String> {
    if raw.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(raw).map_err(|e| format!("the arguments are not valid JSON ({e}); send one JSON object"))
}

fn error_code(error: &Error) -> &'static str {
    match error {
        Error::AuthRequired { .. } => "AUTH_REQUIRED",
        Error::Cancelled => "CANCELLED",
        Error::Timeout { .. } => "TIMEOUT",
        Error::PermissionDenied(_) => "PERMISSION_DENIED",
        Error::RateLimited { .. } => "RATE_LIMITED",
        Error::InvalidInput(_) => "INVALID_INPUT",
        Error::Unavailable(_) => "UNAVAILABLE",
        Error::Internal(_) => "INTERNAL",
    }
}

impl PluginManifest for ReactLoopAgent {
    fn manifest() -> ResolverManifest {
        ResolverManifest {
            id: sdk::REACT_RUNNER.into(),
            name: "runner-react".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            form: PluginForm::Function,
            registrations: vec![RegistrationDecl::Runner { name: sdk::REACT_RUNNER.into() }],
            config_schema: None,
        }
    }
}

/// Registration adapter: contributes the runner service as an effect.
pub struct ReactLoopPlugin {
    agent: ReactLoopAgent,
}

impl ReactLoopPlugin {
    pub fn new(agent: ReactLoopAgent) -> Self { Self { agent } }
}

impl FunctionPlugin for ReactLoopPlugin {
    fn name(&self) -> &str { "runner-react" }
    fn registrations(&self) -> Vec<RegistrationDecl> { ReactLoopAgent::manifest().registrations }
    fn apply(&self, _c: &Config, ctx: &mut dyn Ctx) -> Result<Disposer> {
        let h = ctx.effect(Contribution::Runner {
            runner: Box::new(ReactLoopAgent::new(self.agent.id().to_string(), 0)),
        })?;
        let mut d = Disposer::default();
        d.push(h);
        Ok(d)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sdk::{FsPort, ModelGateway, ModelInfo, ProviderInfo, Stream, Tool, ToolContext};
    use std::sync::atomic::AtomicUsize;

    #[derive(Default)]
    struct NoFs;
    impl FsPort for NoFs {}

    /// Test executor: runs the tool directly, mirroring the native package.
    struct TestExecutor;
    impl sdk::ToolExecutor for TestExecutor {
        fn name(&self) -> &str { "test" }
        fn execute(&self, t: &dyn sdk::Tool, a: Value, c: &ToolContext,
                   p: &sdk::PermissionCheck) -> Result<ToolOutput> {
            p(t.name(), &a)?;
            t.execute(a, c)
        }
        fn definitions(&self, tools: &[&dyn Tool]) -> Vec<Value> {
            tools.iter().map(|t| json!({"name": t.name()})).collect()
        }
        fn dispatch(&self, call: &str, a: Value, tools: &[&dyn Tool], c: &ToolContext,
                    p: &sdk::PermissionCheck) -> Result<ToolOutput> {
            match tools.iter().find(|t| t.name() == call) {
                Some(t) => self.execute(*t, a, c, p),
                None => Ok(ToolOutput::err(format!("unknown tool {call}"))),
            }
        }
    }

    fn allow_all(_: &str, _: &Value) -> Result<()> { Ok(()) }

    /// Recognisable rendering: marks failures and keeps 20 characters.
    struct Capped;
    impl sdk::ToolPresentation for Capped {
        fn render_call(&self, name: &str, _: &Value) -> String { name.into() }
        fn render_result(&self, output: &ToolOutput) -> String {
            let text: String = output.content.chars().take(20).collect();
            if output.is_error { format!("Error: {text}") } else { text }
        }
        fn render_diagnostic(&self, error: &Error) -> String { error.to_string() }
    }

    #[derive(Clone, Default)]
    struct TestController {
        cancelled: Arc<AtomicBool>,
        steers: Arc<Mutex<Vec<String>>>,
    }
    impl TestController {
        fn steer(&self, text: &str) { self.steers.lock().unwrap().push(text.into()); }
    }
    impl sdk::TurnControl for TestController {
        fn is_cancelled(&self) -> bool { self.cancelled.load(Ordering::SeqCst) }
        fn take_steers(&self) -> Vec<String> { std::mem::take(&mut *self.steers.lock().unwrap()) }
    }

    struct EchoTool;
    impl Tool for EchoTool {
        fn name(&self) -> &str { "echo" }
        fn description(&self) -> &str { "echoes its input" }
        fn parameters_schema(&self) -> Value { json!({"type":"object"}) }
        fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<ToolOutput> {
            Ok(ToolOutput::ok(args["text"].as_str().unwrap_or("")))
        }
    }

    struct ScriptedGateway {
        calls: AtomicUsize,
        saw_tool_result: Arc<AtomicBool>,
    }
    impl ModelGateway for ScriptedGateway {
        fn id(&self) -> &str { "scripted" }
        fn providers(&self) -> Result<Vec<ProviderInfo>> { Ok(vec![]) }
        fn models(&self) -> Result<Vec<ModelInfo>> { Ok(vec![]) }
        fn connect(&mut self, _p: &str, _k: &str) -> Result<()> { Ok(()) }
        fn default_model(&self) -> Option<String> { Some("test-model".into()) }
        fn stream(&self, _model: &str, messages: &[SessionMessage], tools: &[Value]) -> Result<Stream> {
            assert!(!tools.is_empty(), "active tool schemas must reach the gateway");
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if messages.iter().any(|m| m.role == Role::Tool) {
                self.saw_tool_result.store(true, Ordering::SeqCst);
            }
            let chunks: Vec<Result<Chunk>> = if n == 0 {
                vec![
                    Ok(Chunk::ToolCallChunk {
                        id: "c1".into(),
                        name: Some("echo".into()),
                        arguments_delta: "{\"te".into(),
                    }),
                    Ok(Chunk::ToolCallChunk {
                        id: "c1".into(),
                        name: None,
                        arguments_delta: "xt\":\"hello\"}".into(),
                    }),
                    Ok(Chunk::Done),
                ]
            } else {
                vec![Ok(Chunk::TextDelta("done".into())), Ok(Chunk::Done)]
            };
            Ok(Box::new(chunks.into_iter()))
        }
    }

    #[derive(Default)]
    struct CollectSink { events: Mutex<Vec<Event>> }
    impl EventSink for CollectSink {
        fn emit(&self, event: Event) { self.events.lock().unwrap().push(event); }
    }

    fn context<'a>(gw: &'a ScriptedGateway, ctrl: &'a TestController) -> TurnContext<'a> {
        TurnContext {
            session_id: "s1", cwd: ".", history: &[], gateway: gw,
            tools: &[&EchoTool], model: Some("test-model"), effort: None,
            controller: Arc::new(ctrl.clone()),
            executor: &TestExecutor, permission: &allow_all, presentation: &Capped,
            tool_context: ToolContext {
                session_id: "s1".into(), cwd: ".".into(),
                cancellation_token: Arc::new(|| false), fs: Arc::new(NoFs),
            },
        }
    }

    #[test]
    fn tool_call_then_final_answer() {
        let saw = Arc::new(AtomicBool::new(false));
        let gw = ScriptedGateway { calls: AtomicUsize::new(0), saw_tool_result: saw.clone() };
        let ctrl = TestController::default();
        let tc = context(&gw, &ctrl);
        let sink = CollectSink::default();
        let agent = ReactLoopAgent::new("s1", 0);
        agent.followup(UserMessage::user("go"));
        agent.run(&tc, &sink).unwrap();

        assert!(saw.load(Ordering::SeqCst), "second call must see the tool result");
        let events = sink.events.lock().unwrap();
        assert!(events.iter().any(|e| matches!(e, Event::ToolCallStart { name, .. } if name == "echo")));
        assert!(events.iter().any(|e| matches!(e, Event::ToolCallEnd { output, .. } if output == "hello")));
        assert!(events.iter().any(|e| matches!(e, Event::TextDelta { text } if text == "done")));
        assert_eq!(gw.calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn steer_mid_turn_is_injected_into_the_next_model_call() {
        let saw = Arc::new(AtomicBool::new(false));
        let gw = ScriptedGateway { calls: AtomicUsize::new(0), saw_tool_result: saw.clone() };
        let ctrl = TestController::default();
        ctrl.steer("mid-turn note");
        let tc = context(&gw, &ctrl);
        let sink = CollectSink::default();
        let agent = ReactLoopAgent::new("s1", 0);
        agent.followup(UserMessage::user("go"));
        agent.run(&tc, &sink).unwrap();
        assert!(saw.load(Ordering::SeqCst));
        assert_eq!(gw.calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn cancel_default_clears_inbox_while_keep_inbox_preserves_it() {
        let a = ReactLoopAgent::new("s1", 0);
        a.followup(UserMessage::user("first"));
        a.cancel(CancelCause::User, CancelOptions::Default);
        assert!(!a.state.lock().unwrap().inbox.has_pending());

        let b = ReactLoopAgent::new("s2", 0);
        b.followup(UserMessage::user("first"));
        b.cancel(CancelCause::User, CancelOptions::KeepInbox);
        assert!(b.state.lock().unwrap().inbox.has_pending());
    }

    #[test]
    fn history_driven_turn_calls_the_model_without_inbox_input() {
        // Mirrors JobManager::start_run: the user message lives in history,
        // the runner's inbox is empty.
        let history = vec![SessionMessage {
            id: "m1".into(), role: Role::User, content: "from history".into(), created_at: 0, tool_calls: Vec::new(),
        }];
        let saw = Arc::new(AtomicBool::new(false));
        let gw = ScriptedGateway { calls: AtomicUsize::new(0), saw_tool_result: saw.clone() };
        let ctrl = TestController::default();
        let tool = EchoTool;
        let tc = TurnContext {
            session_id: "s1", cwd: ".", history: &history, gateway: &gw,
            tools: &[&tool], model: Some("test-model"), effort: None,
            controller: Arc::new(ctrl.clone()), executor: &TestExecutor, permission: &allow_all, presentation: &Capped,
            tool_context: ToolContext {
                session_id: "s1".into(), cwd: ".".into(),
                cancellation_token: Arc::new(|| false), fs: Arc::new(NoFs),
            },
        };
        let sink = CollectSink::default();
        ReactLoopAgent::new("s1", 0).run(&tc, &sink).unwrap();
        assert!(gw.calls.load(Ordering::SeqCst) >= 1, "model must be called from history");
    }

    #[test]
    fn plugin_registers_a_runner_not_a_generic_service() {
        #[derive(Default)]
        struct CaptureCtx {
            seen: Mutex<Vec<String>>,
        }
        impl Ctx for CaptureCtx {
            fn effect(&mut self, contribution: Contribution) -> Result<sdk::RegistrationHandle> {
                let kind = match contribution {
                    Contribution::Runner { .. } => "runner",
                    Contribution::Service { .. } => "service",
                    _ => "other",
                };
                self.seen.lock().unwrap().push(kind.into());
                Ok(sdk::RegistrationHandle::new(|| {}))
            }
            fn on(&mut self, _e: &'static str, _h: Box<dyn Fn(&dyn std::any::Any) + Send + Sync>) -> Result<sdk::RegistrationHandle> {
                Ok(sdk::RegistrationHandle::new(|| {}))
            }
            fn get(&self, _n: &str) -> Option<std::sync::Arc<dyn std::any::Any + Send + Sync>> { None }
            fn declared(&self, _n: &str) -> std::sync::Arc<dyn std::any::Any + Send + Sync> { unreachable!() }
        }

        let plugin = ReactLoopPlugin::new(ReactLoopAgent::new("s1", 0));
        assert!(matches!(
            plugin.registrations().first(),
            Some(RegistrationDecl::Runner { name }) if name == "react"
        ));
        let mut ctx = CaptureCtx::default();
        plugin.apply(&Config::default(), &mut ctx).unwrap();
        assert_eq!(ctx.seen.into_inner().unwrap(), vec!["runner".to_string()]);
    }


    #[test]
    fn history_seeded_run_calls_model_once() {
        let history = vec![SessionMessage {
            id: "m1".into(), role: Role::User, content: "ciao".into(), created_at: 0, tool_calls: Vec::new(),
        }];
        // A gateway that always emits plain text:
        // a tool-less answer must end the turn, not loop on the same history.
        struct PlainGateway(AtomicUsize);
        impl sdk::ModelGateway for PlainGateway {
            fn id(&self) -> &str { "plain" }
            fn providers(&self) -> Result<Vec<sdk::ProviderInfo>> { Ok(vec![]) }
            fn models(&self) -> Result<Vec<sdk::ModelInfo>> { Ok(vec![]) }
            fn connect(&mut self, _: &str, _: &str) -> Result<()> { Ok(()) }
            fn default_model(&self) -> Option<String> { Some("m".into()) }
            fn stream(&self, _: &str, _: &[SessionMessage], _: &[Value]) -> Result<sdk::Stream> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(Box::new(vec![Ok(Chunk::TextDelta("ciao!".into())), Ok(Chunk::Done)].into_iter()))
            }
        }
        let plain = PlainGateway(AtomicUsize::new(0));
        let ctrl = TestController::default();
        let tool = EchoTool;
        let tc = TurnContext {
            session_id: "s1", cwd: ".", history: &history, gateway: &plain,
            tools: &[&tool], model: Some("m"), effort: None,
            controller: Arc::new(ctrl.clone()), executor: &TestExecutor, permission: &allow_all, presentation: &Capped,
            tool_context: ToolContext {
                session_id: "s1".into(), cwd: ".".into(),
                cancellation_token: Arc::new(|| false), fs: Arc::new(NoFs),
            },
        };
        let sink = CollectSink::default();
        ReactLoopAgent::new("s1", 0).run(&tc, &sink).unwrap();
        assert_eq!(plain.0.load(Ordering::SeqCst), 1, "one answer must not re-enter the turn");
    }

    #[test]
    fn a_second_run_sends_the_new_prompt_to_the_model() {
        struct RecordingGateway(Mutex<Vec<String>>);
        impl sdk::ModelGateway for RecordingGateway {
            fn id(&self) -> &str { "rec" }
            fn providers(&self) -> Result<Vec<sdk::ProviderInfo>> { Ok(vec![]) }
            fn models(&self) -> Result<Vec<sdk::ModelInfo>> { Ok(vec![]) }
            fn connect(&mut self, _: &str, _: &str) -> Result<()> { Ok(()) }
            fn default_model(&self) -> Option<String> { Some("m".into()) }
            fn stream(&self, _: &str, msgs: &[SessionMessage], _: &[Value]) -> Result<sdk::Stream> {
                let last = msgs.last().map(|m| m.content.clone()).unwrap_or_default();
                self.0.lock().unwrap().push(last);
                Ok(Box::new(vec![Ok(Chunk::TextDelta("ok".into())), Ok(Chunk::Done)].into_iter()))
            }
        }
        let gw = RecordingGateway(Mutex::new(vec![]));
        let agent = ReactLoopAgent::new("s1", 0);
        let user = |text: &str| SessionMessage {
            id: String::new(), role: Role::User, content: text.into(), created_at: 0, tool_calls: Vec::new(),
        };
        let reply = SessionMessage { id: String::new(), role: Role::Assistant, content: "ok".into(), created_at: 0, tool_calls: Vec::new() };
        for history in [vec![user("first")], vec![user("first"), reply, user("second")]] {
            let ctrl = TestController::default();
            let tc = TurnContext {
                session_id: "s1", cwd: ".", history: &history, gateway: &gw,
                tools: &[], model: Some("m"), effort: None,
                controller: Arc::new(ctrl), executor: &TestExecutor, permission: &allow_all, presentation: &Capped,
                tool_context: ToolContext {
                    session_id: "s1".into(), cwd: ".".into(),
                    cancellation_token: Arc::new(|| false), fs: Arc::new(NoFs),
                },
            };
            agent.run(&tc, &CollectSink::default()).unwrap();
        }
        assert_eq!(*gw.0.lock().unwrap(), vec!["first".to_string(), "second".to_string()]);
    }

    #[test]
    fn a_gateway_error_fails_the_run_instead_of_finishing_it() {
        struct FailingGateway;
        impl sdk::ModelGateway for FailingGateway {
            fn id(&self) -> &str { "fail" }
            fn providers(&self) -> Result<Vec<sdk::ProviderInfo>> { Ok(vec![]) }
            fn models(&self) -> Result<Vec<sdk::ModelInfo>> { Ok(vec![]) }
            fn connect(&mut self, _: &str, _: &str) -> Result<()> { Ok(()) }
            fn default_model(&self) -> Option<String> { Some("m".into()) }
            fn stream(&self, _: &str, _: &[SessionMessage], _: &[Value]) -> Result<sdk::Stream> {
                Err(Error::AuthRequired { provider: "p".into() })
            }
        }
        let history = vec![SessionMessage {
            id: String::new(), role: Role::User, content: "hi".into(), created_at: 0, tool_calls: Vec::new(),
        }];
        let tc = TurnContext {
            session_id: "s1", cwd: ".", history: &history, gateway: &FailingGateway,
            tools: &[], model: Some("m"), effort: None,
            controller: Arc::new(TestController::default()), executor: &TestExecutor, permission: &allow_all, presentation: &Capped,
            tool_context: ToolContext {
                session_id: "s1".into(), cwd: ".".into(),
                cancellation_token: Arc::new(|| false), fs: Arc::new(NoFs),
            },
        };
        let agent = ReactLoopAgent::new("s1", 0);
        assert!(matches!(agent.run(&tc, &CollectSink::default()), Err(Error::AuthRequired { .. })));
        assert_eq!(agent.status(), AgentStatus::Idle, "a failed run must not leave the agent running");
    }


    #[test]
    fn the_model_sees_its_own_call_before_the_result_and_bad_arguments_are_reported() {
        struct Recording(Mutex<Vec<Vec<SessionMessage>>>);
        impl sdk::ModelGateway for Recording {
            fn id(&self) -> &str { "rec" }
            fn providers(&self) -> Result<Vec<sdk::ProviderInfo>> { Ok(vec![]) }
            fn models(&self) -> Result<Vec<sdk::ModelInfo>> { Ok(vec![]) }
            fn connect(&mut self, _: &str, _: &str) -> Result<()> { Ok(()) }
            fn default_model(&self) -> Option<String> { Some("m".into()) }
            fn stream(&self, _: &str, msgs: &[SessionMessage], _: &[Value]) -> Result<sdk::Stream> {
                let mut calls = self.0.lock().unwrap();
                calls.push(msgs.to_vec());
                let chunks = if calls.len() == 1 {
                    vec![
                        Ok(Chunk::TextDelta("checking".into())),
                        Ok(Chunk::ToolCallChunk { id: "c1".into(), name: Some("echo".into()), arguments_delta: "{\"text\":\"hi\"}".into() }),
                        Ok(Chunk::ToolCallChunk { id: "c2".into(), name: Some("echo".into()), arguments_delta: "{not json".into() }),
                        Ok(Chunk::Done),
                    ]
                } else {
                    vec![Ok(Chunk::TextDelta("done".into())), Ok(Chunk::Done)]
                };
                Ok(Box::new(chunks.into_iter()))
            }
        }
        let gw = Recording(Mutex::new(vec![]));
        let history = vec![SessionMessage {
            id: String::new(), role: Role::User, content: "go".into(), created_at: 0, tool_calls: Vec::new(),
        }];
        let tc = TurnContext {
            session_id: "s1", cwd: ".", history: &history, gateway: &gw,
            tools: &[&EchoTool], model: Some("m"), effort: None,
            controller: Arc::new(TestController::default()), executor: &TestExecutor, permission: &allow_all, presentation: &Capped,
            tool_context: ToolContext {
                session_id: "s1".into(), cwd: ".".into(),
                cancellation_token: Arc::new(|| false), fs: Arc::new(NoFs),
            },
        };
        let sink = CollectSink::default();
        ReactLoopAgent::new("s1", 0).run(&tc, &sink).unwrap();
        let calls = gw.0.lock().unwrap();
        let second = &calls[1];
        let roles: Vec<Role> = second.iter().map(|m| m.role).collect();
        assert_eq!(roles, [Role::User, Role::Assistant, Role::Tool, Role::Tool]);
        assert_eq!(second[1].content, "checking");
        assert_eq!(second[1].tool_calls.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), ["c1", "c2"]);
        assert_eq!((second[2].id.as_str(), second[2].content.as_str()), ("c1", "hi"));
        assert_eq!(second[3].id, "c2");
        // The model gets the presentation's rendering: failure marked, size capped…
        assert_eq!(second[3].content, "Error: the arguments are no");
        // …while the frontend's event carries the whole output.
        let events = sink.events.lock().unwrap();
        assert!(events.iter().any(|e| matches!(
            e, Event::ToolCallEnd { id, output, failed: true }
                if id == "c2" && output.starts_with("the arguments are not valid JSON (")
        )), "{events:?}");
    }
}
