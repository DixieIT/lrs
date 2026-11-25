//! Contributes the active tool set through `ctx.effect()`.
//!
//! The filesystem tools reach files only through `ToolContext::fs`, the port
//! the composition root supplies; they never touch `std::fs`. Relative paths
//! are resolved against the turn's working directory. Where a path may lead
//! is the port's decision, not the tool's.

use sdk::{
    Config, Contribution, Ctx, Disposer, Error, FunctionPlugin, PluginForm, PluginManifest,
    RegistrationDecl, ResolverManifest, Result, Tool, ToolContext, ToolOutput,
};
use serde_json::{json, Value};
use std::path::PathBuf;

pub const READ_FILE: &str = "read_file";
pub const WRITE_FILE: &str = "write_file";
pub const EDIT_FILE: &str = "edit_file";

/// The path argument, resolved against the turn's working directory.
fn path_arg(args: &Value, ctx: &ToolContext) -> std::result::Result<PathBuf, String> {
    match args.get("path").and_then(Value::as_str) {
        Some(p) if !p.trim().is_empty() => Ok(PathBuf::from(&ctx.cwd).join(p)),
        _ => Err("missing string argument `path`".into()),
    }
}

/// A port refusal or I/O failure is something the model can react to (wrong
/// path, outside the project): it becomes a tool error. Cancellation and
/// internal faults stay errors of the turn.
fn as_output(result: Result<String>) -> Result<ToolOutput> {
    match result {
        Ok(text) => Ok(ToolOutput::ok(text)),
        Err(e @ (Error::PermissionDenied(_) | Error::InvalidInput(_) | Error::Unavailable(_))) => {
            Ok(ToolOutput::err(e.to_string()))
        }
        Err(e) => Err(e),
    }
}

pub struct ReadFile;
impl Tool for ReadFile {
    fn name(&self) -> &str { READ_FILE }
    fn description(&self) -> &str {
        "Read a text file and return its content. Give offset and limit to read a range of lines; \
         the range is followed by a line saying where it ends and how to continue."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File path, relative to the project root."},
                "offset": {"type": "integer", "description": "First line to read, starting at 1."},
                "limit": {"type": "integer", "description": "Maximum number of lines to read."},
            },
            "required": ["path"],
        })
    }
    fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let path = match path_arg(&args, ctx) {
            Ok(p) => p,
            Err(reason) => return Ok(ToolOutput::err(reason)),
        };
        let offset = args.get("offset").and_then(Value::as_i64);
        let limit = args.get("limit").and_then(Value::as_i64);
        let content = match ctx.fs.read(&path) {
            Ok(text) => text,
            Err(e) => return as_output(Err(e)),
        };
        if offset.is_none() && limit.is_none() {
            return Ok(ToolOutput::ok(content));
        }
        Ok(match line_range(&content, offset.unwrap_or(1), limit) {
            Ok(text) => ToolOutput::ok(text),
            Err(reason) => ToolOutput::err(reason),
        })
    }
}

/// Lines `offset..offset+limit` of `content`, verbatim (no line numbers, so
/// the text can be quoted back to `edit_file`), then one line with the range
/// and, when the file goes on, the offset to continue from.
fn line_range(content: &str, offset: i64, limit: Option<i64>) -> std::result::Result<String, String> {
    if offset < 1 {
        return Err("offset starts at 1".into());
    }
    if limit.is_some_and(|l| l < 1) {
        return Err("limit must be at least 1".into());
    }
    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();
    let start = usize::try_from(offset - 1).unwrap_or(usize::MAX);
    if start >= total {
        let lines = if total == 1 { "line" } else { "lines" };
        return Err(format!("offset {offset} is past the end: the file has {total} {lines}"));
    }
    let end = match limit {
        Some(l) => start.saturating_add(usize::try_from(l).unwrap_or(usize::MAX)).min(total),
        None => total,
    };
    let mut out = lines[start..end].join("\n");
    out.push_str(&format!("\n[lines {}–{end} of {total}", start + 1));
    if end < total {
        out.push_str(&format!("; continue with offset={}", end + 1));
    }
    out.push(']');
    Ok(out)
}

