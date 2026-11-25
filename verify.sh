#!/bin/sh
# One entry point for the inspectable contract.
#   ./verify.sh inspect   -> print contracts.inspect
#   ./verify.sh           -> architecture + contract + tests
set -eu

ROOT="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
cd "$ROOT"

if [ "${1:-}" = "inspect" ]; then
    exec cat contracts.inspect
fi

./check-architecture.sh
./check-contract.sh
cargo test --workspace
echo "verify ok"
