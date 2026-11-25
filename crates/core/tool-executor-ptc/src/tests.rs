use super::*;
use sdk::FsPort;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

struct NoFs;
impl FsPort for NoFs {}

/// `add({a, b})` returns the sum as text.
struct Add;
impl Tool for Add {
    fn name(&self) -> &str { "add" }
    fn description(&self) -> &str { "Add two numbers." }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {"a": {"type": "number"}, "b": {"type": "number"}}})
    }
    fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        let sum = args["a"].as_f64().unwrap_or(0.0) + args["b"].as_f64().unwrap_or(0.0);
        Ok(ToolOutput::ok(sum.to_string()))
    }
}

/// `upper({text})` returns the text in upper case.
struct Upper;
impl Tool for Upper {
    fn name(&self) -> &str { "upper" }
    fn description(&self) -> &str { "Upper-case a text." }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {"text": {"type": "string"}}})
    }
    fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        Ok(ToolOutput::ok(args["text"].as_str().unwrap_or("").to_uppercase()))
    }
}

/// Always fails as a tool (a domain failure the script can handle).
struct Broken;
impl Tool for Broken {
    fn name(&self) -> &str { "broken" }
    fn description(&self) -> &str { "Always fails." }
    fn parameters_schema(&self) -> Value { json!({"type": "object"}) }
    fn execute(&self, _args: Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        Ok(ToolOutput::err("disk on fire"))
    }
}

/// Counts how often it actually ran, to prove a denied call never executes.
struct Secret(Arc<AtomicUsize>);
impl Tool for Secret {
    fn name(&self) -> &str { "secret" }
    fn description(&self) -> &str { "Guarded." }
    fn parameters_schema(&self) -> Value { json!({"type": "object"}) }
    fn execute(&self, _args: Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput::ok("leaked"))
    }
}

fn context_with(cancelled: Arc<AtomicBool>) -> ToolContext {
    ToolContext {
        session_id: "s".into(),
        cwd: ".".into(),
        cancellation_token: Arc::new(move || cancelled.load(Ordering::SeqCst)),
        fs: Arc::new(NoFs),
    }
}

fn context() -> ToolContext {
    context_with(Arc::new(AtomicBool::new(false)))
}

fn allow_all(_: &str, _: &Value) -> Result<()> { Ok(()) }

fn run(code: &str, tools: &[&dyn Tool]) -> Result<ToolOutput> {
    run_with(ToolExecutorPtc::default(), code, tools, &context(), &allow_all)
}

fn run_with(
    executor: ToolExecutorPtc,
    code: &str,
    tools: &[&dyn Tool],
    ctx: &ToolContext,
    permission: &PermissionCheck,
) -> Result<ToolOutput> {
    executor.dispatch(RUN_JS, json!({ "code": code }), tools, ctx, permission)
}

fn short(timeout_ms: u64) -> ToolExecutorPtc {
    ToolExecutorPtc::new(PtcLimits { timeout: Duration::from_millis(timeout_ms), ..PtcLimits::default() })
}

// (a) orchestration -------------------------------------------------------

#[test]
fn one_program_chains_several_tool_calls() {
    let out = run(
        r#"
        const first = Number(tools.add({ a: 2, b: 3 }));
        const second = Number(tools.add({ a: first, b: 10 }));
        const label = tools.upper({ text: "total" });
        return { label, values: [first, second] };
        "#,
        &[&Add, &Upper],
    ).unwrap();
    assert!(!out.is_error, "{}", out.content);
    let value: Value = serde_json::from_str(&out.content).unwrap();
    assert_eq!(value, json!({ "label": "TOTAL", "values": [5, 15] }));
}

#[test]
fn a_failing_tool_throws_an_error_the_program_can_handle() {
    let out = run(
        r#"
        try { tools.broken({}); return "unreachable"; }
        catch (e) { return "handled: " + e.message; }
        "#,
        &[&Broken],
    ).unwrap();
    assert_eq!(out.content, "handled: disk on fire");
}

#[test]
fn an_unknown_tool_is_not_a_function_and_the_error_reaches_the_model() {
    let out = run("return tools.nope({});", &[&Add]).unwrap();
    assert!(out.is_error);
    assert!(out.content.starts_with("TypeError"), "{}", out.content);
}