pub struct EditFile;
impl Tool for EditFile {
    fn name(&self) -> &str { EDIT_FILE }
    fn description(&self) -> &str {
        "Replace an exact piece of text in a file. `old` must match the file exactly, whitespace \
         included, and occur once; set replace_all to change every occurrence."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File path, relative to the project root."},
                "old": {"type": "string", "description": "The exact text to replace."},
                "new": {"type": "string", "description": "The text to put in its place."},
                "replace_all": {"type": "boolean", "description": "Replace every occurrence instead of exactly one."},
            },
            "required": ["path", "old", "new"],
        })
    }
    fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let path = match path_arg(&args, ctx) {
            Ok(p) => p,
            Err(reason) => return Ok(ToolOutput::err(reason)),
        };
        let (Some(old), Some(new)) = (
            args.get("old").and_then(Value::as_str),
            args.get("new").and_then(Value::as_str),
        ) else {
            return Ok(ToolOutput::err("missing string arguments `old` and `new`"));
        };
        let replace_all = args.get("replace_all").and_then(Value::as_bool).unwrap_or(false);
        let content = match ctx.fs.read(&path) {
            Ok(text) => text,
            Err(e) => return as_output(Err(e)),
        };
        let edited = match replace(&content, old, new, replace_all) {
            Ok(edited) => edited,
            Err(reason) => return Ok(ToolOutput::err(format!("{}: {reason}", display(&args)))),
        };
        let written = ctx.fs.write(&path, &edited.text).map(|()| format!(
            "replaced {} occurrence{} in {}",
            edited.count,
            if edited.count == 1 { "" } else { "s" },
            display(&args),
        ));
        as_output(written)
    }
}

struct Edited {
    text: String,
    count: usize,
}

/// The edit itself, decided before anything is written. A file with CRLF line
/// endings is matched with `old` and `new` converted to CRLF, since the model
/// writes `\n` whatever the file uses.
fn replace(content: &str, old: &str, new: &str, replace_all: bool) -> std::result::Result<Edited, String> {
    if old.is_empty() {
        return Err("`old` must not be empty".into());
    }
    if old == new {
        return Err("`old` and `new` are the same; nothing would change".into());
    }
    let (old, new) = if !content.contains(old) && content.contains("\r\n") && old.contains('\n') {
        (to_crlf(old), to_crlf(new))
    } else {
        (old.to_string(), new.to_string())
    };
    let count = content.matches(old.as_str()).count();
    match count {
        0 => Err("`old` was not found; read the file and copy the exact text, whitespace included".into()),
        n if n > 1 && !replace_all => Err(format!(
            "`old` occurs {n} times; include more surrounding text to make it unique, or set replace_all to true"
        )),
        n => Ok(Edited {
            text: if replace_all { content.replace(&old, &new) } else { content.replacen(&old, &new, 1) },
            count: if replace_all { n } else { 1 },
        }),
    }
}

fn to_crlf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\n', "\r\n")
}

pub struct WriteFile;
impl Tool for WriteFile {
    fn name(&self) -> &str { WRITE_FILE }
    fn description(&self) -> &str { "Create or replace a text file with the given content." }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File path, relative to the project root."},
                "content": {"type": "string", "description": "The complete new content of the file."},
            },
            "required": ["path", "content"],
        })
    }
    fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let path = match path_arg(&args, ctx) {
            Ok(p) => p,
            Err(reason) => return Ok(ToolOutput::err(reason)),
        };
        let Some(content) = args.get("content").and_then(Value::as_str) else {
            return Ok(ToolOutput::err("missing string argument `content`"));
        };
        let written = ctx.fs.write(&path, content)
            .map(|()| format!("wrote {} bytes to {}", content.len(), display(&args)));
        as_output(written)
    }
}

fn display(args: &Value) -> &str {
    args.get("path").and_then(Value::as_str).unwrap_or("")
}

pub struct ToolsPlugin;

impl PluginManifest for ToolsPlugin {
    fn manifest() -> ResolverManifest {
        ResolverManifest {
            id: "tools".into(),
            name: "tools".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            form: PluginForm::Function,
            registrations: [READ_FILE, WRITE_FILE, EDIT_FILE].iter()
                .map(|name| RegistrationDecl::Tool { name: (*name).into() })
                .collect(),
            config_schema: None,
        }
    }
}

