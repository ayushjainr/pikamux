#!/usr/bin/env bash
# Pika's inspectable Mac/Linux bootstrap. No sudo, unapproved profile edits, or agent changes.
# uv digests: https://github.com/astral-sh/uv/releases/tag/0.8.15 (asset SHA256).
set -euo pipefail

fail() { printf 'Pika installer: %s\n' "$*" >&2; exit 1; }
usage() {
    printf '%s\n' 'Usage: bash install.sh [--version TAG | --bundle DIRECTORY] [--no-setup]' \
        '                       [--root DIRECTORY] [--bin-dir DIRECTORY]' \
        '' 'Installs Pika for your user on macOS/Linux. No sudo; profile edits require approval.' \
        '--bundle uses a private local release bundle; runtime/dependency downloads may still occur.' \
        '--version selects a published GitHub release; otherwise the newest published version is used.'
}

pika_version=''
pika_bundle=''
pika_install_args=()
while [ "$#" -gt 0 ]; do
    case "$1" in
        --help|-h) usage; exit 0 ;;
        --version|--bundle|--root|--bin-dir)
            [ "$#" -ge 2 ] && [ -n "$2" ] || fail "Missing value for $1. Run bash install.sh --help."
            case "$1" in
                --version) pika_version=$2 ;;
                --bundle) pika_bundle=$2 ;;
                *) pika_install_args+=("$1" "$2") ;;
            esac
            shift 2 ;;
        --no-setup) pika_install_args+=("--no-setup"); shift ;;
        *) fail "Unknown option: $1. Run bash install.sh --help." ;;
    esac
done
# Do not let a caller's package/runtime environment redirect this installation,
# disable TLS verification, or borrow an unrelated virtual environment. compgen
# emits names, not values; no environment content is evaluated as shell code.
while IFS= read -r pika_env_name; do
    [[ "$pika_env_name" =~ ^[A-Za-z_][A-Za-z_0-9]*$ ]] || continue
    case "$pika_env_name" in
        UV_*|PIP_*|PYTHON*|VIRTUAL_ENV|CONDA_PREFIX) unset "$pika_env_name" ;;
    esac
done < <(compgen -e)
[ -z "$pika_version" ] || [ -z "$pika_bundle" ] || fail 'Choose --version or --bundle, not both.'
if [ -n "$pika_version" ]; then
    [[ "$pika_version" =~ ^v?[0-9]+\.[0-9]+\.[0-9]+((a|b|rc)[0-9]+)?$ ]] || fail 'Invalid release tag. Use a version such as v0.5.0a1 or 1.0.0.'
    pika_version="v${pika_version#v}"
fi
if [ -n "$pika_bundle" ]; then
    [ -d "$pika_bundle" ] || fail 'The bundle directory does not exist.'
    pika_bundle=$(cd "$pika_bundle" && pwd -P)
    [ -f "$pika_bundle/pika-release.json" ] || fail 'The bundle has no pika-release.json.'
fi

case "$(uname -s):$(uname -m)" in
    Darwin:arm64|Darwin:aarch64)
        pika_uv_target=aarch64-apple-darwin
        pika_uv_sha=103367962c5cb00bf7370d84cbaa3fec5a9807be9cc833ea9d8eea400c119fa2 ;;
    Darwin:x86_64)
        pika_uv_target=x86_64-apple-darwin
        pika_uv_sha=2bbef70982e97dfc36454de173f35ec1a5e83ae11e3885df6a50db3fd76171cb ;;
    Linux:aarch64|Linux:arm64)
        pika_uv_target=aarch64-unknown-linux-musl
        pika_uv_sha=23ea21a05c62c4c307ce691f29bff2f15c94c4f07f2b83d9b356f0664bc8b3a2 ;;
    Linux:x86_64)
        pika_uv_target=x86_64-unknown-linux-musl
        pika_uv_sha=d0fec58f3124e05e0a1af0f6541abfce4333253cdaf23c7b6bb2e6128bf138ea ;;
    *) fail 'This installer supports macOS/Linux on arm64 or x86_64. Windows users: use a supported WSL Linux environment.' ;;
