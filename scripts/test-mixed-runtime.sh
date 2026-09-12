#!/usr/bin/env bash
# Run the frozen Python v0.5.0a4 ↔ native Rust compatibility laboratory.
set -euo pipefail

pika_script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
pika_repo_dir=$(dirname "$pika_script_dir")
cd "$pika_repo_dir"

pika_cargo=${CARGO:-cargo}
command -v "$pika_cargo" >/dev/null 2>&1 || {
    printf 'cargo is required\n' >&2
    exit 2
}
command -v python3 >/dev/null 2>&1 || {
    printf 'python3 is required for the frozen compatibility reference\n' >&2
    exit 2
}

printf '%s\n' \
    'Pika mixed-runtime compatibility laboratory' \
    '  frozen peer: Python v0.5.0a4' \
    '  state: disposable SQLite only' \
    '  transport: in-process/fake only; no SSH or network'
"$pika_cargo" test --locked --test mixed_runtime_contract "$@"
