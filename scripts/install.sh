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

for pika_command in gzip head tar mktemp mkfifo sed tr wc find cut cp mkdir chmod rm sleep uname; do
    command -v "$pika_command" >/dev/null 2>&1 || fail "Required command missing: $pika_command."
done
if [ -z "$pika_bundle" ]; then
    command -v curl >/dev/null 2>&1 || fail 'Required command missing: curl.'
fi
if command -v sha256sum >/dev/null 2>&1; then
    digest() { sha256sum "$1" | cut -d ' ' -f 1; }
elif command -v shasum >/dev/null 2>&1; then
    digest() { shasum -a 256 "$1" | cut -d ' ' -f 1; }
else
    fail 'A SHA256 verifier is required (sha256sum or shasum).'
fi

pika_tmp=$(mktemp -d "${TMPDIR:-/tmp}/pika-install.XXXXXXXX")
pika_active_probe_pgid=''
terminate_active_probe() {
    local pika_probe_pgid=${pika_active_probe_pgid:-}
    [ -n "$pika_probe_pgid" ] || return 0
    # Clear ownership before signalling so cleanup stays idempotent. The
    # monitor-mode wrapper is both process-group leader and the child Bash can
    # reap; it deliberately remains alive until this exact cleanup path runs.
    pika_active_probe_pgid=''
    kill -TERM -- "-$pika_probe_pgid" 2>/dev/null || :
    sleep 0.1
    kill -KILL -- "-$pika_probe_pgid" 2>/dev/null || :
    wait "$pika_probe_pgid" 2>/dev/null || :
}
cleanup() {
    local pika_status=$?
    trap - EXIT
    # Do not let a repeated interrupt strand the group while cleanup is in
    # progress. Preserve the first signal's conventional exit status.
    trap '' INT TERM
    terminate_active_probe
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
    [ -f "$pika_bundle/pika-native-release.json" ] && [ ! -L "$pika_bundle/pika-native-release.json" ] || fail 'The bundle has no regular pika-native-release.json.'
    [ -f "$pika_bundle/$pika_archive" ] && [ ! -L "$pika_bundle/$pika_archive" ] || fail "The bundle has no regular $pika_archive."
    [ -f "$pika_bundle/$pika_archive.sha256" ] && [ ! -L "$pika_bundle/$pika_archive.sha256" ] || fail 'The bundle has no regular artifact checksum.'
    cp "$pika_bundle/pika-native-release.json" "$pika_tmp/pika-native-release.json"
    cp "$pika_bundle/$pika_archive" "$pika_tmp/$pika_archive"
    cp "$pika_bundle/$pika_archive.sha256" "$pika_tmp/$pika_archive.sha256"
else
    pika_base="$PIKA_RELEASE_ROOT/download/v${pika_version}"
    download "$pika_base/pika-native-release.json" "$pika_tmp/pika-native-release.json" 65536
    download "$pika_base/$pika_archive" "$pika_tmp/$pika_archive" 20971520
    download "$pika_base/$pika_archive.sha256" "$pika_tmp/$pika_archive.sha256" 128
fi
[ "$(wc -c < "$pika_tmp/pika-native-release.json" | tr -d ' ')" -le 65536 ] || fail 'Release manifest exceeds 64 KiB.'

pika_sha=$(sed -n '1p' "$pika_tmp/$pika_archive.sha256")
[ "$(wc -l < "$pika_tmp/$pika_archive.sha256" | tr -d ' ')" = 1 ] || fail 'Invalid artifact checksum file.'
[ "${#pika_sha}" = 64 ] || fail 'Invalid artifact checksum.'
case "$pika_sha" in *[!0-9a-f]*|'') fail 'Invalid artifact checksum.' ;; esac
[ "$(digest "$pika_tmp/$pika_archive")" = "$pika_sha" ] || fail 'Pika checksum mismatch. Downloaded code was not executed.'
[ "$(wc -c < "$pika_tmp/$pika_archive" | tr -d ' ')" -le 20971520 ] || fail 'Native archive exceeds the compressed size limit.'

# The current POSIX archive format intentionally contains one regular file.
# Bound decompression before any archive parser or filesystem write. Ignoring
# the upstream SIGPIPE here is intentional: head closes the stream at the
# first byte over the complete-tar budget, so a compression bomb cannot make
# tar scan or extract an unbounded payload.
pika_stream_limit=53477376
pika_stream_bytes=$(
    set +o pipefail
    gzip -dc -- "$pika_tmp/$pika_archive" 2>/dev/null |
        head -c $((pika_stream_limit + 1)) |
        wc -c | tr -d ' '
)
[ "$pika_stream_bytes" -le "$pika_stream_limit" ] || fail 'Native archive expands beyond the 51 MiB safety limit.'
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
[ "$(wc -c < "$pika_tmp/extracted/pika" | tr -d ' ')" -le 52428800 ] || fail 'Native executable exceeds the 50 MiB size limit.'
chmod 700 "$pika_tmp/extracted/pika"

