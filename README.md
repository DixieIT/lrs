# lrs

`lrs` is the terminal frontend for **Lele**, an agent harness being rebuilt in
Rust. This repository is a Cargo workspace: the TUI is only one crate, and the
runtime it talks to lives in its own crate with no knowledge of terminals,
HTTP or storage.

```
❯ fix the failing test

  thinking
    the assertion compares wrapped output…

  ▪ read_file  src/ui/chat.rs
    pub fn wrap(lines: &[Line<'static>], width: u16) …
    … 42 more lines (^o)

  The wrapper kept the space that caused the break. Trimming on flush
  fixes both cases.

❯ ask anything…  (/ for commands)
lele-harness · opus-5 · effort high · ctx 32.5k · tokyonight
```

## Current state

| Path | Role |
|---|---|
| `crates/contracts/protocol` | Pure serializable data: `RunRequest`, `Event`, sessions, commands, widgets, plugins. |
| `crates/contracts/sdk` | The six ports (`ModelGateway`, `AgentRunner`, `Tool`, `McpProvider`, `SkillProvider`, `Capability`), sinks and typed errors. |
| `crates/runtime` | Orchestration only: registry, sessions, jobs (cancel/steer), command bus, widget bus. |
| `crates/frontends/lrs` | The TUI. Composition root: picks a runtime, registers plugins, starts the system. |
| `packages/` | 52 package slots (account, acp, api, … workspace). Each depends **only** on `protocol` and `sdk`. |

Hard rules:

- A package may never import a sibling package. If it needs file access it
  uses the `FsPort` handed to it through `ToolContext`.
- Registrations are effects: `ctx.effect()` / `ctx.on()`. `apply()` returns a
  `Disposer`. Every bare plugin is declared in its resolver manifest.
- A package is either a `Service` (default export) or a `FunctionPlugin`
  (`name` / `inject` / `Config` / `apply`). Mixing forms is rejected.
- `frontends/*` are the only composition roots.
- `core/runtime` contains no UI, network or storage code.

## Backends

Two backends exist during the migration:

- **`local` (primary)** — `runtime` runs in the same process as the TUI.
  Feature `local-runtime`.
- **`legacy-http` (temporary)** — talks to the Python `lele serve` over
  HTTP/SSE, so the TUI keeps working while the Rust runtime is being filled in.
  Enabled by default. Use `--no-spawn` to fail instead of starting a server.

The HTTP adapter is a bridge, not the architecture. It will be removed once
the Rust runtime reaches feature parity.

## Running

```console
$ cargo build --release     # binary: target/release/lrs
$ lrs
```

Flags:

| flag | |
|---|---|
| `--port N` | port of `lele serve` (legacy backend, default 8765) |
| `--no-spawn` | fail instead of starting a server |
| `-r`, `--resume` | reopen this project's last session |
| `--debug` | trace the loop to `~/.lele/lrs.log` |

Themes live in `~/.lele/themes/<name>.toml`; switch with `/theme`.

Settings live in `~/.lele/lrs.toml`; `[status]` with `context = false` hides the
context window gauge under the prompt.

### Your own commands

A file `~/.lele/lrs/commands/<name>.toml` defines `/<name>`:

```toml
description = "Start or stop the local Inkling server"
run = "~/models/inkling/inkling.sh"
to_model = false            # optional; true tells the model what it printed

[sub]                       # optional; the arguments the menu offers
start = "load the model and serve it"
stop = "stop the server"
```

`/inkling stop` runs `run` with `stop` appended, in the project directory, like a
`!` command: `esc` stops it and its output is in the transcript. Files are read at
startup and by `/reload`. A built-in or plugin command with the same name wins.

## Installing

```console
$ ./install.sh                 # Linux, macOS
> .\install.ps1                # Windows
```

Both build the workspace and install the `lrs` binary into `~/.local/bin`.
They replace a running executable safely: on Linux the new file is moved into
place; on Windows the old one is renamed aside.

`build.ps1` is a thin cargo wrapper that puts a 64-bit mingw-w64 ahead of
anything stale on `PATH`, because rustc resolves both its linker and `dlltool`
that way. If your `PATH` is clean, plain `cargo` works.

## Keys

| | |
|---|---|
| `enter` | send, or take the highlighted completion |
| `tab` | take the highlighted completion |
| `shift+enter`, `^j` | newline |
| `↑`/`↓` | prompt history (the completion menu, while it's open) |
| `^o` | expand/collapse all tool output |
| `!` | on an empty prompt: a command to run here; its output goes to the model |
| `^c` | stop the turn or command, then clear the input, then quit |
| `esc` | stop the turn or command, or clear the input |
| `^p` | attach the clipboard image (`^v` too, where the terminal passes it on) |
| `pgup`/`pgdn`, `^↑`/`^↓` | scroll |
| `^l` | jump back to the newest line |
| `^w` / `^u` | delete word / clear input |
| drag | select and copy; `shift`+drag for the terminal's own selection |

Completion: `/` opens the command menu, `@` opens project files. Images can be
dropped onto the window, pasted, or attached with `/image` / `^p`.

Commands: `/help` `/theme` `/model` `/connect` `/effort` `/image` `/plugin`
`/extension` `/reload` `/resume` `/clear` `/expand` `/quit`. Plugin commands are
registered by components and dispatched through `CommandBus`.

## Credits

This is a port of a Python-based harness, built from scratch and designed by
Gabriele Masiero.
