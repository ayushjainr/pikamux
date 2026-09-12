#!/usr/bin/env bash
# Pika's native Mac/Linux bootstrap. It verifies fixed release bytes before
# invoking the staged binary; the binary owns managed-root validation.
set -euo pipefail

PIKA_RELEASE_ROOT='https://github.com/ayushjainr/pikamux/releases'

fail() { printf 'Pika installer: %s\n' "$*" >&2; exit 1; }
usage() {
    printf '%s\n' \
        'Usage: bash install.sh [--version TAG | --bundle DIRECTORY] [--no-setup]' \
        '                       [--root DIRECTORY] [--bin-dir DIRECTORY]' \
        '' \
        'Installs native Pika for your user on macOS/Linux. No sudo.'
}

pika_version=''
pika_bundle=''
pika_no_setup=''
pika_root="${HOME}/.local/share/pikamux"
pika_bin_dir="${HOME}/.local/bin"
while [ "$#" -gt 0 ]; do
    case "$1" in
        --help|-h) usage; exit 0 ;;
        --version|--bundle|--root|--bin-dir)
            [ "$#" -ge 2 ] && [ -n "$2" ] || fail "Missing value for $1. Run bash install.sh --help."
            case "$1" in
                --version) pika_version=${2#v} ;;
                --bundle) pika_bundle=$2 ;;
                --root) pika_root=$2 ;;
                --bin-dir) pika_bin_dir=$2 ;;
            esac
            shift 2 ;;
        --no-setup) pika_no_setup=1; shift ;;
        *) fail "Unknown option: $1. Run bash install.sh --help." ;;
    esac
done
[ -z "$pika_version" ] || [ -z "$pika_bundle" ] || fail 'Choose --version or --bundle, not both.'

if [ -n "$pika_bundle" ]; then
    [ -d "$pika_bundle" ] && [ ! -L "$pika_bundle" ] || fail 'The bundle must be a real directory.'
    pika_bundle=$(cd "$pika_bundle" && pwd -P)
    [ -f "$pika_bundle/pika-version" ] && [ ! -L "$pika_bundle/pika-version" ] || fail 'The native bundle has no regular pika-version file.'
    pika_version=$(sed -n '1p' "$pika_bundle/pika-version")
    [ "$(wc -l < "$pika_bundle/pika-version" | tr -d ' ')" = 1 ] || fail 'Invalid pika-version file.'
fi

case "$(uname -s):$(uname -m)" in
    Darwin:arm64|Darwin:aarch64) pika_target='aarch64-apple-darwin' ;;
    Darwin:x86_64) pika_target='x86_64-apple-darwin' ;;
    Linux:aarch64|Linux:arm64) pika_target='aarch64-unknown-linux-musl' ;;
    Linux:x86_64) pika_target='x86_64-unknown-linux-musl' ;;
    *) fail 'Native Pika supports macOS/Linux on arm64 or x86_64.' ;;
esac

for pika_command in curl tar mktemp sed tr wc find cut; do
    command -v "$pika_command" >/dev/null 2>&1 || fail "Required command missing: $pika_command."
done
if command -v sha256sum >/dev/null 2>&1; then
    digest() { sha256sum "$1" | cut -d ' ' -f 1; }
elif command -v shasum >/dev/null 2>&1; then
    digest() { shasum -a 256 "$1" | cut -d ' ' -f 1; }
else
    fail 'A SHA256 verifier is required (sha256sum or shasum).'
fi

