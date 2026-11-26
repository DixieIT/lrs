//! Commands the user defines as files, one per command, in
//! `~/.lele/lrs/commands/<name>.toml`:
//!
//! ```toml
//! description = "Start or stop the local Inkling server"
//! run = "~/models/inkling/inkling.sh"
//! to_model = false          # optional: tell the model what it printed
//!
//! [sub]                     # optional: arguments the completion menu offers
//! start = "load the model and serve it"
//! stop = "stop the server"
//! ```
//!
//! `/inkling stop` runs `run` with whatever follows the name appended, in the
//! project directory, like a `!` command: stoppable with esc, its output in the
//! transcript. A built-in or a plugin command with the same name wins.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserCommand {
    /// With the slash, as it is typed: `/inkling`.
    pub name: String,
    pub desc: String,
    pub run: String,
    /// Send the output to the model as a turn, as `!` does. Off by default: most
    /// of these start or stop something, and the model has nothing to say to that.
    pub to_model: bool,
    pub sub: BTreeMap<String, String>,
}

impl UserCommand {
    /// What to hand to the shell for `/name <arg>`.
    pub fn command_line(&self, arg: &str) -> String {
        match arg.trim() {
            "" => self.run.clone(),
            arg => format!("{} {arg}", self.run),
        }
    }
}

/// The file as written. Keys it does not know are ignored, so a newer lrs's file
/// still loads in an older one.
#[derive(Deserialize)]
struct File {
    #[serde(default)]
    description: String,
    run: String,
    #[serde(default)]
    to_model: bool,
    #[serde(default)]
    sub: BTreeMap<String, String>,
}

pub fn dir() -> Option<PathBuf> {
    super::config::dir().map(|d| d.join("lrs").join("commands"))
}

/// Every `*.toml` in `dir`, by file name. A file that cannot be used is reported
/// and skipped: one typo must not take the other commands with it. A directory
/// that is not there is not an error, it is just no commands.
pub fn load(dir: &Path) -> (Vec<UserCommand>, Vec<String>) {
    let mut found = Vec::new();
    let mut errors = Vec::new();
    let read = match std::fs::read_dir(dir) {
        Ok(read) => read,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (found, errors),
        Err(e) => return (found, vec![format!("commands: cannot read {}: {e}", dir.display())]),
    };
    let mut paths: Vec<PathBuf> = read
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "toml"))
        .collect();
    paths.sort();
    for path in paths {
        let file = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        match parse(&path) {
            Ok(command) => found.push(command),
            Err(why) => errors.push(format!("commands/{file}: {why}")),
        }
    }
    (found, errors)
}

fn parse(path: &Path) -> Result<UserCommand, String> {
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let valid = !stem.is_empty()
        && stem.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    if !valid {
        return Err("the file name is the command name: lowercase letters, digits, - and _".into());
    }
    let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read it: {e}"))?;
    let file: File = toml::from_str(&text).map_err(|e| e.message().to_string())?;
    if file.run.trim().is_empty() {
        return Err("`run` is empty".into());
    }
    Ok(UserCommand {
        name: format!("/{stem}"),
        desc: file.description,
        run: file.run.trim().to_string(),
        to_model: file.to_model,
        sub: file.sub,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir_with(name: &str, files: &[(&str, &str)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lrs-usercmd-{}-{name}", std::process::id()));
        if dir.exists() {
            std::fs::remove_dir_all(&dir).unwrap();
        }
        std::fs::create_dir_all(&dir).unwrap();
        for (file, text) in files {
            std::fs::write(dir.join(file), text).unwrap();
        }
        dir
    }

    #[test]
    fn a_file_becomes_a_command_named_after_it() {
        let dir = dir_with("basic", &[(
            "inkling.toml",
            "description = \"Local model\"\nrun = \"~/models/inkling/inkling.sh\"\n[sub]\nstart = \"serve it\"\n",
        )]);
        let (commands, errors) = load(&dir);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(commands.len(), 1);
        let c = &commands[0];
        assert_eq!(c.name, "/inkling");
        assert_eq!(c.desc, "Local model");
        assert_eq!(c.run, "~/models/inkling/inkling.sh");
        assert!(!c.to_model, "the model hears about a command only when asked to");
        assert_eq!(c.sub.get("start").map(String::as_str), Some("serve it"));
    }

    #[test]
    fn what_follows_the_name_is_appended_to_the_command() {
        let dir = dir_with("line", &[("x.toml", "run = \"echo hi\"")]);
        let c = &load(&dir).0[0];
        assert_eq!(c.command_line(""), "echo hi");
        assert_eq!(c.command_line("  there  "), "echo hi there");
    }

    #[test]
    fn a_bad_file_is_reported_and_the_others_still_load() {
        let dir = dir_with("bad", &[
            ("good.toml", "run = \"true\""),
            ("broken.toml", "run = "),
            ("empty.toml", "run = \"  \""),
            ("Bad Name.toml", "run = \"true\""),
            ("norun.toml", "description = \"no command\""),
            ("notes.txt", "not a command"),
        ]);
        let (commands, errors) = load(&dir);
        let names: Vec<&str> = commands.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["/good"]);
        assert_eq!(errors.len(), 4, "{errors:?}");
        assert!(errors.iter().any(|e| e.starts_with("commands/empty.toml")));
        assert!(errors.iter().any(|e| e.starts_with("commands/Bad Name.toml")));
    }

    #[test]
    fn no_directory_is_no_commands_and_no_complaint() {
        let (commands, errors) = load(Path::new("/definitely/not/here"));
        assert!(commands.is_empty() && errors.is_empty());
    }
}
