//! Model-facing rendering of tool calls, results and diagnostics.
//!
//! All text here is stable, task-relevant and pinned by the tests below. It
//! contains no UI, transport or implementation vocabulary.

use sdk::{
    Config, Contribution, Ctx, Disposer, Error, FunctionPlugin, PluginForm, PluginManifest,
    RegistrationDecl, ResolverManifest, Result, ToolOutput, ToolPresentation,
    TOOL_PRESENTATION,
};
use serde_json::Value;

/// Preferred argument keys, in display order.
const PREFERRED_ARGS: [&str; 5] = ["path", "command", "query", "file", "url"];
/// Default model-facing cap on one tool result, in characters.
pub const DEFAULT_MAX_RESULT_CHARS: usize = 24_000;

/// Renders tool calls, results and diagnostics for the model. Results longer
/// than `max_result_chars` keep their head and tail around an explicit notice.
#[derive(Debug, Clone, Copy)]
pub struct AgentToolPresentation {
    max_result_chars: usize,
}

impl Default for AgentToolPresentation {
    fn default() -> Self {
        Self { max_result_chars: DEFAULT_MAX_RESULT_CHARS }
    }
}

impl AgentToolPresentation {
    pub fn new(max_result_chars: usize) -> Self {
        Self { max_result_chars: max_result_chars.max(100) }
    }
}

impl ToolPresentation for AgentToolPresentation {
    fn render_call(&self, name: &str, args: &Value) -> String {
        if let Some(obj) = args.as_object() {
            for key in PREFERRED_ARGS {
                if let Some(value) = obj.get(key) {
                    return format!("{name}({key}={})", compact(value));
                }
            }
        }
        match args {
            Value::Null => format!("{name}()"),
            Value::Object(map) if map.is_empty() => format!("{name}()"),
            other => format!("{name}({})", truncate_inline(&compact(other), 160)),
        }
    }

    fn render_result(&self, output: &ToolOutput) -> String {
        let shown = truncate_middle(&output.content, self.max_result_chars);
        if output.is_error {
            format!("Error: {shown}")
        } else {
            shown
        }
    }

    fn render_diagnostic(&self, error: &Error) -> String {
        match error {
            Error::AuthRequired { provider } => format!(
                "Authentication required for {provider}. Set a credential with /connect {provider}."
            ),
            Error::PermissionDenied(what) => format!(
                "Permission denied: {what}. Adjust the tool policy or workspace scope, then retry."
            ),
            Error::RateLimited { provider, retry_after } => match retry_after {
                Some(secs) => format!(
                    "Rate limited by {provider}. Retry in {secs}s or choose another model."
                ),
                None => format!(
                    "Rate limited by {provider}. Retry shortly or choose another model."
                ),
            },
            Error::Cancelled => "Cancelled by the user.".into(),
            Error::Timeout { limit_ms } => format!(
                "Stopped after {limit_ms} ms without finishing. Split the work into smaller steps."
            ),
            Error::InvalidInput(what) => {
                format!("Invalid input: {what}. Check the tool arguments and try again.")
            }
            Error::Unavailable(what) => {
                format!("Unavailable: {what}. Verify the provider or service configuration.")
            }
            Error::Internal(what) => {
                format!("Internal error: {what}. Retry; if it persists, report the failure.")
            }
        }
    }
}

impl PluginManifest for AgentToolPresentation {
    fn manifest() -> ResolverManifest {
        ResolverManifest {
            id: TOOL_PRESENTATION.into(),
            name: TOOL_PRESENTATION.into(),
            version: env!("CARGO_PKG_VERSION").into(),
            form: PluginForm::Function,
            registrations: vec![RegistrationDecl::Service { name: TOOL_PRESENTATION.into() }],
            config_schema: None,
        }
    }
}

#[derive(Default)]
pub struct AgentToolPresentationPlugin {
    presentation: AgentToolPresentation,
}

impl AgentToolPresentationPlugin {
    pub fn new(presentation: AgentToolPresentation) -> Self { Self { presentation } }
}

impl FunctionPlugin for AgentToolPresentationPlugin {
    fn name(&self) -> &str { TOOL_PRESENTATION }
    fn registrations(&self) -> Vec<RegistrationDecl> { AgentToolPresentation::manifest().registrations }
    fn apply(&self, _c: &Config, ctx: &mut dyn Ctx) -> Result<Disposer> {
        let presentation: std::sync::Arc<dyn ToolPresentation> = std::sync::Arc::new(self.presentation);
        let h = ctx.effect(Contribution::Service {
            name: TOOL_PRESENTATION.into(),
            // Registered as `Arc<dyn ToolPresentation>`, the shape the runtime resolves.
            service: std::sync::Arc::new(presentation),
        })?;
        let mut d = Disposer::default();
        d.push(h);
        Ok(d)
    }
}

fn compact(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => serde_json::to_string(other).unwrap_or_else(|_| other.to_string()),
    }
}

