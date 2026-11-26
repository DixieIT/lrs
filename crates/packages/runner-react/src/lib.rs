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

/// What a step has put in the conversation, and what it still owes it.
struct InFlight {
    messages: Vec<SessionMessage>,
    /// Text the model has streamed that is not a message yet.
    text: String,
    /// Calls the model made that have no result yet.
    unanswered: Vec<ToolCall>,
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

    /// The controller's cancel flag becomes the turn's abort, so a cancel is
    /// honoured wherever the loop polls, not only at step boundaries.
    fn cancelled(tc: &TurnContext<'_>, abort: &Abort) -> bool {
        if tc.controller.is_cancelled() {
            abort.abort(CancelCause::User);
        }
        abort.signal()
    }

    /// Close the conversation when a step stops early so the next request is
    /// valid: every call the model made gets a result, and a reply cut short is
    /// kept, with a marker, instead of leaving user messages with no answer
    /// between them. A failure before any text leaves nothing to keep: the
    /// prompt can simply be sent again.
    fn close_conversation(&self, flight: &mut InFlight, sink: &dyn EventSink, cause: Interruption) {
        let nothing_said = flight.text.is_empty() && cause == Interruption::Failed;
        if flight.unanswered.is_empty() && !nothing_said {
            let marker = cause.reply_marker();
            let content = if flight.text.is_empty() {
                marker.to_string()
            } else {
                format!("{}\n\n{marker}", flight.text)
            };
            add(&mut flight.messages, sink, SessionMessage {
                id: String::new(), role: Role::Assistant, content, created_at: 0,
                tool_calls: Vec::new(),
            });
        }
        for call in std::mem::take(&mut flight.unanswered) {
            add(&mut flight.messages, sink, SessionMessage {
                id: call.id, role: Role::Tool, content: cause.tool_result().into(),
                created_at: 0, tool_calls: Vec::new(),
            });
        }
        self.state.lock().unwrap().turn_messages = std::mem::take(&mut flight.messages);
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
            // No abort check here: `step` polls before it calls the model and
            // closes the conversation, which an early return would skip.
            {
                let mut s = self.state.lock().unwrap();
                Self::sync_controller(&mut s, tc, sink);
            }
            let step = {
                let s = self.state.lock().unwrap();
                match s.phase { Phase::Running { step, .. } => step + 1, _ => unreachable!() }
            };
            let (claimed, added) = {
                let mut s = self.state.lock().unwrap();
                let claimed = s.inbox.claim(turn, 16);
                if step == 1 && !s.history_seeded {
                    s.turn_messages = tc.history.to_vec();
                    s.history_seeded = true;
                }
                // Every claimed message must reach the model: a steer synced
                // after a tool round is claimed on step 2+, where it would
                // otherwise be dropped with no later turn to carry it.
                let added: Vec<SessionMessage> = claimed.iter().map(|m| SessionMessage {
                    id: String::new(),
                    role: match m.source {
                        SourceKind::User => Role::User,
                        SourceKind::RuntimeContext => Role::System,
                    },
                    content: m.text.clone(),
                    created_at: 0,
                    tool_calls: Vec::new(),
                }).collect();
                s.turn_messages.extend(added.iter().cloned());
                (claimed, added)
            };
            // Announced outside the state lock: a sink may call back into the agent.
            for message in added {
                sink.emit(Event::MessageAdded { message });
            }
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
            // A steer queued during the model call or its tools is not in the
            // inbox yet; without this sync the turn would end and strand it.
            let pending = {
                let mut s = self.state.lock().unwrap();
                Self::sync_controller(&mut s, tc, sink);
                s.inbox.next_step_len()
            };
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

    /// One step: model calls and the tool rounds they ask for, until the model
    /// answers or a round of tools has run.
    ///
    /// Every way out goes through here, so the conversation is made valid in one
    /// place: whatever `run_step` left half done is closed by `close_conversation`
    /// before the error goes on. A new early return inside the step cannot forget it.
    fn step(
        &self,
        tc: &TurnContext<'_>,
        sink: &dyn EventSink,
        abort: &Abort,
        selection: &str,
    ) -> Result<StepEnd> {
        let mut flight = InFlight {
            messages: self.state.lock().unwrap().turn_messages.clone(),
            text: String::new(),
            unanswered: Vec::new(),
        };
        let end = self.run_step(tc, sink, abort, selection, &mut flight);
        match &end {
            Ok(_) => {
                self.state.lock().unwrap().turn_messages = std::mem::take(&mut flight.messages);
            }
            Err(error) => {
                let cause = match error {
                    Error::Cancelled => Interruption::Cancelled,
                    _ => Interruption::Failed,
                };
                self.close_conversation(&mut flight, sink, cause);
            }
        }
        end
    }

    fn run_step(
        &self,
        tc: &TurnContext<'_>,
        sink: &dyn EventSink,
        abort: &Abort,
        selection: &str,
        flight: &mut InFlight,
    ) -> Result<StepEnd> {
        // The executor decides what the model is offered: every tool (native)
        // or one code tool that calls them (PTC).
        let definitions: Vec<Value> = tc.executor.definitions(tc.tools);
        let mut step_guard = 0usize;
        loop {
            step_guard += 1;
            debug_assert!(step_guard < 1024, "step loop did not converge");
            if Self::cancelled(tc, abort) {
                return Err(Error::Cancelled);
            }
            let mut stream = tc.gateway.stream(selection, &flight.messages, &definitions)?;
            let mut calls: Vec<ToolCall> = Vec::new();
            flight.text.clear();
            let mut usage = None;

            // Cancelled before each pull, so no chunk is read after the user
            // asked to stop; returning drops the stream and its connection.
            loop {
                if Self::cancelled(tc, abort) {
                    return Err(Error::Cancelled);
                }
                let Some(chunk) = stream.next() else { break };
                match chunk? {
                    // Nothing arrived yet: the loop only needs to look again.
                    Chunk::Idle => {}
                    Chunk::TextDelta(t) => {
                        flight.text.push_str(&t);
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
                    input_tokens, output_tokens,
                    context_limit: tc.gateway.context_limit(selection),
                });
            }
            let text = std::mem::take(&mut flight.text);
            if calls.is_empty() {
                if !text.is_empty() {
                    add(&mut flight.messages, sink, SessionMessage {
                        id: String::new(), role: Role::Assistant, content: text, created_at: 0,
                        tool_calls: Vec::new(),
                    });
                }
                return Ok(StepEnd::Completed);
            }

            // The assistant's request goes into the conversation before the
            // results, so the model sees its own calls on the next request.
            add(&mut flight.messages, sink, SessionMessage {
                id: String::new(), role: Role::Assistant, content: text, created_at: 0,
                tool_calls: calls.iter().map(|c| protocol::ToolCall {
                    id: c.id.clone(), name: c.name.clone(), arguments: c.raw_args.clone(),
                }).collect(),
            });
            flight.unanswered = calls;

            while let Some(call) = flight.unanswered.first().cloned() {
                if Self::cancelled(tc, abort) {
                    return Err(Error::Cancelled);
                }
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
                    Ok(args) => match tc.executor.dispatch(
                        &call.name, args, tc.tools, &tc.tool_context, tc.permission,
                    ) {
                        Ok(out) => out,
                        // A stopped tool shows as stopped; its call, and every
                        // call after it, is answered by `step`.
                        Err(Error::Cancelled) => {
                            sink.emit(Event::ToolCallEnd {
                                id: call.id.clone(), output: TOOL_INTERRUPTED.into(), failed: true,
                            });
                            return Err(Error::Cancelled);
                        }
                        Err(e) => return Err(e),
                    },
                    Err(reason) => ToolOutput::err(reason),
                };
                // The frontend gets the whole output; the model gets it as the
                // presentation renders it (failure marked, size bounded).
                let for_model = tc.presentation.render_result(&out);
                sink.emit(Event::ToolCallEnd {
                    id: call.id.clone(), output: out.content, failed: out.is_error,
                });
                add(&mut flight.messages, sink, SessionMessage {
                    id: call.id.clone(), role: Role::Tool, content: for_model, created_at: 0,
                    tool_calls: Vec::new(),
                });
                flight.unanswered.remove(0);
            }

            for text in tc.controller.take_steers() {
                sink.emit(Event::Steered { text: text.clone() });
                add(&mut flight.messages, sink, SessionMessage {
                    id: String::new(), role: Role::User, content: text, created_at: 0,
                    tool_calls: Vec::new(),
                });
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

/// What the model reads where the user cut a reply or a tool short. The
/// conversation keeps strict alternation and no call is left unanswered.
const REPLY_INTERRUPTED: &str = "[Request interrupted by user]";
const TOOL_INTERRUPTED: &str = "[Tool execution interrupted by user]";
const REPLY_FAILED: &str = "[Reply interrupted by an error]";
const TOOL_FAILED: &str = "[Tool execution failed]";

/// Why a step stopped before it could finish what the model asked for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Interruption {
    /// The user stopped it.
    Cancelled,
    /// The model call or a tool failed; the turn ends with that error.
    Failed,
}

impl Interruption {
    fn reply_marker(self) -> &'static str {
        match self { Self::Cancelled => REPLY_INTERRUPTED, Self::Failed => REPLY_FAILED }
    }
    fn tool_result(self) -> &'static str {
        match self { Self::Cancelled => TOOL_INTERRUPTED, Self::Failed => TOOL_FAILED }
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
    impl CollectSink {
        /// What the conversation gained, as (role, id, content).
        fn messages(&self) -> Vec<(Role, String, String)> {
            self.events.lock().unwrap().iter().filter_map(|e| match e {
                Event::MessageAdded { message } =>
                    Some((message.role.clone(), message.id.clone(), message.content.clone())),
                _ => None,
            }).collect()
        }
    }

    fn context<'a>(gw: &'a dyn ModelGateway, ctrl: &'a TestController) -> TurnContext<'a> {
        context_with(gw, ctrl, &[&EchoTool])
    }

    fn context_with<'a>(
        gw: &'a dyn ModelGateway,
        ctrl: &'a TestController,
        tools: &'a [&'a dyn Tool],
    ) -> TurnContext<'a> {
        TurnContext {
            session_id: "s1", cwd: ".", history: &[], gateway: gw,
            tools, model: Some("test-model"), effort: None,
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
    fn a_steer_queued_during_the_final_model_call_is_not_stranded() {
        // The steer lands after the last step's own drain and before the turn
        // would end. It must be synced, claimed on a later step and shown to
        // the model, not left in the controller for the next run.
        struct LateSteerGateway {
            calls: AtomicUsize,
            steers: Arc<Mutex<Vec<String>>>,
            saw_steer: Arc<AtomicBool>,
        }
        impl ModelGateway for LateSteerGateway {
            fn id(&self) -> &str { "late-steer" }
            fn providers(&self) -> Result<Vec<ProviderInfo>> { Ok(vec![]) }
            fn models(&self) -> Result<Vec<ModelInfo>> { Ok(vec![]) }
            fn connect(&mut self, _: &str, _: &str) -> Result<()> { Ok(()) }
            fn default_model(&self) -> Option<String> { Some("test-model".into()) }
            fn stream(&self, _: &str, messages: &[SessionMessage], _: &[Value]) -> Result<Stream> {
                let n = self.calls.fetch_add(1, Ordering::SeqCst);
                if messages.iter().any(|m| m.content == "late note") {
                    self.saw_steer.store(true, Ordering::SeqCst);
                }
                if n == 0 {
                    self.steers.lock().unwrap().push("late note".into());
                }
                Ok(Box::new(vec![Ok(Chunk::TextDelta("done".into())), Ok(Chunk::Done)].into_iter()))
            }
        }

        let ctrl = TestController::default();
        let gw = LateSteerGateway {
            calls: AtomicUsize::new(0),
            steers: ctrl.steers.clone(),
            saw_steer: Arc::new(AtomicBool::new(false)),
        };
        let sink = CollectSink::default();
        let agent = ReactLoopAgent::new("s1", 0);
        agent.followup(UserMessage::user("go"));
        agent.run(&context(&gw, &ctrl), &sink).unwrap();

        assert_eq!(gw.calls.load(Ordering::SeqCst), 2, "the steer must cost one more step");
        assert!(gw.saw_steer.load(Ordering::SeqCst), "the model must receive the steer");
        assert!(ctrl.steers.lock().unwrap().is_empty(), "nothing is left for the next run");
        let announced: Vec<String> = sink.events.lock().unwrap().iter().filter_map(|e| match e {
            Event::Steered { text } => Some(text.clone()),
            _ => None,
        }).collect();
        assert_eq!(announced, vec!["late note"]);
    }

    #[test]
    fn a_cancel_during_the_stream_stops_reading_it() {
        struct CountingGateway { pulled: Arc<AtomicUsize> }
        impl ModelGateway for CountingGateway {
            fn id(&self) -> &str { "counting" }
            fn providers(&self) -> Result<Vec<ProviderInfo>> { Ok(vec![]) }
            fn models(&self) -> Result<Vec<ModelInfo>> { Ok(vec![]) }
            fn connect(&mut self, _: &str, _: &str) -> Result<()> { Ok(()) }
            fn default_model(&self) -> Option<String> { Some("test-model".into()) }
            fn stream(&self, _: &str, _: &[SessionMessage], _: &[Value]) -> Result<Stream> {
                let pulled = self.pulled.clone();
                Ok(Box::new(std::iter::from_fn(move || {
                    let n = pulled.fetch_add(1, Ordering::SeqCst) + 1;
                    (n <= 5).then(|| Ok(Chunk::TextDelta(format!("chunk{n}"))))
                })))
            }
        }
        // Flips the cancel flag once the second chunk has been shown.
        struct CancelOnSecond { cancelled: Arc<AtomicBool>, shown: AtomicUsize, inner: CollectSink }
        impl EventSink for CancelOnSecond {
            fn emit(&self, event: Event) {
                if matches!(event, Event::TextDelta { .. })
                    && self.shown.fetch_add(1, Ordering::SeqCst) + 1 == 2
                {
                    self.cancelled.store(true, Ordering::SeqCst);
                }
                self.inner.emit(event);
            }
        }
        impl CancelOnSecond {
            fn messages(&self) -> Vec<(Role, String, String)> { self.inner.messages() }
        }

        let ctrl = TestController::default();
        let pulled = Arc::new(AtomicUsize::new(0));
        let gw = CountingGateway { pulled: pulled.clone() };
        let sink = CancelOnSecond { cancelled: ctrl.cancelled.clone(), shown: AtomicUsize::new(0), inner: CollectSink::default() };
        let agent = ReactLoopAgent::new("s1", 0);
        agent.followup(UserMessage::user("go"));
        let outcome = agent.run(&context(&gw, &ctrl), &sink);

        assert!(matches!(outcome, Err(Error::Cancelled)), "got {outcome:?}");
        assert_eq!(pulled.load(Ordering::SeqCst), 2, "no chunk may be read after the cancel");
        assert_eq!(
            sink.messages(),
            vec![
                (Role::User, String::new(), "go".to_string()),
                (Role::Assistant, String::new(), format!("chunk1chunk2\n\n{REPLY_INTERRUPTED}")),
            ],
            "what was said is kept, marked as cut short",
        );
    }

    #[test]
    fn a_cancel_before_any_text_still_answers_the_user() {
        struct SilentGateway;
        impl ModelGateway for SilentGateway {
            fn id(&self) -> &str { "silent" }
            fn providers(&self) -> Result<Vec<ProviderInfo>> { Ok(vec![]) }
            fn models(&self) -> Result<Vec<ModelInfo>> { Ok(vec![]) }
            fn connect(&mut self, _: &str, _: &str) -> Result<()> { Ok(()) }
            fn default_model(&self) -> Option<String> { Some("test-model".into()) }
            fn stream(&self, _: &str, _: &[SessionMessage], _: &[Value]) -> Result<Stream> {
                panic!("a cancelled turn must not call the model");
            }
        }
        let ctrl = TestController::default();
        ctrl.cancelled.store(true, Ordering::SeqCst);
        let sink = CollectSink::default();
        let agent = ReactLoopAgent::new("s1", 0);
        agent.followup(UserMessage::user("go"));
        let outcome = agent.run(&context(&SilentGateway, &ctrl), &sink);

        assert!(matches!(outcome, Err(Error::Cancelled)), "got {outcome:?}");
        let roles: Vec<Role> = sink.messages().into_iter().map(|m| m.0).collect();
        assert_eq!(roles, vec![Role::User, Role::Assistant], "the prompt is never left unanswered");
    }

    /// A model that asks for the named tools in one reply, then answers.
    struct CallsGateway { names: Vec<&'static str>, calls: AtomicUsize }
    impl ModelGateway for CallsGateway {
        fn id(&self) -> &str { "calls" }
        fn providers(&self) -> Result<Vec<ProviderInfo>> { Ok(vec![]) }
        fn models(&self) -> Result<Vec<ModelInfo>> { Ok(vec![]) }
        fn connect(&mut self, _: &str, _: &str) -> Result<()> { Ok(()) }
        fn default_model(&self) -> Option<String> { Some("test-model".into()) }
        fn stream(&self, _: &str, _: &[SessionMessage], _: &[Value]) -> Result<Stream> {
            assert_eq!(self.calls.fetch_add(1, Ordering::SeqCst), 0, "the turn must end after the cancel");
            let chunks: Vec<Result<Chunk>> = self.names.iter().enumerate().map(|(i, name)| {
                Ok(Chunk::ToolCallChunk {
                    id: format!("c{}", i + 1), name: Some((*name).into()), arguments_delta: "{}".into(),
                })
            }).chain(std::iter::once(Ok(Chunk::Done))).collect();
            Ok(Box::new(chunks.into_iter()))
        }
    }

    /// Runs one tool that cancels the turn, and one that must then never run.
    struct CancelTool { name: &'static str, cancelled: Arc<AtomicBool>, fail: bool }
    impl Tool for CancelTool {
        fn name(&self) -> &str { self.name }
        fn description(&self) -> &str { "cancels the turn while it runs" }
        fn parameters_schema(&self) -> Value { json!({"type":"object"}) }
        fn execute(&self, _: Value, _: &ToolContext) -> Result<ToolOutput> {
            self.cancelled.store(true, Ordering::SeqCst);
            if self.fail { Err(Error::Cancelled) } else { Ok(ToolOutput::ok("ran")) }
        }
    }

    fn cancelled_tool_round(fail: bool) -> Vec<(Role, String, String)> {
        let ctrl = TestController::default();
        let first = CancelTool { name: "first", cancelled: ctrl.cancelled.clone(), fail };
        let second = EchoTool;
        let tools: [&dyn Tool; 2] = [&first, &second];
        let gw = CallsGateway { names: vec!["first", "echo", "echo"], calls: AtomicUsize::new(0) };
        let sink = CollectSink::default();
        let agent = ReactLoopAgent::new("s1", 0);
        agent.followup(UserMessage::user("go"));
        let outcome = agent.run(&context_with(&gw, &ctrl, &tools), &sink);
        assert!(matches!(outcome, Err(Error::Cancelled)), "got {outcome:?}");
        sink.messages()
    }

    /// A model that says `said`, then fails, or never says anything.
    struct FailingGateway { said: Vec<&'static str> }
    impl ModelGateway for FailingGateway {
        fn id(&self) -> &str { "failing" }
        fn providers(&self) -> Result<Vec<ProviderInfo>> { Ok(vec![]) }
        fn models(&self) -> Result<Vec<ModelInfo>> { Ok(vec![]) }
        fn connect(&mut self, _: &str, _: &str) -> Result<()> { Ok(()) }
        fn default_model(&self) -> Option<String> { Some("test-model".into()) }
        fn stream(&self, _: &str, _: &[SessionMessage], _: &[Value]) -> Result<Stream> {
            let chunks: Vec<Result<Chunk>> = self.said.iter()
                .map(|t| Ok(Chunk::TextDelta((*t).into())))
                .chain(std::iter::once(Err(Error::Unavailable("connection lost".into()))))
                .collect();
            Ok(Box::new(chunks.into_iter()))
        }
    }

    fn failed_stream(said: Vec<&'static str>) -> (Result<()>, Vec<(Role, String, String)>) {
        let ctrl = TestController::default();
        let sink = CollectSink::default();
        let agent = ReactLoopAgent::new("s1", 0);
        agent.followup(UserMessage::user("go"));
        let outcome = agent.run(&context(&FailingGateway { said }, &ctrl), &sink);
        (outcome, sink.messages())
    }

    #[test]
    fn a_stream_that_fails_after_some_text_keeps_the_text_with_a_marker() {
        let (outcome, messages) = failed_stream(vec!["Rome ", "was"]);
        assert!(matches!(outcome, Err(Error::Unavailable(_))), "got {outcome:?}");
        assert_eq!(messages, vec![
            (Role::User, String::new(), "go".to_string()),
            (Role::Assistant, String::new(), format!("Rome was\n\n{REPLY_FAILED}")),
        ]);
    }

    #[test]
    fn a_stream_that_fails_before_any_text_leaves_only_the_prompt() {
        let (outcome, messages) = failed_stream(vec![]);
        assert!(matches!(outcome, Err(Error::Unavailable(_))), "got {outcome:?}");
        assert_eq!(messages, vec![(Role::User, String::new(), "go".to_string())],
            "nothing to keep: the prompt can be sent again");
    }

    #[test]
    fn a_tool_that_fails_leaves_no_call_without_a_result() {
        struct BrokenTool;
        impl Tool for BrokenTool {
            fn name(&self) -> &str { "broken" }
            fn description(&self) -> &str { "always fails" }
            fn parameters_schema(&self) -> Value { json!({"type":"object"}) }
            fn execute(&self, _: Value, _: &ToolContext) -> Result<ToolOutput> {
                Err(Error::Internal("disk gone".into()))
            }
        }
        let ctrl = TestController::default();
        let tools: [&dyn Tool; 2] = [&BrokenTool, &EchoTool];
        let gw = CallsGateway { names: vec!["broken", "echo"], calls: AtomicUsize::new(0) };
        let sink = CollectSink::default();
        let agent = ReactLoopAgent::new("s1", 0);
        agent.followup(UserMessage::user("go"));
        let outcome = agent.run(&context_with(&gw, &ctrl, &tools), &sink);

        assert!(matches!(outcome, Err(Error::Internal(_))), "got {outcome:?}");
        let results: Vec<(String, String)> = sink.messages().into_iter()
            .filter(|m| m.0 == Role::Tool).map(|m| (m.1, m.2)).collect();
        assert_eq!(results, vec![
            ("c1".to_string(), TOOL_FAILED.to_string()),
            ("c2".to_string(), TOOL_FAILED.to_string()),
        ]);
    }

    #[test]
    fn a_cancel_is_honoured_while_the_model_has_sent_nothing() {
        // The model is still reading the prompt: the stream only says "idle".
        struct IdleGateway { pulled: Arc<AtomicUsize>, cancelled: Arc<AtomicBool> }
        impl ModelGateway for IdleGateway {
            fn id(&self) -> &str { "idle" }
            fn providers(&self) -> Result<Vec<ProviderInfo>> { Ok(vec![]) }
            fn models(&self) -> Result<Vec<ModelInfo>> { Ok(vec![]) }
            fn connect(&mut self, _: &str, _: &str) -> Result<()> { Ok(()) }
            fn default_model(&self) -> Option<String> { Some("test-model".into()) }
            fn stream(&self, _: &str, _: &[SessionMessage], _: &[Value]) -> Result<Stream> {
                let (pulled, cancelled) = (self.pulled.clone(), self.cancelled.clone());
                Ok(Box::new(std::iter::from_fn(move || {
                    // The user presses escape during the third wait.
                    if pulled.fetch_add(1, Ordering::SeqCst) + 1 == 3 {
                        cancelled.store(true, Ordering::SeqCst);
                    }
                    Some(Ok(Chunk::Idle))
                })))
            }
        }
        let ctrl = TestController::default();
        let pulled = Arc::new(AtomicUsize::new(0));
        let gw = IdleGateway { pulled: pulled.clone(), cancelled: ctrl.cancelled.clone() };
        let sink = CollectSink::default();
        let agent = ReactLoopAgent::new("s1", 0);
        agent.followup(UserMessage::user("go"));
        let outcome = agent.run(&context(&gw, &ctrl), &sink);

        assert!(matches!(outcome, Err(Error::Cancelled)), "got {outcome:?}");
        assert_eq!(pulled.load(Ordering::SeqCst), 3, "stops at the first look after the cancel");
        assert_eq!(sink.messages(), vec![
            (Role::User, String::new(), "go".to_string()),
            (Role::Assistant, String::new(), REPLY_INTERRUPTED.to_string()),
        ]);
    }

    #[test]
    fn usage_carries_the_window_the_gateway_reports() {
        struct WindowGateway;
        impl ModelGateway for WindowGateway {
            fn id(&self) -> &str { "window" }
            fn providers(&self) -> Result<Vec<ProviderInfo>> { Ok(vec![]) }
            fn models(&self) -> Result<Vec<ModelInfo>> { Ok(vec![]) }
            fn connect(&mut self, _: &str, _: &str) -> Result<()> { Ok(()) }
            fn default_model(&self) -> Option<String> { Some("test-model".into()) }
            fn context_limit(&self, model: &str) -> u64 { if model == "test-model" { 8_192 } else { 0 } }
            fn stream(&self, _: &str, _: &[SessionMessage], _: &[Value]) -> Result<Stream> {
                let chunks: Vec<Result<Chunk>> = vec![
                    Ok(Chunk::TextDelta("hi".into())),
                    Ok(Chunk::Usage { input_tokens: 100, output_tokens: 5 }),
                    Ok(Chunk::Done),
                ];
                Ok(Box::new(chunks.into_iter()))
            }
        }
        let ctrl = TestController::default();
        let sink = CollectSink::default();
        let agent = ReactLoopAgent::new("s1", 0);
        agent.followup(UserMessage::user("go"));
        agent.run(&context(&WindowGateway, &ctrl), &sink).unwrap();

        let usage: Vec<(u64, u64, u64)> = sink.events.lock().unwrap().iter().filter_map(|e| match e {
            Event::Usage { input_tokens, output_tokens, context_limit } =>
                Some((*input_tokens, *output_tokens, *context_limit)),
            _ => None,
        }).collect();
        assert_eq!(usage, vec![(100, 5, 8_192)]);
    }

    #[test]
    fn a_cancel_between_tool_calls_answers_the_ones_that_did_not_run() {
        let messages = cancelled_tool_round(false);
        let results: Vec<(String, String)> = messages.into_iter()
            .filter(|m| m.0 == Role::Tool).map(|m| (m.1, m.2)).collect();
        assert_eq!(results, vec![
            ("c1".to_string(), "ran".to_string()),
            ("c2".to_string(), TOOL_INTERRUPTED.to_string()),
            ("c3".to_string(), TOOL_INTERRUPTED.to_string()),
        ]);
    }

    #[test]
    fn a_tool_stopped_by_the_cancel_and_the_calls_after_it_are_answered() {
        let messages = cancelled_tool_round(true);
        let results: Vec<(String, String)> = messages.into_iter()
            .filter(|m| m.0 == Role::Tool).map(|m| (m.1, m.2)).collect();
        assert_eq!(results, vec![
            ("c1".to_string(), TOOL_INTERRUPTED.to_string()),
            ("c2".to_string(), TOOL_INTERRUPTED.to_string()),
            ("c3".to_string(), TOOL_INTERRUPTED.to_string()),
        ], "every call the model made has exactly one result");
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
