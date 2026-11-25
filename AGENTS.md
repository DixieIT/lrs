# AGENTS.md — workspace rules

Read this before changing code. This repository is a Rust workspace for a
modular agent harness: a zero-domain micro-kernel, pure port contracts,
independent packages, and one TUI frontend.

## Layout

```
crates/
  contracts/protocol/   pure serializable data shared by runtime and frontends
  contracts/sdk/        port traits only (no implementations, no policy)
  runtime/              zero-domain micro-kernel: Registry, EventBus, JobManager,
                        CommandBus, WidgetBus, DI store, Runtime/RuntimeBuilder
  core/*                foundational sdk packages (independent, no siblings)
  packages/*            optional sdk packages (gateway, runner-react, ...)
  support/*             technical crates (tool-schema): no domain, no workspace deps
  frontends/lrs/        TUI + composition roots (HttpBackend / LocalBackend)
```

## Hard rules

1. `contracts/sdk` contains **only** port traits and type aliases. No policy,
   no HTTP, no filesystem, no concrete implementation. `sdk::ports` is the
   module name: never call it `core` again.
2. `crates/runtime` contains **only** orchestration. It depends on `protocol`
   and `sdk`; it must never depend on a package or a frontend.
3. Every package in `crates/core/*` and `crates/packages/*` depends **only** on
   `protocol`, `sdk`, `support/*` and its own technical crates. Never on
   `runtime`, never on a frontend, never on a sibling package (in either
   folder). `support/*` depends on no workspace crate. `lrs` is the composition
   root: the only crate that may depend on packages. A test that needs two
   packages together belongs in `lrs`.
4. A package that exports a plugin must have a `plugin.toml` with a
   `[[registrations]]` entry for every contribution it emits. The runtime
   validates each `ctx.effect()` against the manifest of **that plugin**.
5. Registrations are RAII effects: `ctx.effect()` returns a `RegistrationHandle`
   whose drop undoes the contribution. Keep the handle in a `Disposer`.
6. Service names come from `sdk::service_keys` constants, never inline strings.
7. Model-facing text (tool schemas, prompts, renderings, diagnostics) contains
   only task-relevant concepts. No UI, transport or implementation vocabulary.
   Stable text is pinned by tests.
8. Enforcement happens in the deciding operation (the tool executor), never in
   a wrapper, a listener order trick or a schema filter.
9. No `#[allow(dead_code)]`. Zero warnings is the bar.
10. No `unused` prefixes, no `let _ =`, no placeholder implementations in
    production code. Tests may use mocks only under `#[cfg(test)]`. A dropped
    result is handled and says why (see `app::side::deliver`), not discarded.


`--backend http|local` selects one. Default is `local`; both features are on
by default. Never bypass `Backend` by calling `Client` directly from `App`.

`--executor ptc|native` picks the tool executor of the local backend; the
default is `ptc` (code mode). Tools only touch files inside the directory
`lrs` was started in; `bash` is not confined.

`~/.lele/lrs-store.toml` (secrets and config of the local backend):

## Build and verify

On Windows, TLS depends on `ring` and needs the correct 64-bit mingw-w64 on
`PATH`. Use the wrapper:

```powershell
powershell -ExecutionPolicy Bypass -File .\build.ps1 build --release
powershell -ExecutionPolicy Bypass -File .\install.ps1
```

`install.ps1` puts `lrs.exe` in `~/.local/bin`. If an old process holds the
file, it warns and copies in place; open a new shell to pick up the new binary.

Verification before handing off:

```sh
cargo check --workspace --all-targets          # default: local-runtime + legacy-http
cargo check --workspace --all-targets --no-default-features --features lrs/legacy-http
cargo test --workspace
./check-architecture.sh
./check-contract.sh
```

`contracts.inspect` is the inspectable contract. `./verify.sh inspect` prints it.
`check-contract.sh` cross-checks it against the public symbols in the code; it
cannot see signatures, so a changed port must be updated there by hand.
