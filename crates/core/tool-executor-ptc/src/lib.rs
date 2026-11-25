//! PTC (programmatic tool calling) executor.
//!
//! The model is offered one tool, `run_js`, whose argument is a JavaScript
//! program. Inside the program every tool of the turn is a synchronous
//! function `tools.<name>(args)`. The program runs in a fresh QuickJS runtime
//! with a heap limit, a stack limit and an interrupt handler that stops it on
//! cancellation or when its wall-clock budget runs out.
//!
//! Isolation: the runtime has only the ECMAScript built-ins and `tools`. There
//! is no network, filesystem, process or console global. Each tool call leaves
//! the script and is decided on the caller's thread by `execute`, through the
//! permission closure: the deciding operation is the only path to a tool.
//!
//! Threading: QuickJS values cannot outlive the closure that created them, so
//! host functions cannot borrow the turn's tools. The script therefore runs on
//! its own thread holding only owned data, and every tool call is a message to
//! the calling thread, which still holds the borrowed tools and permission.

mod heap;

use heap::MeteredHeap;
use rquickjs::{Context, Function, Runtime};
use sdk::{
    Config, Contribution, Ctx, Disposer, Error, FunctionPlugin, PermissionCheck, PluginForm,
    PluginManifest, RegistrationDecl, ResolverManifest, Result, Tool, ToolContext, ToolExecutor,
    ToolOutput,
};
use serde_json::{json, Value};
use std::cell::Cell;
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The single tool the model is offered under this executor.
pub const RUN_JS: &str = "run_js";

/// How long the caller waits past the deadline for a script that has stopped
/// answering, before reporting the timeout without it.
const UNRESPONSIVE_GRACE: Duration = Duration::from_secs(2);

/// Native stack reserved for the script thread on top of the QuickJS stack
/// limit, for the Rust frames around the interpreter.
const THREAD_STACK_HEADROOM: usize = 4 * 1024 * 1024;

/// Resource limits applied to every program.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtcLimits {
    /// Maximum bytes the JavaScript heap may allocate.
    pub memory_bytes: usize,
    /// Maximum native stack the interpreter may use.
    pub stack_bytes: usize,
    /// Wall-clock budget for one program, tool calls included.
    pub timeout: Duration,
}

