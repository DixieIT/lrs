#!/bin/sh
# Enforce isolation of crates/core/* and crates/packages/*.
# C1-C4 apply to every package in both roots.
set -eu

ROOT="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
cd "$ROOT"

fail=0

# Every package name, derived from the folders so a new package is covered
# the moment it exists. Crate names use dashes, Rust paths use underscores.
packages="$(for d in crates/core/*/ crates/packages/*/; do
    [ -f "$d/Cargo.toml" ] && basename "$d"
done | sort -u)"

# Alternation of every package name except $1 (a package is not its own sibling).
siblings() {
    for p in $packages; do [ "$p" = "$1" ] || echo "$p"; done | paste -sd'|' -
}
deps_re="$(siblings '')"

for root in crates/core crates/packages; do
    [ -d "$root" ] || continue
    for dir in "$root"/*; do
        [ -d "$dir" ] || continue
        [ -f "$dir/Cargo.toml" ] || continue
        name="$(basename "$dir")"

        # C1: no dependency on the micro-kernel or on any frontend.
        if grep -qE '^\s*(runtime|lrs)\s*=' "$dir/Cargo.toml"; then
            echo "C1 ERROR: $dir depends on runtime or a frontend" >&2
            fail=1
        fi

        # C2: no sibling package dependency declared in Cargo.toml.
        sibling_deps="$(siblings "$name")"
        sibling_paths="$(echo "$sibling_deps" | tr '-' '_')"
        if [ -n "$sibling_deps" ] && grep -qE "^\s*(${sibling_deps})\s*=" "$dir/Cargo.toml"; then
            echo "C2 ERROR: $dir declares a sibling package dependency" >&2
            fail=1
        fi

        # C3: no source import of a sibling package (as a crate path: `x::`
        # at the start of a path, not a field or local module `a.x::`).
        if [ -d "$dir/src" ] && [ -n "$sibling_paths" ] \
            && grep -RInE "(^|[^A-Za-z0-9_.:])(${sibling_paths})::" "$dir/src" 2>/dev/null; then
            echo "C3 ERROR: $dir imports a sibling package" >&2
            fail=1
        fi

        # C4: bare plugins must declare their registrations in plugin.toml.
        if [ -f "$dir/src/lib.rs" ] && grep -qE 'impl FunctionPlugin for|impl .*PluginManifest for' "$dir/src/lib.rs"; then
            if [ ! -f "$dir/plugin.toml" ]; then
                echo "C4 ERROR: $dir has a plugin but no plugin.toml" >&2
                fail=1
            elif ! grep -q '\[\[registrations\]\]' "$dir/plugin.toml"; then
                echo "C4 ERROR: $dir/plugin.toml has no [[registrations]]" >&2
                fail=1
            fi
        fi
    done
done

# S1: support crates are technical only: no workspace dependency at all —
# not the contracts, the kernel, a package, a frontend or another support crate.
support="$(for d in crates/support/*/; do
    [ -f "$d/Cargo.toml" ] && basename "$d"
done | sort -u)"
workspace_re="$(for p in protocol sdk runtime lrs $packages $support; do echo "$p"; done | paste -sd'|' -)"
for dir in crates/support/*/; do
    [ -f "$dir/Cargo.toml" ] || continue
    if grep -qE "^\s*(${workspace_re})\s*=" "$dir/Cargo.toml"; then
        echo "S1 ERROR: ${dir%/} depends on a workspace crate" >&2
        fail=1
    fi
done

# The micro-kernel depends on no package.
if grep -qE "^\s*(${deps_re})\s*=" crates/runtime/Cargo.toml; then
    echo "KERNEL ERROR: runtime depends on a domain package" >&2
    fail=1
fi

# Protocol depends on nothing; sdk depends on protocol only.
if grep -qE '^\s*sdk\s*=' crates/contracts/protocol/Cargo.toml; then
    echo "EDGE ERROR: protocol depends on sdk" >&2
    fail=1
fi

if [ "$fail" -eq 0 ]; then
    echo "architecture ok"
else
    exit 1
fi
