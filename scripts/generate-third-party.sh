#!/usr/bin/env bash
# Rebuild the exact attribution bundle shipped beside every native release.
set -euo pipefail

pika_script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
pika_repo_dir=$(CDPATH= cd -- "$pika_script_dir/.." && pwd -P)
pika_check=0
case "$#" in
    0) pika_output="$pika_repo_dir/THIRD_PARTY.md" ;;
    1)
        if [ "$1" = "--check" ]; then
            pika_check=1
            pika_output="$pika_repo_dir/THIRD_PARTY.md"
        else
            pika_output=$1
        fi
        ;;
    *) printf 'usage: generate-third-party.sh [--check|OUTPUT]\n' >&2; exit 2 ;;
esac

for pika_command in cargo cmp python3 rustc sed sort mktemp; do
    command -v "$pika_command" >/dev/null 2>&1 || {
        printf '%s is required\n' "$pika_command" >&2
        exit 2
    }
done

pika_rust_version=$(rustc --version)
case "$pika_rust_version" in
    'rustc 1.88.0 '*) ;;
    *) printf 'rustc 1.88.0 is required to regenerate release notices\n' >&2; exit 2 ;;
esac
pika_rust_copyright="$(rustc --print sysroot)/share/doc/rust/COPYRIGHT-library.html"
[ -f "$pika_rust_copyright" ] && [ ! -L "$pika_rust_copyright" ] || {
    printf 'Rust standard-library copyright report is missing\n' >&2
    exit 2
}
pika_rust_full_copyright="$(rustc --print sysroot)/share/doc/rust/COPYRIGHT.html"
[ -f "$pika_rust_full_copyright" ] && [ ! -L "$pika_rust_full_copyright" ] || {
    printf 'Rust toolchain copyright report is missing\n' >&2
    exit 2
}

pika_tmp=$(mktemp -d "${TMPDIR:-/tmp}/pika-third-party.XXXXXXXX")
cleanup() {
    local pika_status=$?
    trap - EXIT
    rm -rf -- "$pika_tmp"
    exit "$pika_status"
}
trap cleanup EXIT

(cd "$pika_repo_dir" && cargo metadata --locked --offline --format-version 1) \
    > "$pika_tmp/metadata.json"
: > "$pika_tmp/tree.tsv"
for pika_target in \
    aarch64-apple-darwin \
    x86_64-apple-darwin \
    aarch64-unknown-linux-musl \
    x86_64-unknown-linux-musl \
    x86_64-pc-windows-msvc
do
    (cd "$pika_repo_dir" && \
        cargo tree --color never --locked --offline --target "$pika_target" \
            --edges normal,build --prefix none --format $'{p}\t{l}') |
        sed -e '1d' -e 's/ (\*)$//' -e 's/ (proc-macro)//' \
            >> "$pika_tmp/tree.tsv"
done
LC_ALL=C sort -u "$pika_tmp/tree.tsv" > "$pika_tmp/tree-sorted.tsv"

python3 "$pika_script_dir/generate-third-party.py" \
    --metadata "$pika_tmp/metadata.json" \
    --tree "$pika_tmp/tree-sorted.tsv" \
    --lock "$pika_repo_dir/Cargo.lock" \
    --rust-copyright "$pika_rust_copyright" \
    --rust-full-copyright "$pika_rust_full_copyright" \
    --output "$pika_tmp/THIRD_PARTY.md"

if [ "$pika_check" -eq 1 ]; then
    cmp -- "$pika_output" "$pika_tmp/THIRD_PARTY.md"
else
    mkdir -p -- "$(dirname -- "$pika_output")"
    mv -- "$pika_tmp/THIRD_PARTY.md" "$pika_output"
fi