impl Default for PtcLimits {
    fn default() -> Self {
        Self {
            memory_bytes: 64 * 1024 * 1024,
            stack_bytes: 1024 * 1024,
            timeout: Duration::from_secs(30),
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ToolExecutorPtc {
    limits: PtcLimits,
}

impl ToolExecutorPtc {
    pub fn new(limits: PtcLimits) -> Self {
        Self { limits }
    }

    /// Answer one call a script made. `Err` is fatal for the whole program:
    /// the script is stopped and its result discarded (fail closed).
    fn host_call(
        &self,
        name: &str,
        raw_args: &str,
        tools: &[&dyn Tool],
        ctx: &ToolContext,
        permission: &PermissionCheck,
        deadline: Instant,
    ) -> Result<Reply> {
        let Some(tool) = tools.iter().find(|t| t.name() == name) else {
            return Ok(Reply::ToolError(format!("unknown tool {name}")));
        };
        let Ok(args) = serde_json::from_str::<Value>(raw_args) else {
            return Ok(Reply::ToolError(format!("arguments for {name} are not valid JSON")));
        };
        if Instant::now() >= deadline {
            return Err(self.timeout());
        }
        let out = self.execute(*tool, args, ctx, permission)?;
        Ok(if out.is_error { Reply::ToolError(out.content) } else { Reply::Output(out.content) })
    }

    fn timeout(&self) -> Error {
        Error::Timeout { limit_ms: self.limits.timeout.as_millis().try_into().unwrap_or(u64::MAX) }
    }

    fn run(
        &self,
        code: &str,
        tools: &[&dyn Tool],
        ctx: &ToolContext,
        permission: &PermissionCheck,
    ) -> Result<ToolOutput> {
        let deadline = Instant::now() + self.limits.timeout;
        let (to_host, from_script) = mpsc::channel::<Request>();
        let (to_script, from_host) = mpsc::channel::<Reply>();
        let job = Script {
            code: code.to_string(),
            tool_names: tools.iter().map(|t| t.name().to_string()).collect(),
            limits: self.limits,
            deadline,
            cancelled: ctx.cancellation_token.clone(),
        };
        // Detached on purpose: a script that stops answering must not hold the
        // turn. The interrupt handler still ends it at the deadline.
        std::thread::Builder::new()
            .name("ptc-script".into())
            .stack_size(self.limits.stack_bytes + THREAD_STACK_HEADROOM)
            .spawn(move || job.run(to_host, from_host))
            .map_err(|e| Error::Internal(format!("cannot start script thread: {e}")))?;

        // The first fatal error wins; once set, every further call is refused.
        let mut fatal: Option<Error> = None;
        loop {
            let wait = deadline.saturating_duration_since(Instant::now()) + UNRESPONSIVE_GRACE;
            match from_script.recv_timeout(wait) {
                Ok(Request::Call { name, args }) => {
                    let reply = match &fatal {
                        Some(_) => Reply::Abort,
                        None => self.host_call(&name, &args, tools, ctx, permission, deadline)
                            .unwrap_or_else(|e| {
                                fatal = Some(e);
                                Reply::Abort
                            }),
                    };
                    if to_script.send(reply).is_err() {
                        return Err(Error::Internal("script stopped during a tool call".into()));
                    }
                }
                Ok(Request::Done(outcome)) => return self.finish(outcome, fatal, ctx, deadline),
                Err(RecvTimeoutError::Timeout) => return Err(fatal.unwrap_or_else(|| self.timeout())),
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(fatal.unwrap_or_else(|| Error::Internal("script thread stopped unexpectedly".into())));
                }
            }
        }
    }

    fn finish(
        &self,
        outcome: Outcome,
        fatal: Option<Error>,
        ctx: &ToolContext,
        deadline: Instant,
    ) -> Result<ToolOutput> {
        if let Some(e) = fatal {
            return Err(e);
        }
        match outcome {
            // A string goes back as the text it is: JSON would wrap it in quotes
            // and escape every newline, harder to read and longer to send.
            Outcome::Value(json) => Ok(ToolOutput::ok(match serde_json::from_str::<Value>(&json) {
                Ok(Value::String(text)) => text,
                _ => json,
            })),
            Outcome::Failed(message) => Ok(ToolOutput::err(message)),
            Outcome::Interrupted if ctx.is_cancelled() => Err(Error::Cancelled),
            Outcome::Interrupted if Instant::now() >= deadline => Err(self.timeout()),
            Outcome::Interrupted => Err(Error::Internal("script interrupted without a cause".into())),
        }
    }
}

impl ToolExecutor for ToolExecutorPtc {
    fn name(&self) -> &str { sdk::PTC_EXECUTOR }

    fn execute(
        &self,
        tool: &dyn Tool,
        args: Value,
        ctx: &ToolContext,
        permission: &PermissionCheck,
    ) -> Result<ToolOutput> {
        permission(tool.name(), &args)?;
        // After the decision, before the tool: a malformed call is reported to
        // the model with the field and the type it expected, and never runs.
        if let Err(problems) = tool_schema::validate_args(&tool.parameters_schema(), &args) {
            return Ok(ToolOutput::err(format!("invalid arguments for {}: {problems}", tool.name())));
        }
        if ctx.is_cancelled() {
            return Err(Error::Cancelled);
        }
        tool.execute(args, ctx)
    }

    fn definitions(&self, tools: &[&dyn Tool]) -> Vec<Value> {
        vec![json!({
            "name": RUN_JS,
            "description": describe(tools),
            "parameters": {
                "type": "object",
                "properties": {
                    "code": {
                        "type": "string",
                        "description": "Body of a JavaScript function. Call tools as tools.<name>({...}) and end with `return <value>`.",
                    },
                },
                "required": ["code"],
            },
        })]
    }

    fn dispatch(
        &self,
        call: &str,
        args: Value,
        tools: &[&dyn Tool],
        ctx: &ToolContext,
        permission: &PermissionCheck,
    ) -> Result<ToolOutput> {
        if call != RUN_JS {
            return Ok(ToolOutput::err(format!("unknown tool {call}; call {RUN_JS} with a program")));
        }
        match args.get("code").and_then(Value::as_str) {
            Some(code) => self.run(code, tools, ctx, permission),
            None => Ok(ToolOutput::err(format!("{RUN_JS} needs a string argument `code`"))),
        }
    }
}

/// Model-facing description of `run_js`: what a program can do, then the
/// tools it can call as TypeScript declarations generated from their schemas.
fn describe(tools: &[&dyn Tool]) -> String {
    let schemas: Vec<Value> = tools.iter().map(|t| t.parameters_schema()).collect();
    let signatures: Vec<tool_schema::ToolSignature<'_>> = tools.iter().zip(&schemas)
        .map(|(t, parameters)| tool_schema::ToolSignature {
            name: t.name(),
            description: t.description(),
            parameters,
        })
        .collect();
    let mut text = String::from(
        "Run a JavaScript program and return its result. Write plain JavaScript: the program \
         runs as written, so TypeScript syntax such as type annotations is a syntax error; the \
         declarations below only describe the tools. The program is the body of a \
         function: it runs top to bottom and must end with `return <value>`, where the \
         value can be converted to JSON; a string comes back as plain text, any other value \
         as JSON. Each tool is a synchronous function \
         `tools.<name>(args)` that takes one object of arguments and returns the tool's \
         text output; a failed tool call throws an Error. Only the standard JavaScript \
         built-ins and `tools` exist: no network, files or console besides the tools. \
         Use one program to chain several tool calls and compute over their results.",
    );
    if tools.is_empty() {
        text.push_str("\n\nNo tools are available; the program can only compute.");
        return text;
    }
    text.push_str("\n\nAvailable tools:\n");
    text.push_str(&tool_schema::render_ts_declarations(&signatures));
    text
}

// ---------------------------------------------------------------------------
// The script thread
// ---------------------------------------------------------------------------

/// A message from the script to the caller.
enum Request {
    Call { name: String, args: String },
    Done(Outcome),
}

/// The caller's answer to one tool call.
enum Reply {
    Output(String),
    /// The tool ran and failed, or was misnamed: the script sees an Error.
    ToolError(String),
    /// A fatal error on the caller's side: the script must stop.
    Abort,
}

/// How a program ended, as seen from inside the interpreter.
enum Outcome {
    /// The JSON text of the returned value.
    Value(String),
    /// A JavaScript error: message and stack, or the reason the value was refused.
    Failed(String),
    /// Stopped by the interrupt handler.
    Interrupted,
}

/// Everything the script thread owns. No borrowed data crosses the thread.
struct Script {
    code: String,
    tool_names: Vec<String>,
    limits: PtcLimits,
    deadline: Instant,
    cancelled: Arc<dyn Fn() -> bool + Send + Sync>,
}

impl Script {
    fn run(self, to_host: Sender<Request>, from_host: Receiver<Reply>) {
        let outcome = self.evaluate(to_host.clone(), from_host);
        match to_host.send(Request::Done(outcome)) {
            Ok(()) => {}
            // The caller stopped waiting and already reported a timeout:
            // there is nobody left to tell.
            Err(mpsc::SendError(_)) => {}
        }
    }

    fn evaluate(&self, to_host: Sender<Request>, from_host: Receiver<Reply>) -> Outcome {
        // The metered heap is the only memory authority: it enforces the limit
        // and remembers refusing, so exhaustion is never reported as `null`.
        let (heap, exhausted) = MeteredHeap::new(self.limits.memory_bytes);
        let runtime = match Runtime::new_with_alloc(heap) {
            Ok(rt) => rt,
            Err(e) => return Outcome::Failed(format!("cannot start the interpreter: {e}")),
        };
        runtime.set_max_stack_size(self.limits.stack_bytes);
        let out_of_memory = || Outcome::Failed(format!(
            "out of memory: the program used more than {} MiB",
            self.limits.memory_bytes / (1024 * 1024),
        ));

        // Set by the host function when the caller aborts the program, so the
        // interrupt handler stops it even if the script catches the Error.
        let aborted = Rc::new(Cell::new(false));
        let interrupted = Rc::new(Cell::new(false));
        {
            let (aborted, interrupted) = (aborted.clone(), interrupted.clone());
            let (cancelled, deadline) = (self.cancelled.clone(), self.deadline);
            runtime.set_interrupt_handler(Some(Box::new(move || {
                let stop = aborted.get() || cancelled() || Instant::now() >= deadline;
                if stop {
                    interrupted.set(true);
                }
                stop
            })));
        }

        let context = match Context::full(&runtime) {
            Ok(c) => c,
            Err(e) => return Outcome::Failed(format!("cannot start the interpreter: {e}")),
        };
        let outcome = context.with(|ctx| {
            let host = Function::new(ctx.clone(), move |name: String, args: String| -> String {
                host_reply(&to_host, &from_host, &aborted, name, args)
            });
            let installed = host.and_then(|f| ctx.globals().set("__host_call", f))
                .and_then(|()| ctx.eval::<(), _>(bootstrap(&self.tool_names)));
            if let Err(e) = installed {
                return Outcome::Failed(format!("cannot prepare the program: {e}"));
            }
            match ctx.eval::<String, _>(wrap(&self.code)) {
                Ok(json) => Outcome::Value(json),
                Err(_) if exhausted.get() => out_of_memory(),
                Err(rquickjs::Error::Exception) => Outcome::Failed(describe_exception(ctx.catch())),
                Err(e) => Outcome::Failed(e.to_string()),
            }
        });
        if interrupted.get() { Outcome::Interrupted } else { outcome }
    }
}

/// The host function behind every `tools.<name>(...)`. Returns a JSON envelope
/// the bootstrap unpacks: `{"output"}`, `{"error"}` (thrown as an Error) or
/// `{"abort"}` (thrown, and the interrupt handler ends the program).
fn host_reply(
    to_host: &Sender<Request>,
    from_host: &Receiver<Reply>,
    aborted: &Cell<bool>,
    name: String,
    args: String,
) -> String {
    let reply = if aborted.get() || to_host.send(Request::Call { name, args }).is_err() {
        Reply::Abort
    } else {
        from_host.recv().unwrap_or(Reply::Abort)
    };
    match reply {
        Reply::Output(text) => json!({ "output": text }).to_string(),
        Reply::ToolError(text) => json!({ "error": text }).to_string(),
        Reply::Abort => {
            aborted.set(true);
            json!({ "abort": "the program was stopped" }).to_string()
        }
    }
}

/// Installs the frozen `tools` object and hides the raw host function.
fn bootstrap(tool_names: &[String]) -> String {
    let names = serde_json::to_string(tool_names).unwrap_or_else(|_| "[]".into());
    format!(
        r#"(function () {{
  "use strict";
  const call = globalThis.__host_call;
  delete globalThis.__host_call;
  const tools = Object.create(null);
  for (const name of {names}) {{
    tools[name] = function (args) {{
      const text = JSON.stringify(args === undefined ? {{}} : args);
      const reply = JSON.parse(call(name, text));
      if ("output" in reply) return reply.output;
      throw new Error("error" in reply ? reply.error : reply.abort);
    }};
  }}
  Object.defineProperty(globalThis, "tools", {{ value: Object.freeze(tools) }});
}})();"#
    )
}

