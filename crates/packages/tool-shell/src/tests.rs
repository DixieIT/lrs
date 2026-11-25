use super::*;
use sdk::FsPort;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

struct NoFs;
impl FsPort for NoFs {}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tool-shell-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn context_in(dir: &Path, cancelled: Arc<AtomicBool>) -> ToolContext {
    ToolContext {
        session_id: "s".into(),
        cwd: dir.display().to_string(),
        cancellation_token: Arc::new(move || cancelled.load(Ordering::SeqCst)),
        fs: Arc::new(NoFs),
    }
}

/// The shell these tests really run, and a command spelled for it.
fn local() -> (Bash, bool) {
    let shell = Shell::detect();
    let powershell = shell == Shell::PowerShell;
    (Bash::new(shell), powershell)
}

fn pick<'a>(powershell: bool, posix: &'a str, ps: &'a str) -> &'a str {
    if powershell { ps } else { posix }
}

fn run_here(command: &str, dir: &Path) -> ToolOutput {
    let (bash, _) = local();
    bash.execute(json!({ "command": command }), &context_in(dir, Arc::new(AtomicBool::new(false)))).unwrap()
}

// Model-facing text --------------------------------------------------------

#[test]
fn the_description_names_the_shell_and_is_pinned() {
    let git = Bash::new(Shell::GitBash { exe: PathBuf::from(r"C:\Git\usr\bin\bash.exe") });
    assert_eq!(
        git.description(),
        "Run a shell command in Git Bash, starting in the project directory, and return its combined \
         output. Commands use bash syntax; Unix tools such as ls, grep, find, sed and sort are \
         available. The command is stopped after timeout_ms milliseconds (default 120000, at most \
         600000); a non-zero exit status is reported as a failure with its code. Prefer read_file, \
         write_file and edit_file for working with files.",
    );
    assert!(Bash::new(Shell::PowerShell).description()
        .contains("in PowerShell, starting in the project directory"));
    assert!(Bash::new(Shell::PowerShell).description()
        .contains("Commands use PowerShell syntax and cmdlets, not bash."));
    assert!(Bash::new(Shell::Sh).description().contains("Commands use POSIX sh syntax."));
}

// Detection -----------------------------------------------------------------