esac
for pika_command in curl tar mktemp; do
    command -v "$pika_command" >/dev/null 2>&1 || fail "Required command missing: $pika_command. Install it with your system package manager and retry."
done
if command -v sha256sum >/dev/null 2>&1; then
    digest() { sha256sum "$1" | cut -d ' ' -f 1; }
elif command -v shasum >/dev/null 2>&1; then
    digest() { shasum -a 256 "$1" | cut -d ' ' -f 1; }
else
    fail 'A SHA256 verifier is required (sha256sum or shasum). Nothing installed.'
fi

pika_tmp=$(mktemp -d "${TMPDIR:-/tmp}/pika-install.XXXXXXXX")
# Only the exact newly created temporary directory is removed, never an install root.
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
    curl --disable --fail --silent --show-error --location --proto '=https' --proto-redir '=https' \
        --connect-timeout 20 --max-time 300 --retry 2 --max-filesize "${3:-104857600}" --output "$2" "$1" || \
        fail 'Download failed. Check network access and the release tag. Private/unpublished releases require --bundle DIRECTORY.'
}

pika_base=''
if [ -n "$pika_version" ]; then
    pika_base="https://github.com/ayushjainr/pikamux/releases/download/$pika_version"
fi
if [ -n "$pika_bundle" ]; then
    cp "$pika_bundle/pika-release.json" "$pika_tmp/pika-release.json"
elif [ -n "$pika_version" ]; then
    download "$pika_base/pika-release.json" "$pika_tmp/pika-release.json" 65536
else
    download 'https://api.github.com/repos/ayushjainr/pikamux/releases?per_page=100' "$pika_tmp/releases.json" 2097152
fi
if [ -f "$pika_tmp/pika-release.json" ]; then
    pika_manifest_size=$(wc -c < "$pika_tmp/pika-release.json")
    [ "$pika_manifest_size" -le 65536 ] || fail 'Release manifest exceeds the 64 KiB limit. Nothing activated.'
fi
printf '%s\n' 'Preparing a private Pika runtime (no system Python changes)...'
download "https://github.com/astral-sh/uv/releases/download/0.8.15/uv-$pika_uv_target.tar.gz" "$pika_tmp/uv.tar.gz"
[ "$(digest "$pika_tmp/uv.tar.gz")" = "$pika_uv_sha" ] || fail 'uv checksum mismatch. Downloaded code was not executed.'
tar -xzf "$pika_tmp/uv.tar.gz" -C "$pika_tmp" "uv-$pika_uv_target/uv"
pika_uv="$pika_tmp/uv-$pika_uv_target/uv"
"$pika_uv" --no-config python install --no-bin 3.13
pika_python=$("$pika_uv" --no-config python find --no-project --managed-python 3.13)
[ -x "$pika_python" ] || fail 'The managed Python runtime is unavailable.'

# Resolve once, then pin both downloads to that release. GitHub's /latest
# endpoint excludes prereleases; a fresh install must also work during alpha.
if [ -z "$pika_bundle" ] && [ -z "$pika_version" ]; then
    pika_version=$("$pika_python" -I - "$pika_tmp/releases.json" <<'PY'
import json, re, sys
from pathlib import Path
try:
    path = Path(sys.argv[1])
    assert path.stat().st_size <= 2097152
    rows = json.loads(path.read_text(encoding="utf-8"))
    assert isinstance(rows, list)
    candidates = []
    for row in rows:
        if not isinstance(row, dict) or row.get("draft") is not False:
            continue
        tag = row.get("tag_name")
        match = re.fullmatch(r"v([0-9]+)\.([0-9]+)\.([0-9]+)(?:(a|b|rc)([0-9]+))?", tag) if isinstance(tag, str) else None
        if not match or not isinstance(row.get("assets"), list):
            continue
        names = {asset.get("name") for asset in row["assets"]
                 if isinstance(asset, dict) and asset.get("state") == "uploaded"
                 and isinstance(asset.get("name"), str)}
        if not {"pika-release.json", f"pikamux-{tag[1:]}-py3-none-any.whl"} <= names:
            continue
        major, minor, patch, phase, number = match.groups()
        key = (int(major), int(minor), int(patch), {"a": 0, "b": 1, "rc": 2, None: 3}[phase], int(number or 0))
        candidates.append((key, tag))
    assert candidates
    print(max(candidates)[1])
except (OSError, ValueError, TypeError, AssertionError):
    sys.exit("Pika installer: Cannot select a complete published release. Nothing activated. Retry later or use --version TAG.")
PY
    )
    pika_base="https://github.com/ayushjainr/pikamux/releases/download/$pika_version"
    download "$pika_base/pika-release.json" "$pika_tmp/pika-release.json" 65536
    pika_manifest_size=$(wc -c < "$pika_tmp/pika-release.json")
    [ "$pika_manifest_size" -le 65536 ] || fail 'Release manifest exceeds the 64 KiB limit. Nothing activated.'