/// Runs the model's function body and converts its value to JSON inside the
/// interpreter, so a refused value is an Error with a reason, never `null`.
fn wrap(code: &str) -> String {
    format!(
        r#"(function () {{
  const value = (function () {{
{code}
  }})();
  if (value === undefined) {{
    throw new TypeError("the program returned no value; end it with `return <value>`");
  }}
  const json = JSON.stringify(value);
  if (json === undefined) {{
    throw new TypeError("the returned value cannot be converted to JSON (" + typeof value + ")");
  }}
  return json;
}})()"#
    )
}

fn describe_exception(value: rquickjs::Value<'_>) -> String {
    if let Some(exception) = value.clone().into_exception() {
        let message = exception.message().unwrap_or_default();
        let name = exception.get::<_, String>("name").unwrap_or_else(|_| "Error".into());
        return match exception.stack().filter(|s| !s.trim().is_empty()) {
            Some(stack) => format!("{name}: {message}\n{}", stack.trim_end()),
            None => format!("{name}: {message}"),
        };
    }
    match value.ctx().json_stringify(value.clone()) {
        Ok(Some(text)) => format!("uncaught value: {}", text.to_string().unwrap_or_default()),
        _ => "uncaught exception".into(),
    }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

impl PluginManifest for ToolExecutorPtc {
    fn manifest() -> ResolverManifest {
        ResolverManifest {
            id: "tool-executor-ptc".into(),
            name: "tool-executor-ptc".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            form: PluginForm::Function,
            registrations: vec![RegistrationDecl::Service { name: sdk::PTC_EXECUTOR.into() }],
            config_schema: None,
        }
    }
}

#[derive(Default)]
pub struct PtcExecutorPlugin {
    limits: PtcLimits,
}

impl PtcExecutorPlugin {
    pub fn new(limits: PtcLimits) -> Self { Self { limits } }
}

impl FunctionPlugin for PtcExecutorPlugin {
    fn name(&self) -> &str { "tool-executor-ptc" }
    fn registrations(&self) -> Vec<RegistrationDecl> { ToolExecutorPtc::manifest().registrations }
    fn apply(&self, _c: &Config, ctx: &mut dyn Ctx) -> Result<Disposer> {
        // Registered as `Arc<dyn ToolExecutor>`, the shape the runtime resolves.
        let executor: Arc<dyn ToolExecutor> = Arc::new(ToolExecutorPtc::new(self.limits));
        let h = ctx.effect(Contribution::Service {
            name: sdk::PTC_EXECUTOR.into(),
            service: Arc::new(executor),
        })?;
        let mut d = Disposer::default();
        d.push(h);
        Ok(d)
    }
}

#[cfg(test)]
mod tests;
