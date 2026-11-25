//! `/context`: the model-facing context of a session's next turn, verbatim.
//!
//! A built-in command of the runtime, because only the runtime knows the
//! selected executor, the registered tools and the session together. It
//! reports `JobManager::preview`, which is built from the same pieces a turn
//! is, so what it shows is what the model would get.

use crate::job_manager::{JobManager, TurnPreview};
use protocol::{CommandResult, CommandSpec, Role};
use sdk::{CommandContext, Result, SlashCommandHandler};
use std::sync::Arc;

pub const CONTEXT_COMMAND: &str = "context";

pub struct ContextCommand {
    jobs: Arc<JobManager>,
}

impl ContextCommand {
    pub fn new(jobs: Arc<JobManager>) -> Self {
        Self { jobs }
    }
}

impl SlashCommandHandler for ContextCommand {
    fn spec(&self) -> CommandSpec {
        CommandSpec {
            name: CONTEXT_COMMAND.into(),
            description: "show what the model receives: system prompt, executor, tools, messages".into(),
            args_schema: None,
            subcommands: vec![],
            owner: Some("runtime".into()),
        }
    }

    fn execute(&self, ctx: &CommandContext) -> Result<CommandResult> {
        let preview = self.jobs.preview(&ctx.session_id, &ctx.cwd)?;
        Ok(CommandResult { text: render(&preview), session_id: None })
    }
}

/// Rough token count: four characters per token. An estimate, labelled so.
fn tokens(chars: usize) -> usize {
    chars.div_ceil(4)
}

fn heading(out: &mut Vec<String>, title: String) {
    if !out.is_empty() {
        out.push(String::new());
    }
    out.push(format!("── {title}"));
}

/// The report, one entry per line. Every model-facing text appears in full.
pub fn render(p: &TurnPreview) -> String {
    let system_chars: usize = p.system.iter().map(String::len).sum();
    let tool_texts: Vec<String> = p.tools.iter()
        .map(|t| serde_json::to_string(t).unwrap_or_default())
        .collect();
    let tool_chars: usize = tool_texts.iter().map(String::len).sum();
    // System messages are listed under their own heading, not again below.
    let conversation: Vec<&protocol::SessionMessage> = p.history.iter().filter(|m| m.role != Role::System).collect();
    let message_chars: usize = conversation.iter()
        .map(|m| m.content.len() + m.tool_calls.iter().map(|c| c.name.len() + c.arguments.len()).sum::<usize>())
        .sum();

    let mut out = vec![
        format!("model      {}", p.model.as_deref().unwrap_or("none")),
        format!("executor   {}", p.executor),
        format!(
            "estimate   ~{} tokens (system ~{}, tools ~{}, messages ~{})",
            tokens(system_chars + tool_chars + message_chars),
            tokens(system_chars), tokens(tool_chars), tokens(message_chars),
        ),
    ];

    heading(&mut out, "system prompt".into());
    if p.system.is_empty() {
        out.push("(empty: no system instructions are sent)".into());
    }
    for text in &p.system {
        out.extend(text.lines().map(str::to_string));
    }

    heading(&mut out, format!("tools offered to the model ({})", p.tools.len()));
    for tool in &p.tools {
        out.push(format!("▸ {}", tool["name"].as_str().unwrap_or("?")));
        out.extend(tool["description"].as_str().unwrap_or("").lines().map(|l| format!("  {l}")));
        out.push("  parameters:".into());
        let schema = serde_json::to_string_pretty(&tool["parameters"]).unwrap_or_default();
        out.extend(schema.lines().map(|l| format!("    {l}")));
    }

    heading(&mut out, format!("messages ({})", conversation.len()));
    for m in conversation {
        let role = match m.role {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::System => "system",
            Role::Tool => "tool",
        };
        let mut label = format!("▸ {role}");
        if m.role == Role::Tool {
            label.push_str(&format!(" · answers {}", m.id));
        }
        if !m.tool_calls.is_empty() {
            let names: Vec<&str> = m.tool_calls.iter().map(|c| c.name.as_str()).collect();
            label.push_str(&format!(" · calls {}", names.join(", ")));
        }
        out.push(label);
        out.extend(m.content.lines().map(|l| format!("  {l}")));
        for call in &m.tool_calls {
            out.push(format!("  {}({})", call.name, call.arguments));
        }
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{SessionMessage, ToolCall};
    use serde_json::json;

    fn message(role: Role, content: &str) -> SessionMessage {
        SessionMessage { id: String::new(), role, content: content.into(), created_at: 0, tool_calls: vec![] }
    }

    #[test]
    fn the_report_shows_every_model_facing_text_in_full() {
        let mut call = message(Role::Assistant, "");
        call.tool_calls.push(ToolCall { id: "c1".into(), name: "run_js".into(), arguments: "{\"code\":\"return 1\"}".into() });
        let mut answer = message(Role::Tool, "1");
        answer.id = "c1".into();
        let preview = TurnPreview {
            model: Some("m/x".into()),
            executor: "ptc".into(),
            system: vec![],
            tools: vec![json!({
                "name": "run_js",
                "description": "Run a program.\n\ndeclare const tools: {};",
                "parameters": {"type": "object", "required": ["code"]},
            })],
            history: vec![message(Role::User, "go"), call, answer],
        };
        assert_eq!(render(&preview), [
            "model      m/x",
            "executor   ptc",
            "estimate   ~39 tokens (system ~0, tools ~32, messages ~7)",
            "",
            "── system prompt",
            "(empty: no system instructions are sent)",
            "",
            "── tools offered to the model (1)",
            "▸ run_js",
            "  Run a program.",
            "  ",
            "  declare const tools: {};",
            "  parameters:",
            "    {",
            "      \"required\": [",
            "        \"code\"",
            "      ],",
            "      \"type\": \"object\"",
            "    }",
            "",
            "── messages (3)",
            "▸ user",
            "  go",
            "▸ assistant · calls run_js",
            "  run_js({\"code\":\"return 1\"})",
            "▸ tool · answers c1",
            "  1",
        ].join("\n"));
    }

    #[test]
    fn system_instructions_are_shown_verbatim() {
        let preview = TurnPreview {
            model: None, executor: "native".into(),
            system: vec!["Be brief.\nUse tools.".into()],
            tools: vec![], history: vec![message(Role::System, "Be brief.\nUse tools.")],
        };
        let text = render(&preview);
        assert!(text.contains("── system prompt\nBe brief.\nUse tools.\n"), "{text}");
        assert!(text.starts_with("model      none\n"));
    }
}