impl FunctionPlugin for ToolsPlugin {
    fn name(&self) -> &str { "tools" }
    fn registrations(&self) -> Vec<RegistrationDecl> { Self::manifest().registrations }
    fn apply(&self, _c: &Config, ctx: &mut dyn Ctx) -> Result<Disposer> {
        let mut d = Disposer::default();
        let tools: [Box<dyn Tool>; 3] = [Box::new(ReadFile), Box::new(WriteFile), Box::new(EditFile)];
        for tool in tools {
            d.push(ctx.effect(Contribution::Tool { tool })?);
        }
        Ok(d)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sdk::FsPort;
    use std::collections::BTreeMap;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    /// In-memory filesystem confined to `/p`, like the real adapter's root.
    #[derive(Default)]
    struct MemFs(Mutex<BTreeMap<PathBuf, String>>);
    impl MemFs {
        fn inside(path: &Path) -> Result<()> {
            if path.starts_with("/p") { Ok(()) } else { Err(Error::PermissionDenied(format!("{} is outside the project", path.display()))) }
        }
    }
    impl FsPort for MemFs {
        fn read(&self, path: &Path) -> Result<String> {
            Self::inside(path)?;
            self.0.lock().unwrap().get(path).cloned()
                .ok_or_else(|| Error::InvalidInput(format!("{} does not exist", path.display())))
        }
        fn write(&self, path: &Path, content: &str) -> Result<()> {
            Self::inside(path)?;
            self.0.lock().unwrap().insert(path.to_path_buf(), content.into());
            Ok(())
        }
    }

    fn ctx(fs: Arc<MemFs>) -> ToolContext {
        ToolContext { session_id: "s".into(), cwd: "/p".into(), cancellation_token: Arc::new(|| false), fs }
    }

    #[test]
    fn write_then_read_relative_to_the_working_directory() {
        let fs = Arc::new(MemFs::default());
        let c = ctx(fs.clone());
        let wrote = WriteFile.execute(json!({"path": "a.txt", "content": "hi"}), &c).unwrap();
        assert_eq!(wrote.content, "wrote 2 bytes to a.txt");
        assert_eq!(ReadFile.execute(json!({"path": "a.txt"}), &c).unwrap().content, "hi");
    }

    #[test]
    fn refusals_and_bad_arguments_reach_the_model_as_tool_errors() {
        let c = ctx(Arc::new(MemFs::default()));
        let outside = ReadFile.execute(json!({"path": "/etc/passwd"}), &c).unwrap();
        assert!(outside.is_error && outside.content.contains("outside the project"), "{}", outside.content);
        let missing = ReadFile.execute(json!({"path": "nope.txt"}), &c).unwrap();
        assert!(missing.is_error);
        let no_path = ReadFile.execute(json!({}), &c).unwrap();
        assert_eq!(no_path.content, "missing string argument `path`");
        let no_content = WriteFile.execute(json!({"path": "x"}), &c).unwrap();
        assert_eq!(no_content.content, "missing string argument `content`");
    }

    #[test]
    fn model_facing_text_is_pinned() {
        assert_eq!(
            ReadFile.description(),
            "Read a text file and return its content. Give offset and limit to read a range of lines; \
             the range is followed by a line saying where it ends and how to continue.",
        );
        assert_eq!(WriteFile.description(), "Create or replace a text file with the given content.");
        assert_eq!(
            EditFile.description(),
            "Replace an exact piece of text in a file. `old` must match the file exactly, whitespace \
             included, and occur once; set replace_all to change every occurrence.",
        );
    }

    fn seeded(content: &str) -> (Arc<MemFs>, ToolContext) {
        let fs = Arc::new(MemFs::default());
        fs.write(Path::new("/p/f.txt"), content).unwrap();
        let c = ctx(fs.clone());
        (fs, c)
    }

    fn file(fs: &MemFs) -> String {
        fs.read(Path::new("/p/f.txt")).unwrap()
    }

    #[test]
    fn edit_replaces_a_unique_match() {
        let (fs, c) = seeded("a = 1\nb = 2\n");
        let out = EditFile.execute(json!({"path": "f.txt", "old": "b = 2", "new": "b = 3"}), &c).unwrap();
        assert_eq!((out.is_error, out.content.as_str()), (false, "replaced 1 occurrence in f.txt"));
        assert_eq!(file(&fs), "a = 1\nb = 3\n");
    }

    #[test]
    fn edit_refuses_a_missing_or_empty_or_unchanged_old_and_leaves_the_file_alone() {
        let (fs, c) = seeded("a = 1\n");
        for (args, reason) in [
            (json!({"path": "f.txt", "old": "zzz", "new": "y"}), "f.txt: `old` was not found; read the file and copy the exact text, whitespace included"),
            (json!({"path": "f.txt", "old": "", "new": "y"}), "f.txt: `old` must not be empty"),
            (json!({"path": "f.txt", "old": "a", "new": "a"}), "f.txt: `old` and `new` are the same; nothing would change"),
        ] {
            let out = EditFile.execute(args, &c).unwrap();
            assert_eq!((out.is_error, out.content.as_str()), (true, reason));
        }
        assert_eq!(file(&fs), "a = 1\n");
    }

    #[test]
    fn an_ambiguous_edit_says_how_many_matches_unless_replace_all() {
        let (fs, c) = seeded("x\nx\nx\n");
        let out = EditFile.execute(json!({"path": "f.txt", "old": "x", "new": "y"}), &c).unwrap();
        assert!(out.is_error);
        assert_eq!(out.content, "f.txt: `old` occurs 3 times; include more surrounding text to make it unique, or set replace_all to true");
        assert_eq!(file(&fs), "x\nx\nx\n", "nothing written on an ambiguous edit");
        let all = EditFile.execute(json!({"path": "f.txt", "old": "x", "new": "y", "replace_all": true}), &c).unwrap();
        assert_eq!(all.content, "replaced 3 occurrences in f.txt");
        assert_eq!(file(&fs), "y\ny\ny\n");
    }

    #[test]
    fn edit_matches_crlf_files_with_the_models_plain_newlines() {
        let (fs, c) = seeded("fn a() {\r\n    1\r\n}\r\n");
        let out = EditFile.execute(json!({"path": "f.txt", "old": "{\n    1\n}", "new": "{\n    2\n}"}), &c).unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(file(&fs), "fn a() {\r\n    2\r\n}\r\n", "line endings stay CRLF");
    }

    #[test]
    fn read_without_a_range_is_the_raw_content() {
        let (_, c) = seeded("1\n2\n3\n");
        assert_eq!(ReadFile.execute(json!({"path": "f.txt"}), &c).unwrap().content, "1\n2\n3\n");
    }

    #[test]
    fn read_pages_through_a_file_and_says_where_to_continue() {
        let body: String = (1..=10).map(|i| format!("line {i}\n")).collect();
        let (_, c) = seeded(&body);
        let first = ReadFile.execute(json!({"path": "f.txt", "limit": 3}), &c).unwrap();
        assert_eq!(first.content, "line 1\nline 2\nline 3\n[lines 1–3 of 10; continue with offset=4]");
        let middle = ReadFile.execute(json!({"path": "f.txt", "offset": 4, "limit": 3}), &c).unwrap();
        assert_eq!(middle.content, "line 4\nline 5\nline 6\n[lines 4–6 of 10; continue with offset=7]");
        let last = ReadFile.execute(json!({"path": "f.txt", "offset": 9}), &c).unwrap();
        assert_eq!(last.content, "line 9\nline 10\n[lines 9–10 of 10]");
    }

    #[test]
    fn a_range_outside_the_file_is_an_error_the_model_can_fix() {
        let (_, c) = seeded("only\n");
        for (args, reason) in [
            (json!({"path": "f.txt", "offset": 5}), "offset 5 is past the end: the file has 1 line"),
            (json!({"path": "f.txt", "offset": 0}), "offset starts at 1"),
            (json!({"path": "f.txt", "limit": 0}), "limit must be at least 1"),
        ] {
            let out = ReadFile.execute(args, &c).unwrap();
            assert_eq!((out.is_error, out.content.as_str()), (true, reason));
        }
    }

    #[test]
    fn the_manifest_declares_every_tool_the_plugin_registers() {
        let names: Vec<String> = ToolsPlugin.registrations().into_iter().map(|r| match r {
            RegistrationDecl::Tool { name } => name,
            other => panic!("unexpected registration {other:?}"),
        }).collect();
        assert_eq!(names, [READ_FILE, WRITE_FILE, EDIT_FILE]);
    }
}