// (b) cancellation and timeout --------------------------------------------

#[test]
fn an_infinite_loop_stops_at_the_deadline_with_a_typed_timeout() {
    let started = Instant::now();
    let err = run_with(short(200), "while (true) {}", &[], &context(), &allow_all).unwrap_err();
    assert_eq!(err, Error::Timeout { limit_ms: 200 });
    assert!(started.elapsed() < Duration::from_secs(2), "took {:?}", started.elapsed());
}

#[test]
fn a_loop_that_catches_errors_still_stops_at_the_deadline() {
    let code = "while (true) { try { while (true) {} } catch (e) {} }";
    let err = run_with(short(200), code, &[], &context(), &allow_all).unwrap_err();
    assert_eq!(err, Error::Timeout { limit_ms: 200 });
}

#[test]
fn cancellation_stops_a_running_program() {
    let flag = Arc::new(AtomicBool::new(false));
    let ctx = context_with(flag.clone());
    let trigger = flag.clone();
    let canceller = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        trigger.store(true, Ordering::SeqCst);
    });
    let started = Instant::now();
    let err = run_with(ToolExecutorPtc::default(), "while (true) {}", &[], &ctx, &allow_all).unwrap_err();
    canceller.join().unwrap();
    assert_eq!(err, Error::Cancelled);
    assert!(started.elapsed() < Duration::from_secs(5), "took {:?}", started.elapsed());
}

// (c) permission, fail closed ---------------------------------------------

#[test]
fn a_denied_call_never_runs_the_tool_and_fails_the_whole_program() {
    let runs = Arc::new(AtomicUsize::new(0));
    let secret = Secret(runs.clone());
    let deny_secret = |name: &str, _: &Value| -> Result<()> {
        if name == "secret" { Err(Error::PermissionDenied(name.into())) } else { Ok(()) }
    };
    // The program catches the error and tries to carry on: the result must
    // still be the denial, and nothing after it may run a tool.
    let code = r#"
        let note = "";
        try { tools.secret({}); } catch (e) { note = e.message; }
        tools.add({ a: 1, b: 1 });
        return "carried on: " + note;
    "#;
    let err = run_with(ToolExecutorPtc::default(), code, &[&secret, &Add], &context(), &deny_secret)
        .unwrap_err();
    assert_eq!(err, Error::PermissionDenied("secret".into()));
    assert_eq!(runs.load(Ordering::SeqCst), 0);
}

#[test]
fn every_call_goes_through_the_permission_closure() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = seen.clone();
    let record = move |name: &str, args: &Value| -> Result<()> {
        log.lock().unwrap().push((name.to_string(), args.clone()));
        Ok(())
    };
    run_with(
        ToolExecutorPtc::default(),
        "tools.add({a: 1, b: 2}); return tools.upper({text: 'x'});",
        &[&Add, &Upper],
        &context(),
        &record,
    ).unwrap();
    assert_eq!(*seen.lock().unwrap(), vec![
        ("add".to_string(), json!({"a": 1, "b": 2})),
        ("upper".to_string(), json!({"text": "x"})),
    ]);
}

#[test]
fn a_failing_permission_check_is_fatal_too() {
    let broken_policy = |_: &str, _: &Value| -> Result<()> { Err(Error::Internal("policy unavailable".into())) };
    let err = run_with(ToolExecutorPtc::default(), "return tools.add({a:1,b:1});", &[&Add], &context(), &broken_policy)
        .unwrap_err();
    assert_eq!(err, Error::Internal("policy unavailable".into()));
}

// (d) error propagation, never null ---------------------------------------

#[test]
fn a_thrown_error_reports_name_message_and_stack() {
    let out = run("function inner() { throw new Error('boom'); }\ninner();\nreturn 1;", &[]).unwrap();
    assert!(out.is_error);
    assert!(out.content.starts_with("Error: boom\n"), "{}", out.content);
    assert!(out.content.contains("inner"), "stack names the function: {}", out.content);
}

#[test]
fn a_syntax_error_is_reported() {
    let out = run("return (;", &[]).unwrap();
    assert!(out.is_error);
    assert!(out.content.starts_with("SyntaxError"), "{}", out.content);
}