fi

# Parse JSON with Python, not eval or shell regexes. Never follow a URL/path from the manifest.
"$pika_python" -I - "$pika_tmp/pika-release.json" "$pika_version" > "$pika_tmp/validated" <<'PY'
import json, re, sys
try:
    with open(sys.argv[1], encoding="utf-8") as stream:
        value = json.load(stream)
    assert isinstance(value, dict) and type(value.get("schema")) is int and value["schema"] == 1
    version, wheel, sha = value["version"], value["wheel"], value["sha256"]
    assert isinstance(version, str) and re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+(?:(?:a|b|rc)[0-9]+)?", version)
    assert wheel == f"pikamux-{version}-py3-none-any.whl"
    assert isinstance(sha, str) and re.fullmatch(r"[0-9a-f]{64}", sha)
    assert not sys.argv[2] or sys.argv[2].removeprefix("v") == version
except (OSError, ValueError, KeyError, TypeError, AssertionError):
    sys.exit("Pika installer: Invalid release manifest or requested-version mismatch. Nothing activated.")
print(wheel)
print(sha)
PY
pika_wheel=$(sed -n '1p' "$pika_tmp/validated")
pika_wheel_sha=$(sed -n '2p' "$pika_tmp/validated")
if [ -n "$pika_bundle" ]; then
    [ -f "$pika_bundle/$pika_wheel" ] || fail 'The bundle is missing its declared wheel.'
    cp "$pika_bundle/$pika_wheel" "$pika_tmp/$pika_wheel"
else
    download "$pika_base/$pika_wheel" "$pika_tmp/$pika_wheel"
fi
[ "$(digest "$pika_tmp/$pika_wheel")" = "$pika_wheel_sha" ] || fail 'Pika checksum mismatch. Downloaded Pika code was not executed.'

if ! command -v tmux >/dev/null 2>&1; then
    printf '%s\n' 'tmux is missing; Pika can install now, but protected terminal sessions need it.'
    case "$pika_uv_target" in
        *apple*) printf '%s\n' 'If Homebrew is installed, run: brew install tmux' 'Otherwise install Homebrew from https://brew.sh, then run: brew install tmux' ;;
        *) printf '%s\n' 'Debian/Ubuntu: sudo apt-get install tmux' 'Fedora/RHEL: sudo dnf install tmux' 'Alpine: sudo apk add tmux' ;;
    esac
    printf '%s\n' 'Pika will not run privileged package-manager commands for you.'
fi
# -I excludes user site packages and ambient PYTHONPATH; only the verified wheel is added.
"$pika_python" -I -c 'import runpy,sys; sys.path.insert(0,sys.argv.pop(1)); sys.argv[0]="pikamux.installation"; runpy.run_module("pikamux.installation",run_name="__main__")' \
    "$pika_tmp/$pika_wheel" install --wheel "$pika_tmp/$pika_wheel" --sha256 "$pika_wheel_sha" \
    --uv "$pika_uv" ${pika_install_args[@]+"${pika_install_args[@]}"}