fn truncate_inline(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        text.into()
    } else {
        format!("{}…", &text[..truncate_at_char_boundary(text, limit)])
    }
}

/// Keep the first two thirds and the last third of `limit` characters, with a
/// notice in between that says exactly what is missing and how to get it.
fn truncate_middle(text: &str, limit: usize) -> String {
    let total = text.chars().count();
    if total <= limit {
        return text.to_string();
    }
    let head = limit * 2 / 3;
    let tail = limit - head;
    let head_end = text.char_indices().nth(head).map_or(text.len(), |(i, _)| i);
    let tail_start = text.char_indices().nth(total - tail).map_or(text.len(), |(i, _)| i);
    format!(
        "{}\n[truncated: showing characters 1–{head} and the last {tail} of {total}; \
         read a smaller range with offset/limit]\n{}",
        &text[..head_end],
        &text[tail_start..],
    )
}

fn truncate_at_char_boundary(text: &str, limit: usize) -> usize {
    let mut end = limit.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn render_call_uses_known_argument_keys() {
        let p = AgentToolPresentation::default();
        assert_eq!(p.render_call("read_file", &json!({"path": "src/main.rs"})), "read_file(path=src/main.rs)");
        assert_eq!(p.render_call("bash", &json!({"command": "cargo test"})), "bash(command=cargo test)");
        assert_eq!(p.render_call("search", &json!({"query": "fn main"})), "search(query=fn main)");
    }

    #[test]
    fn render_call_falls_back_to_compact_json() {
        let p = AgentToolPresentation::default();
        assert_eq!(p.render_call("ping", &json!({})), "ping()");
        assert_eq!(p.render_call("ping", &Value::Null), "ping()");
        let out = p.render_call("combine", &json!({"a": 1, "b": [true, null]}));
        assert_eq!(out, r#"combine({"a":1,"b":[true,null]})"#);
    }

    #[test]
    fn render_result_distinguishes_success_from_error() {
        let p = AgentToolPresentation::default();
        assert_eq!(p.render_result(&ToolOutput::ok("all good")), "all good");
        assert_eq!(p.render_result(&ToolOutput::err("boom")), "Error: boom");
    }

    #[test]
    fn a_long_result_keeps_head_and_tail_around_an_exact_notice() {
        let p = AgentToolPresentation::new(300);
        let body: String = (0..1000).map(|i| char::from(b'a' + (i % 26) as u8)).collect();
        let rendered = p.render_result(&ToolOutput::ok(body.clone()));
        let (head, rest) = rendered.split_once('\n').unwrap();
        let (notice, tail) = rest.split_once('\n').unwrap();
        assert_eq!(head, &body[..200], "the head is the real text, not a byte count");
        assert_eq!(tail, &body[900..]);
        assert_eq!(notice, "[truncated: showing characters 1–200 and the last 100 of 1000; read a smaller range with offset/limit]");
    }

    #[test]
    fn short_results_and_multibyte_text_are_left_intact_or_cut_on_characters() {
        let p = AgentToolPresentation::new(100);
        assert_eq!(p.render_result(&ToolOutput::ok("però")), "però");
        let accented = "è".repeat(500);
        let rendered = p.render_result(&ToolOutput::ok(accented));
        assert!(rendered.starts_with(&"è".repeat(66)) && rendered.ends_with(&"è".repeat(34)));
        assert!(rendered.contains("of 500;"));
    }

    #[test]
    fn errors_are_marked_and_truncated_too() {
        let p = AgentToolPresentation::new(100);
        let rendered = p.render_result(&ToolOutput::err("x".repeat(400)));
        assert!(rendered.starts_with("Error: xxx"));
        assert!(rendered.contains("[truncated:"));
    }

    #[test]
    fn render_diagnostic_covers_every_sdk_error_variant() {
        let p = AgentToolPresentation::default();
        let cases = [
            (Error::AuthRequired { provider: "openrouter".into() }, "Authentication required for openrouter. Set a credential with /connect openrouter."),
            (Error::PermissionDenied("write /etc".into()), "Permission denied: write /etc. Adjust the tool policy or workspace scope, then retry."),
            (Error::RateLimited { provider: "zen".into(), retry_after: Some(30) }, "Rate limited by zen. Retry in 30s or choose another model."),
            (Error::RateLimited { provider: "zen".into(), retry_after: None }, "Rate limited by zen. Retry shortly or choose another model."),
            (Error::Cancelled, "Cancelled by the user."),
            (
                Error::Timeout { limit_ms: 500 },
                "Stopped after 500 ms without finishing. Split the work into smaller steps.",
            ),
            (Error::InvalidInput("missing path".into()), "Invalid input: missing path. Check the tool arguments and try again."),
            (Error::Unavailable("no gateway".into()), "Unavailable: no gateway. Verify the provider or service configuration."),
            (Error::Internal("panic".into()), "Internal error: panic. Retry; if it persists, report the failure."),
        ];
        for (error, expected) in cases {
            assert_eq!(p.render_diagnostic(&error), expected);
        }
    }
}