#[test]
fn typescript_annotations_are_a_syntax_error_as_the_description_says() {
    let out = run("const n: number = 1;\nreturn n;", &[]).unwrap();
    assert!(out.is_error);
    assert!(out.content.starts_with("SyntaxError"), "{}", out.content);
}

#[test]
fn a_thrown_non_error_value_is_reported_verbatim() {
    let out = run("throw { code: 7 };", &[]).unwrap();
    assert!(out.is_error);
    assert_eq!(out.content, "uncaught value: {\"code\":7}");
}

#[test]
fn values_that_are_not_json_are_errors_not_null() {
    for (code, needle) in [
        ("return function () {};", "cannot be converted to JSON (function)"),
        ("return undefined;", "returned no value"),
        ("let x = 1;", "returned no value"),
        ("return 10n;", "BigInt"),
        ("const a = {}; a.self = a; return a;", "TypeError"),
    ] {
        let out = run(code, &[]).unwrap();
        assert!(out.is_error, "{code} must fail, got {}", out.content);
        assert!(out.content.contains(needle), "{code}: {}", out.content);
        assert_ne!(out.content, "null");
    }
}

#[test]
fn a_string_result_comes_back_as_plain_text_and_other_values_as_json() {
    let text = run(r#"return 'line "one"\nline two';"#, &[]).unwrap();
    assert_eq!(text.content, "line \"one\"\nline two", "no quotes added, no escaped newlines");
    let object = run("return { a: 'x\\ny' };", &[]).unwrap();
    assert_eq!(object.content, r#"{"a":"x\ny"}"#);
    let number = run("return 42;", &[]).unwrap();
    assert_eq!(number.content, "42");
}

/// Returns 100 000 characters, far over any model-facing cap.
struct Huge;
impl Tool for Huge {
    fn name(&self) -> &str { "huge" }
    fn description(&self) -> &str { "Big output." }
    fn parameters_schema(&self) -> Value { json!({"type": "object"}) }
    fn execute(&self, _args: Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        Ok(ToolOutput::ok("z".repeat(100_000)))
    }
}

#[test]
fn calls_inside_a_program_see_the_whole_output() {
    // Truncation belongs to what the model receives; a program works on the
    // full data and returns only what matters.
    let out = run("const t = tools.huge({}); return { length: t.length, last: t.slice(-3) };", &[&Huge]).unwrap();
    assert_eq!(out.content, r#"{"length":100000,"last":"zzz"}"#);
}

#[test]
fn null_is_a_legitimate_value_when_returned_on_purpose() {
    let out = run("return null;", &[]).unwrap();
    assert!(!out.is_error);
    assert_eq!(out.content, "null");
}

#[test]
fn running_out_of_memory_is_an_error_and_the_executor_survives() {
    let small = ToolExecutorPtc::new(PtcLimits { memory_bytes: 8 * 1024 * 1024, ..PtcLimits::default() });
    let out = run_with(small, "const a = []; while (true) a.push('x'.repeat(1024));", &[], &context(), &allow_all)
        .unwrap();
    assert!(out.is_error);
    assert_eq!(out.content, "out of memory: the program used more than 8 MiB");
    let after = run_with(small, "return 1 + 1;", &[], &context(), &allow_all).unwrap();
    assert_eq!(after.content, "2");
}

#[test]
fn a_deliberate_throw_null_is_not_mistaken_for_running_out_of_memory() {
    let out = run("throw null;", &[]).unwrap();
    assert!(out.is_error);
    assert_eq!(out.content, "uncaught value: null");
}

#[test]
fn a_program_that_recovers_from_running_out_of_memory_keeps_its_result() {
    let small = ToolExecutorPtc::new(PtcLimits { memory_bytes: 8 * 1024 * 1024, ..PtcLimits::default() });
    let code = "try { const a = []; while (true) a.push('x'.repeat(1024)); } catch (e) {} return 'recovered';";
    let out = run_with(small, code, &[], &context(), &allow_all).unwrap();
    assert_eq!(out.content, "recovered");
}

#[test]
fn unbounded_recursion_hits_the_stack_limit() {
    let out = run("function f() { return f() + 1; } return f();", &[]).unwrap();
    assert!(out.is_error);
    assert!(out.content.starts_with("RangeError: Maximum call stack size exceeded"), "{}", out.content);
}

// Isolation ---------------------------------------------------------------

#[test]
fn only_builtins_and_tools_exist() {
    let out = run(
        r#"return [typeof fetch, typeof require, typeof process, typeof console,
                   typeof __host_call, typeof std, typeof os, typeof tools.add,
                   Object.isFrozen(tools)];"#,
        &[&Add],
    ).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&out.content).unwrap(),
        json!(["undefined", "undefined", "undefined", "undefined",
               "undefined", "undefined", "undefined", "function", true]),
    );
}