pika_probe_timeout=${PIKA_INSTALL_PROBE_TIMEOUT_SECONDS:-10}
case "$pika_probe_timeout" in *[!0-9]*|'') fail 'Invalid candidate probe timeout.' ;; esac
[ "$pika_probe_timeout" -ge 1 ] && [ "$pika_probe_timeout" -le 60 ] || \
    fail 'Candidate probe timeout must be between 1 and 60 seconds.'
pika_probe_index=0
pika_probe_output=''
candidate_probe() {
    local pika_probe_label=$1
    shift
    pika_probe_index=$((pika_probe_index + 1))
    local pika_probe_stdout="$pika_tmp/probe-$pika_probe_index.stdout"
    local pika_probe_stderr="$pika_tmp/probe-$pika_probe_index.stderr"
    local pika_probe_completion="$pika_tmp/probe-$pika_probe_index.completion"
    # A file-size limit prevents a diagnostic probe from filling the staging
    # filesystem before its wall-clock deadline. Monitor mode gives the
    # candidate an isolated process group. A pre-opened FIFO lets Bash wait for
    # completion without polling or mistaking an exited-but-unreaped process
    # for a live one. The wrapper remains alive after sending status, pinning
    # ownership of the PGID until every inherited descendant is terminated.
    mkfifo "$pika_probe_completion"
    exec 9<>"$pika_probe_completion"
    # Defer an interrupt only across the tiny spawn-to-PGID-publication window.
    # Once the group identity is recorded, restore the normal exit traps and
    # replay the first pending exit through the shared cleanup path.
    local pika_probe_signal_status=''
    trap 'pika_probe_signal_status=130' INT
    trap 'pika_probe_signal_status=143' TERM
    set -m
    (
        ulimit -f 1024 2>/dev/null || :
        set +e
        "$@"
        pika_wrapped_status=$?
        printf '%s\n' "$pika_wrapped_status" >&9
        while :; do sleep 60; done
    ) >"$pika_probe_stdout" 2>"$pika_probe_stderr" &
    pika_active_probe_pgid=$!
    set +m
    trap 'exit 130' INT
    trap 'exit 143' TERM
    [ -z "$pika_probe_signal_status" ] || exit "$pika_probe_signal_status"
    local pika_probe_status=''
    local pika_probe_completed=0
    if IFS= read -r -t "$pika_probe_timeout" pika_probe_status <&9; then
        pika_probe_completed=1
    fi
    exec 9>&-
    # Clean the exact pinned group on success and failure. This closes output
    # files held by descendants before validation continues.
    terminate_active_probe
    [ "$pika_probe_completed" -eq 1 ] || \
        fail "Native executable $pika_probe_label timed out after ${pika_probe_timeout}s."
    case "$pika_probe_status" in *[!0-9]*|'') \
        fail "Native executable $pika_probe_label returned an invalid status receipt." ;; esac
    [ "$(wc -c < "$pika_probe_stdout" | tr -d ' ')" -le 1048576 ] && \
        [ "$(wc -c < "$pika_probe_stderr" | tr -d ' ')" -le 1048576 ] || \
        fail "Native executable $pika_probe_label exceeded the output limit."
    [ "$pika_probe_status" -eq 0 ] || \
        fail "Native executable $pika_probe_label failed (exit $pika_probe_status)."
    pika_probe_output=$(<"$pika_probe_stdout")
}

candidate_probe 'version probe' "$pika_tmp/extracted/pika" --version
[ "$pika_probe_output" = "pika $pika_version" ] || \
    fail 'Native executable version does not match the selected release.'
candidate_probe 'help probe' "$pika_tmp/extracted/pika" --help
candidate_probe 'skill probe' "$pika_tmp/extracted/pika" skill show
install_args=(
    _install-native
    --manifest "$pika_tmp/pika-native-release.json"
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
    if ! pika_skill_detail=$("$pika_launcher" skill install 2>&1); then
        printf '%s\n' \
            "Pika $pika_version is installed at $pika_launcher." \
            "Agent consultation is not configured because skill installation failed: $pika_skill_detail"
        printf 'After correcting the path, run exactly: %q skill install\n' "$pika_launcher"
        printf 'Then run exactly: %q setup\n' "$pika_launcher"
        # The native executable is active, but the advertised agent-to-agent
        # integration is not. Preserve that partial-success distinction for
        # automation instead of claiming the complete install succeeded.
        exit 3
    fi
    [ -z "$pika_skill_detail" ] || printf '%s\n' "$pika_skill_detail"
    if ! command -v tmux >/dev/null 2>&1; then
        printf '%s\n' \
            'tmux is missing. Pika is installed; Pika needs tmux to host agents.' \
            'Install tmux with Homebrew or your Linux package manager, then run `pika setup`.'
    elif { exec 3<>/dev/tty; } 2>/dev/null; then
        printf 'Start Pika setup now? [y/N] ' >&3
        IFS= read -r pika_answer <&3 || pika_answer=''
        case "$pika_answer" in
            y|Y|yes|YES|Yes) "$pika_launcher" setup <&3 ;;
            *) printf 'Next: %s setup\n' "$pika_launcher" ;;
        esac
        exec 3>&-
    else
        printf 'Next: %s setup\n' "$pika_launcher"
    fi
fi
