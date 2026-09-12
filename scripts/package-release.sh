#!/usr/bin/env bash
# Assemble immutable native release assets from already-built target binaries.
set -euo pipefail

pika_script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
pika_repo_dir=$(dirname "$pika_script_dir")

fail() { printf 'Pika release: %s\n' "$*" >&2; exit 1; }
usage() {
    printf '%s\n' \
        'Usage: package-release.sh VERSION OUTPUT TARGET=BINARY [TARGET=BINARY ...]' \
        '' \
        'Supported targets: macOS/Linux arm64 and x86_64; Windows x86_64 client.'
}

[ "$#" -ge 3 ] || { usage >&2; exit 2; }
pika_version=${1#v}
pika_output=$2
shift 2
[[ "$pika_version" =~ ^[0-9]+\.[0-9]+\.[0-9]+((a|b|rc)[0-9]+|-(alpha|beta|rc)\.[0-9]+)?$ ]] || \
    fail 'Invalid release version.'
[ ! -e "$pika_output" ] || fail 'Output path already exists; release bytes were not replaced.'

for pika_command in tar gzip mktemp find sort cut wc; do
    command -v "$pika_command" >/dev/null 2>&1 || fail "Required command missing: $pika_command."
done
if command -v sha256sum >/dev/null 2>&1; then
    digest() { sha256sum "$1" | cut -d ' ' -f 1; }
elif command -v shasum >/dev/null 2>&1; then
    digest() { shasum -a 256 "$1" | cut -d ' ' -f 1; }
else
    fail 'A SHA256 verifier is required (sha256sum or shasum).'
fi

pika_parent=$(dirname "$pika_output")
mkdir -p "$pika_parent"
# Archive commands may change directory while assembling a target. Resolve the
# stage and final output once so relative CI output paths cannot be reinterpreted
# from inside a payload directory (notably the Windows zip branch).
pika_parent=$(cd "$pika_parent" && pwd -P)
pika_output="$pika_parent/$(basename "$pika_output")"
pika_stage=$(mktemp -d "$pika_parent/.pika-release.XXXXXXXX")
cleanup() {
    local pika_status=$?
    trap - EXIT
    [ -z "${pika_stage:-}" ] || rm -rf -- "$pika_stage"
    exit "$pika_status"
}
trap cleanup EXIT

pika_rows=''
pika_count=0
for pika_pair in "$@"; do
    pika_target=${pika_pair%%=*}
    pika_binary=${pika_pair#*=}
    [ "$pika_target" != "$pika_pair" ] && [ -n "$pika_binary" ] || fail "Expected TARGET=BINARY, got: $pika_pair"
    case "$pika_target" in
        aarch64-apple-darwin|x86_64-apple-darwin|aarch64-unknown-linux-musl|x86_64-unknown-linux-musl|x86_64-pc-windows-msvc) ;;
        *) fail "Unsupported release target: $pika_target" ;;
    esac
    [ -f "$pika_binary" ] && [ ! -L "$pika_binary" ] || \
        fail "Binary is not a regular file: $pika_binary"
    if [ "$pika_target" != x86_64-pc-windows-msvc ]; then
        [ -x "$pika_binary" ] || fail "Binary is not executable: $pika_binary"
    fi
    [ "$(wc -c < "$pika_binary" | tr -d ' ')" -le 52428800 ] || fail "Binary exceeds the 50 MiB active-footprint budget: $pika_target"
    if [ "${PIKA_CROSS_PACKAGE:-0}" != 1 ]; then
        [ "$($pika_binary --version)" = "pika $pika_version" ] || fail "Binary version does not match $pika_version: $pika_target"
        "$pika_binary" --help >/dev/null
        "$pika_binary" skill show >/dev/null
    fi

    pika_payload="$pika_stage/payload-$pika_target"
    mkdir "$pika_payload"
    if [ "$pika_target" = x86_64-pc-windows-msvc ]; then
        command -v zip >/dev/null 2>&1 || fail 'Required command missing: zip.'
        cp "$pika_binary" "$pika_payload/pika.exe"
        pika_archive="pikamux-$pika_version-$pika_target.zip"
        (cd "$pika_payload" && zip -X -q "$pika_stage/$pika_archive" pika.exe)
    else
        cp "$pika_binary" "$pika_payload/pika"
        chmod 755 "$pika_payload/pika"
        pika_archive="pikamux-$pika_version-$pika_target.tar.gz"
        COPYFILE_DISABLE=1 LC_ALL=C tar -czf "$pika_stage/$pika_archive" -C "$pika_payload" pika
    fi
    rm -rf -- "$pika_payload"
    pika_sha=$(digest "$pika_stage/$pika_archive")
    pika_bytes=$(wc -c < "$pika_stage/$pika_archive" | tr -d ' ')
    [ "$pika_bytes" -le 20971520 ] || fail "Artifact exceeds the 20 MiB compressed budget: $pika_target"
    printf '%s\n' "$pika_sha" > "$pika_stage/$pika_archive.sha256"
    [ "$pika_count" -eq 0 ] || pika_rows="$pika_rows,"
    pika_rows="$pika_rows
    \"$pika_target\": {\"file\": \"$pika_archive\", \"sha256\": \"$pika_sha\", \"bytes\": $pika_bytes}"
    pika_count=$((pika_count + 1))
done

[ "$pika_count" -gt 0 ] || fail 'No target binaries supplied.'
case "$pika_version" in
    *a*|*b*|*rc*|*-alpha.*|*-beta.*|*-rc.*) pika_channel=preview ;;
    *) pika_channel=stable ;;
esac
{
    printf '{\n  "schema": 2,\n  "package": "pikamux",\n'
    printf '  "version": "%s",\n  "channel": "%s",\n  "artifacts": {%s\n  }\n}\n' \
        "$pika_version" "$pika_channel" "$pika_rows"
} > "$pika_stage/pika-native-release.json"
printf '%s\n' "$pika_version" > "$pika_stage/pika-version"
cp "$pika_script_dir/install.sh" "$pika_stage/install.sh"
cp "$pika_repo_dir/LICENSE" "$pika_stage/LICENSE"
cp "$pika_repo_dir/THIRD_PARTY.md" "$pika_stage/THIRD_PARTY.md"
chmod 755 "$pika_stage/install.sh"
(
    cd "$pika_stage"
    find . -maxdepth 1 -type f ! -name SHA256SUMS -exec basename {} \; | LC_ALL=C sort |
        while IFS= read -r pika_file; do printf '%s  %s\n' "$(digest "$pika_file")" "$pika_file"; done > SHA256SUMS
)
mv "$pika_stage" "$pika_output"
pika_stage=''
printf 'Pika %s release bundle: %s\n' "$pika_version" "$pika_output"