fn fake_fs<'a>(files: &'a [&'a str]) -> impl Fn(&Path) -> bool + 'a {
    move |p: &Path| files.iter().any(|f| Path::new(f) == p)
}

#[test]
fn git_bash_is_found_from_either_git_location() {
    let bash = r"C:\Git\usr\bin\bash.exe";
    for git_dir in [r"C:\Git\cmd", r"C:\Git\mingw64\bin"] {
        let git = format!(r"{git_dir}\git.exe");
        let path = std::env::join_paths([r"C:\Windows\System32", git_dir]).unwrap();
        assert_eq!(
            Shell::detect_windows(&path, fake_fs(&[&git, bash])),
            Shell::GitBash { exe: PathBuf::from(bash) },
            "from {git_dir}",
        );
    }
}

#[test]
fn without_git_the_shell_is_powershell_never_the_wsl_bash() {
    let path = std::env::join_paths([r"C:\Windows\System32"]).unwrap();
    let only_wsl = fake_fs(&[r"C:\Windows\System32\bash.exe"]);
    assert_eq!(Shell::detect_windows(&path, only_wsl), Shell::PowerShell);
}

// Output hygiene --------------------------------------------------------------

#[test]
fn escape_sequences_are_removed_and_text_kept() {
    assert_eq!(strip_ansi("\u{1b}[1;31mred\u{1b}[0m plain"), "red plain");
    assert_eq!(strip_ansi("\u{1b}]0;title\u{7}after"), "after");
    assert_eq!(strip_ansi("\u{1b}]8;;http://x\u{1b}\\link\u{1b}]8;;\u{1b}\\"), "link");
    assert_eq!(strip_ansi("però ✓"), "però ✓");
}

// (a) execution, merged streams, colours ------------------------------------------

#[test]
fn stdout_and_stderr_come_back_together_without_colour_codes() {
    let (_, ps) = local();
    let dir = scratch("merged");
    let out = run_here(pick(
        ps,
        r"echo out; echo err 1>&2; printf '\033[32mgreen\033[0m\n'",
        "Write-Output out; [Console]::Error.WriteLine('err'); Write-Output \"$([char]27)[32mgreen$([char]27)[0m\"",
    ), &dir);
    assert!(!out.is_error, "{}", out.content);
    for word in ["out", "err", "green"] {
        assert!(out.content.lines().any(|l| l == word), "{word} missing in {:?}", out.content);
    }
    assert!(!out.content.contains('\u{1b}'));
}

#[test]
fn the_command_starts_in_the_project_directory_and_silence_is_said() {
    let (_, ps) = local();
    let dir = scratch("cwd");
    std::fs::write(dir.join("marker-here.txt"), "x").unwrap();
    let listed = run_here(pick(ps, "ls", "Get-ChildItem -Name"), &dir);
    assert!(listed.content.contains("marker-here.txt"), "{}", listed.content);
    let silent = run_here(pick(ps, "true", "$null = 1"), &dir);
    assert_eq!((silent.is_error, silent.content.as_str()), (false, "(no output)"));
}

// (b) exit codes and timeout -------------------------------------------------

#[test]
fn a_non_zero_exit_is_a_failure_that_names_the_code() {
    let (_, ps) = local();
    let out = run_here(pick(ps, "echo before; exit 3", "Write-Output before; exit 3"), &scratch("exit"));
    assert!(out.is_error);
    assert_eq!(out.content, "before\n[exit code 3]");
}

#[test]
fn a_command_past_its_timeout_is_stopped_promptly() {
    let (bash, ps) = local();
    let started = Instant::now();
    let out = bash.execute(
        json!({ "command": pick(ps, "sleep 30", "Start-Sleep 30"), "timeout_ms": 400 }),
        &context_in(&scratch("timeout"), Arc::new(AtomicBool::new(false))),
    ).unwrap();
    assert!(out.is_error);
    assert!(out.content.ends_with("[stopped: timed out after 400 ms]"), "{}", out.content);
    assert!(started.elapsed() < Duration::from_secs(10), "took {:?}", started.elapsed());
}

#[test]
fn a_missing_command_is_a_tool_error() {
    let (bash, _) = local();
    let out = bash.execute(json!({ "command": "  " }), &context_in(&scratch("empty"), Arc::new(AtomicBool::new(false)))).unwrap();
    assert_eq!((out.is_error, out.content.as_str()), (true, "missing string argument `command`"));
}

// (c) cancellation kills the whole tree ------------------------------------------

#[test]
fn cancelling_stops_the_command_and_the_processes_it_started() {
    let (bash, ps) = local();
    let dir = scratch("tree");
    let marker = dir.join("late.txt");
    if marker.exists() {
        std::fs::remove_file(&marker).unwrap();
    }
    // A child that would write a file two seconds from now, behind a long sleep.
    let command = pick(
        ps,
        "(sleep 2; echo late > late.txt) & sleep 30",
        "Start-Process powershell -NoNewWindow -ArgumentList '-NoProfile','-Command','Start-Sleep 2; Set-Content late.txt late'; Start-Sleep 30",
    );
    let flag = Arc::new(AtomicBool::new(false));
    let trigger = flag.clone();
    let canceller = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(400));
        trigger.store(true, Ordering::SeqCst);
    });
    let started = Instant::now();
    let result = bash.execute(json!({ "command": command }), &context_in(&dir, flag));
    canceller.join().unwrap();
    assert_eq!(result.unwrap_err(), Error::Cancelled);
    assert!(started.elapsed() < Duration::from_secs(5), "took {:?}", started.elapsed());
    // Past the child's two seconds: had it survived, the file would be there.
    std::thread::sleep(Duration::from_secs(3));
    assert!(!marker.exists(), "a process started by the command outlived the cancel");
}

/// The fallback is exercised even on a machine that has Git Bash.
#[cfg(windows)]
#[test]
fn the_powershell_fallback_runs_commands_in_utf8_and_reports_exit_codes() {
    let bash = Bash::new(Shell::PowerShell);
    let ctx = context_in(&scratch("powershell"), Arc::new(AtomicBool::new(false)));
    let out = bash.execute(json!({ "command": "Write-Output 'però ✓'; exit 2" }), &ctx).unwrap();
    assert!(out.is_error);
    assert_eq!(out.content, "però ✓\n[exit code 2]");
}

// Registration ------------------------------------------------------------------

#[test]
fn the_plugin_registers_the_bash_tool() {
    let plugin = ToolShellPlugin::new(Shell::Sh);
    assert!(matches!(
        plugin.registrations().as_slice(),
        [RegistrationDecl::Tool { name }] if name == BASH
    ));
}