pika_tmp=$(mktemp -d "${TMPDIR:-/tmp}/pika-install.XXXXXXXX")
cleanup() {
    local pika_status=$?
    trap - EXIT
    [ -z "${pika_tmp:-}" ] || rm -rf -- "$pika_tmp"
    exit "$pika_status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

download() {
    curl --disable --fail --silent --show-error --location \
        --proto '=https' --proto-redir '=https' --connect-timeout 20 \
        --max-time 300 --retry 2 --max-filesize "$3" --output "$2" "$1" || \
        fail 'Download failed. Nothing activated.'
}

if [ -z "$pika_bundle" ] && [ -z "$pika_version" ]; then
    # Pin the moving `latest` pointer once, then fetch every other byte from
    # that immutable tag. Users never need to put a version in the install
    # command and a release race cannot mix artifacts.
    download "$PIKA_RELEASE_ROOT/latest/download/pika-version" "$pika_tmp/pika-version" 128
    pika_version=$(sed -n '1p' "$pika_tmp/pika-version")
    [ "$(wc -l < "$pika_tmp/pika-version" | tr -d ' ')" = 1 ] || fail 'Invalid release version file.'
fi
[[ "$pika_version" =~ ^[0-9]+\.[0-9]+\.[0-9]+((a|b|rc)[0-9]+|-(alpha|beta|rc)\.[0-9]+)?$ ]] || \
    fail 'Invalid release tag.'

pika_archive="pikamux-${pika_version}-${pika_target}.tar.gz"
if [ -n "$pika_bundle" ]; then
    [ -f "$pika_bundle/pika-release.json" ] && [ ! -L "$pika_bundle/pika-release.json" ] || fail 'The bundle has no regular pika-release.json.'
    [ -f "$pika_bundle/$pika_archive" ] && [ ! -L "$pika_bundle/$pika_archive" ] || fail "The bundle has no regular $pika_archive."
    [ -f "$pika_bundle/$pika_archive.sha256" ] && [ ! -L "$pika_bundle/$pika_archive.sha256" ] || fail 'The bundle has no regular artifact checksum.'
    cp "$pika_bundle/pika-release.json" "$pika_tmp/pika-release.json"
    cp "$pika_bundle/$pika_archive" "$pika_tmp/$pika_archive"
    cp "$pika_bundle/$pika_archive.sha256" "$pika_tmp/$pika_archive.sha256"
else
    pika_base="$PIKA_RELEASE_ROOT/download/v${pika_version}"
    download "$pika_base/pika-release.json" "$pika_tmp/pika-release.json" 65536
    download "$pika_base/$pika_archive" "$pika_tmp/$pika_archive" 104857600
    download "$pika_base/$pika_archive.sha256" "$pika_tmp/$pika_archive.sha256" 128
fi
[ "$(wc -c < "$pika_tmp/pika-release.json" | tr -d ' ')" -le 65536 ] || fail 'Release manifest exceeds 64 KiB.'

pika_sha=$(sed -n '1p' "$pika_tmp/$pika_archive.sha256")
[ "$(wc -l < "$pika_tmp/$pika_archive.sha256" | tr -d ' ')" = 1 ] || fail 'Invalid artifact checksum file.'
[ "${#pika_sha}" = 64 ] || fail 'Invalid artifact checksum.'
case "$pika_sha" in *[!0-9a-f]*|'') fail 'Invalid artifact checksum.' ;; esac
[ "$(digest "$pika_tmp/$pika_archive")" = "$pika_sha" ] || fail 'Pika checksum mismatch. Downloaded code was not executed.'

# The current POSIX archive format intentionally contains one regular file.
# Exact listing validation makes traversal, duplicate-name and link payloads
# unnecessary; post-extraction checks still reject a link or extra entry.
pika_listing=$(tar -tzf "$pika_tmp/$pika_archive")
[ "$pika_listing" = 'pika' ] || fail 'Native archive contains an unexpected path.'
pika_verbose=$(LC_ALL=C tar -tvzf "$pika_tmp/$pika_archive")
case "$pika_verbose" in -*) ;; *) fail 'Native archive executable is not a regular file.' ;; esac
mkdir "$pika_tmp/extracted"
tar -xzf "$pika_tmp/$pika_archive" -C "$pika_tmp/extracted"
[ -f "$pika_tmp/extracted/pika" ] && [ ! -L "$pika_tmp/extracted/pika" ] || fail 'Native archive has no regular Pika executable.'
[ "$(find "$pika_tmp/extracted" -mindepth 1 -maxdepth 1 | wc -l | tr -d ' ')" = 1 ] || fail 'Native archive extracted unexpected entries.'
[ "$(wc -c < "$pika_tmp/extracted/pika" | tr -d ' ')" -le 104857600 ] || fail 'Native executable exceeds the size limit.'
chmod 700 "$pika_tmp/extracted/pika"

[ "$("$pika_tmp/extracted/pika" --version)" = "pika $pika_version" ] || \
    fail 'Native executable version does not match the selected release.'
"$pika_tmp/extracted/pika" --help >/dev/null
"$pika_tmp/extracted/pika" skill show >/dev/null
install_args=(
    _install-native
    --manifest "$pika_tmp/pika-release.json"
    --artifact "$pika_tmp/$pika_archive"
    --candidate "$pika_tmp/extracted/pika"
    --target "$pika_target"
    --root "$pika_root"
    --bin-dir "$pika_bin_dir"
)
[ -z "$pika_no_setup" ] || install_args+=(--no-setup)
"$pika_tmp/extracted/pika" "${install_args[@]}"

if [ -z "$pika_no_setup" ]; then
    pika_launcher="$pika_bin_dir/pika"
    "$pika_launcher" skill install
    if ! command -v tmux >/dev/null 2>&1; then
        printf '%s\n' \
            'tmux is missing. Pika is installed; Pika needs tmux to host agents.' \
            'Install tmux with Homebrew or your Linux package manager, then run `pika setup`.'
    elif [ -r /dev/tty ] && [ -w /dev/tty ]; then
        printf 'Start Pika setup now? [y/N] ' > /dev/tty
        IFS= read -r pika_answer < /dev/tty || pika_answer=''
        case "$pika_answer" in
            y|Y|yes|YES|Yes) "$pika_launcher" setup < /dev/tty ;;
            *) printf 'Next: %s setup\n' "$pika_launcher" ;;
        esac
    else
        printf 'Next: %s setup\n' "$pika_launcher"
    fi
fi
