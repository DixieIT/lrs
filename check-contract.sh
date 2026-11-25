#!/bin/sh
# Cross-check contracts.inspect against the actual public symbols.
# The inspect file is the spec; the compiler is the witness.
set -eu

ROOT="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
cd "$ROOT"
SPEC=contracts.inspect

fail=0

# 1. Every `trait X` in the spec must exist as `pub trait X` in the source.
grep -oE '^trait [A-Za-z_][A-Za-z0-9_]*' "$SPEC" | awk '{print $2}' | sort -u | while read -r t; do
    if ! grep -RqE "^pub trait ${t}([[:space:]]|:|<)" \
        crates/contracts/*/src crates/core/*/src crates/runtime/src; then
        echo "MISSING trait $t (declared in $SPEC)" >&2
        exit 1
    fi
done || fail=1

# 2. Every `struct X` in the spec must exist as `pub struct X`.
grep -oE '^struct [A-Za-z_][A-Za-z0-9_]*' "$SPEC" | awk '{print $2}' | sort -u | while read -r s; do
    if ! grep -RqE "^pub struct ${s}([[:space:]]|<|\()" \
        crates/contracts/*/src crates/core/*/src crates/runtime/src; then
        echo "MISSING struct $s (declared in $SPEC)" >&2
        exit 1
    fi
done || fail=1

# 3. Every `enum X` in the spec must exist as `pub enum X`.
grep -oE '^enum [A-Za-z_][A-Za-z0-9_]*' "$SPEC" | awk '{print $2}' | sort -u | while read -r e; do
    if ! grep -RqE "^pub enum ${e}([[:space:]]|<)" \
        crates/contracts/*/src crates/core/*/src crates/runtime/src; then
        echo "MISSING enum $e (declared in $SPEC)" >&2
        exit 1
    fi
done || fail=1

# 4. No public trait in SDK/core is absent from the spec.
for src in crates/contracts/sdk/src/*.rs crates/runtime/src/*.rs; do
    grep -oE '^pub trait [A-Za-z_][A-Za-z0-9_]*' "$src" | awk '{print $3}' | sort -u | while read -r t; do
        if ! grep -qE "^trait ${t}([[:space:]]|:|<)" "$SPEC"; then
            echo "UNDOCUMENTED trait $t in $src" >&2
            exit 1
        fi
    done || fail=1
done

if [ "$fail" -eq 0 ]; then
    echo "contract ok"
else
    exit 1
fi
