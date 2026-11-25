//! `/recall <words>`: find past conversations by what was asked or answered.
//!
//! Lexical (FTS5): every word must appear, case and accents ignored. Meaning-
//! based search needs an embedding model and is not here.

use crate::sqlite::SqliteSessionStore;
use protocol::{CommandResult, CommandSpec, Role};
use sdk::{
    CommandContext, Config, Contribution, Ctx, Disposer, FunctionPlugin, PluginForm,
    PluginManifest, RegistrationDecl, ResolverManifest, Result, SlashCommandHandler,
};
use std::sync::Arc;

pub const RECALL_COMMAND: &str = "recall";

/// Results shown per search.
const LIMIT: usize = 8;

/// One matching message.
#[derive(Debug, Clone, PartialEq)]
pub struct Recall {
    pub session_id: String,
    pub title: String,
    pub cwd: String,
    pub role: Role,
    /// The matching words between «guillemets», with context.
    pub snippet: String,
    pub updated_at: i64,
}

pub struct RecallCommand {
    store: Arc<SqliteSessionStore>,
}

impl RecallCommand {
    pub fn new(store: Arc<SqliteSessionStore>) -> Self {
        Self { store }
    }
}

impl SlashCommandHandler for RecallCommand {
    fn spec(&self) -> CommandSpec {
        CommandSpec {
            name: RECALL_COMMAND.into(),
            description: "search past sessions for words you or the model wrote".into(),
            args_schema: Some(serde_json::json!({"type": "string"})),
            subcommands: vec![],
            owner: Some("session".into()),
        }
    }

    fn execute(&self, ctx: &CommandContext) -> Result<CommandResult> {
        let query = ctx.arg.trim();
        if query.is_empty() {
            return Ok(CommandResult { text: "usage: /recall <words>".into(), session_id: None });
        }
        Ok(CommandResult { text: render(query, &self.store.search(query, LIMIT)?), session_id: None })
    }
}

fn render(query: &str, hits: &[Recall]) -> String {
    if hits.is_empty() {
        return format!("nothing found for \"{query}\"");
    }
    let mut out = Vec::new();
    for hit in hits {
        let who = if hit.role == Role::User { "you" } else { "model" };
        let title = if hit.title.is_empty() { "(untitled)" } else { hit.title.as_str() };
        out.push(format!("{title} · {} · {}", hit.session_id, hit.cwd));
        out.push(format!("  {who}: {}", hit.snippet.split_whitespace().collect::<Vec<_>>().join(" ")));
    }
    out.push("open one with /resume <id>".into());
    out.join("\n")
}

impl PluginManifest for RecallCommand {
    fn manifest() -> ResolverManifest {
        ResolverManifest {
            id: "session".into(),
            name: "session".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            form: PluginForm::Function,
            registrations: vec![RegistrationDecl::Command { name: RECALL_COMMAND.into() }],
            config_schema: None,
        }
    }
}

/// Registers `/recall` over the store the composition root opened.
pub struct SessionRecallPlugin {
    store: Arc<SqliteSessionStore>,
}

impl SessionRecallPlugin {
    pub fn new(store: Arc<SqliteSessionStore>) -> Self {
        Self { store }
    }
}

impl FunctionPlugin for SessionRecallPlugin {
    fn name(&self) -> &str { "session" }
    fn registrations(&self) -> Vec<RegistrationDecl> { RecallCommand::manifest().registrations }
    fn apply(&self, _c: &Config, ctx: &mut dyn Ctx) -> Result<Disposer> {
        let mut d = Disposer::default();
        d.push(ctx.effect(Contribution::Command { handler: Box::new(RecallCommand::new(self.store.clone())) })?);
        Ok(d)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sdk::SessionStore;

    fn ctx(arg: &str) -> CommandContext {
        CommandContext { arg: arg.into(), session_id: String::new(), cwd: ".".into() }
    }

    #[test]
    fn recall_lists_sessions_with_the_id_to_resume() {
        let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
        let s = store.create("/proj").unwrap();
        store.push(&s.id, Role::User, "come configuro il parser?".into()).unwrap();
        let out = RecallCommand::new(store).execute(&ctx("parser")).unwrap().text;
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], format!("come configuro il parser? · {} · /proj", s.id));
        assert_eq!(lines[1], "  you: come configuro il «parser»?");
        assert_eq!(lines.last().copied(), Some("open one with /resume <id>"));
    }

    #[test]
    fn empty_and_unmatched_queries_say_so() {
        let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
        let cmd = RecallCommand::new(store);
        assert_eq!(cmd.execute(&ctx("  ")).unwrap().text, "usage: /recall <words>");
        assert_eq!(cmd.execute(&ctx("zebra")).unwrap().text, "nothing found for \"zebra\"");
    }

    #[test]
    fn the_plugin_declares_the_command_it_registers() {
        let plugin = SessionRecallPlugin::new(Arc::new(SqliteSessionStore::in_memory().unwrap()));
        assert!(matches!(
            plugin.registrations().as_slice(),
            [RegistrationDecl::Command { name }] if name == RECALL_COMMAND
        ));
    }
}
