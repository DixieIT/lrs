//! Which shell runs the commands, chosen once when the plugin starts.
//!
//! On Windows, Git Bash when Git is installed (the tool is called `bash` and
//! the model writes Unix commands), else Windows PowerShell. Elsewhere `sh`.
//! Git Bash is found next to the `git` on `PATH`, never as a bare `bash.exe`:
//! System32 has one that starts WSL instead.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Shell {
    /// `bash.exe` from a Git for Windows install.
    GitBash { exe: PathBuf },
    /// Windows PowerShell, when Git Bash is missing.
    PowerShell,
    /// `/bin/sh` on Unix.
    Sh,
}

/// Makes PowerShell write UTF-8, like the other shells, instead of the
/// console code page.
const POWERSHELL_UTF8: &str =
    "$OutputEncoding = [System.Text.Encoding]::UTF8; [Console]::OutputEncoding = [System.Text.Encoding]::UTF8; ";

impl Shell {
    /// The shell for this machine.
    pub fn detect() -> Self {
        if cfg!(windows) {
            let path = std::env::var_os("PATH").unwrap_or_default();
            Self::detect_windows(&path, |p| p.is_file())
        } else {
            Shell::Sh
        }
    }

    /// Git Bash next to the first `git.exe` on `path`, else PowerShell.
    /// `exists` is the filesystem check, injected so tests need no Git.
    pub fn detect_windows(path: &OsString, exists: impl Fn(&Path) -> bool) -> Self {
        for dir in std::env::split_paths(path) {
            let git = dir.join("git.exe");
            if !exists(&git) {
                continue;
            }
            // `Git\cmd\git.exe` and `Git\mingw64\bin\git.exe` both sit below
            // the install root, which holds `usr\bin\bash.exe`.
            for root in git.ancestors().skip(1).take(3) {
                let bash = root.join("usr").join("bin").join("bash.exe");
                if exists(&bash) {
                    return Shell::GitBash { exe: bash };
                }
            }
        }
        Shell::PowerShell
    }

    /// The name the model is told.
    pub fn label(&self) -> &'static str {
        match self {
            Shell::GitBash { .. } => "Git Bash",
            Shell::PowerShell => "PowerShell",
            Shell::Sh => "sh",
        }
    }

    /// A process that runs `command` in this shell.
    pub fn command(&self, command: &str) -> Command {
        match self {
            Shell::GitBash { exe } => {
                let mut cmd = Command::new(exe);
                cmd.arg("-c").arg(command);
                // `bash -c` skips the login profile that puts Git's coreutils on
                // PATH; without them System32's `find.exe` and `sort.exe` win.
                if let Some(usr_bin) = exe.parent() {
                    let mut path = OsString::from(usr_bin);
                    path.push(";");
                    path.push(std::env::var_os("PATH").unwrap_or_default());
                    cmd.env("PATH", path);
                }
                utf8_env(&mut cmd);
                cmd
            }
            Shell::PowerShell => {
                let mut cmd = Command::new("powershell.exe");
                cmd.args(["-NoProfile", "-NonInteractive", "-Command"])
                    .arg(format!("{POWERSHELL_UTF8}{command}"));
                utf8_env(&mut cmd);
                cmd
            }
            Shell::Sh => {
                let mut cmd = Command::new("/bin/sh");
                cmd.arg("-c").arg(command);
                cmd
            }
        }
    }
}

/// Python scripts run from the shell print UTF-8 instead of the code page.
fn utf8_env(cmd: &mut Command) {
    cmd.env("PYTHONIOENCODING", "utf-8").env("PYTHONUTF8", "1");
}