#[test]
fn each_program_starts_from_a_clean_interpreter() {
    run("globalThis.leak = 42; return 1;", &[]).unwrap();
    let out = run("return typeof leak;", &[]).unwrap();
    assert_eq!(out.content, "undefined");
}

// Model-facing contract ---------------------------------------------------

#[test]
fn the_model_is_offered_one_code_tool_listing_every_tool() {
    let defs = ToolExecutorPtc::default().definitions(&[&Add, &Upper]);
    assert_eq!(defs.len(), 1);
    assert_eq!(defs[0]["name"], RUN_JS);
    assert_eq!(defs[0]["parameters"]["required"], json!(["code"]));
    let description = defs[0]["description"].as_str().unwrap();
    assert!(description.starts_with(
        "Run a JavaScript program and return its result. Write plain JavaScript: the program \
         runs as written, so TypeScript syntax such as type annotations is a syntax error; the \
         declarations below only describe the tools. "
    ), "{description}");
    assert!(description.ends_with(concat!(
        "\n\nAvailable tools:\n",
        "declare const tools: {\n",
        "  /** Add two numbers. */\n",
        "  add(args?: {\n",
        "    a?: number;\n",
        "    b?: number;\n",
        "  }): string;\n",
        "  /** Upper-case a text. */\n",
        "  upper(args?: {\n",
        "    text?: string;\n",
        "  }): string;\n",
        "};",
    )), "{description}");
}

#[test]
fn calls_other_than_run_js_and_missing_code_are_tool_errors() {
    let executor = ToolExecutorPtc::default();
    let other = executor.dispatch("add", json!({}), &[&Add], &context(), &allow_all).unwrap();
    assert!(other.is_error && other.content.contains(RUN_JS));
    let missing = executor.dispatch(RUN_JS, json!({}), &[&Add], &context(), &allow_all).unwrap();
    assert!(missing.is_error && missing.content.contains("`code`"));
}

/// Requires `path` (string); counts how often it really ran.
struct Strict(Arc<AtomicUsize>);
impl Tool for Strict {
    fn name(&self) -> &str { "strict" }
    fn description(&self) -> &str { "Needs a path." }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"], "additionalProperties": false})
    }
    fn execute(&self, _args: Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput::ok("ran"))
    }
}

#[test]
fn invalid_arguments_never_reach_the_tool_and_the_program_sees_why() {
    let runs = Arc::new(AtomicUsize::new(0));
    let strict = Strict(runs.clone());
    let out = run(
        "try { tools.strict({ pth: 1 }); return 'ran'; } catch (e) { return e.message; }",
        &[&strict],
    ).unwrap();
    assert_eq!(
        out.content,
        "invalid arguments for strict: missing required field `path` (string); unknown field `pth`; expected one of: path",
    );
    assert_eq!(runs.load(Ordering::SeqCst), 0);
}

#[test]
fn validation_comes_after_the_permission_decision() {
    let runs = Arc::new(AtomicUsize::new(0));
    let strict = Strict(runs.clone());
    let deny = |name: &str, _: &Value| -> Result<()> { Err(Error::PermissionDenied(name.into())) };
    let err = run_with(ToolExecutorPtc::default(), "return tools.strict({});", &[&strict], &context(), &deny)
        .unwrap_err();
    assert_eq!(err, Error::PermissionDenied("strict".into()));
    assert_eq!(runs.load(Ordering::SeqCst), 0);
}

#[test]
fn the_plugin_registers_the_executor_under_the_ptc_key() {
    let plugin = PtcExecutorPlugin::default();
    assert!(matches!(
        plugin.registrations().as_slice(),
        [RegistrationDecl::Service { name }] if name == sdk::PTC_EXECUTOR
    ));
}
