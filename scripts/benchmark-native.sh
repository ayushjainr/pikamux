#!/usr/bin/env bash
# Measure process startup and artifact size without reading real Pika state.
set -euo pipefail

pika_script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
pika_repo_dir=$(dirname "$pika_script_dir")
cd "$pika_repo_dir"

pika_binary=${1:-target/release/pika}
pika_samples=${PIKA_BENCH_SAMPLES:-100}
[ -x "$pika_binary" ] || { printf 'Build a release binary first: cargo build --release\n' >&2; exit 2; }
[[ "$pika_samples" =~ ^[1-9][0-9]*$ ]] || { printf 'PIKA_BENCH_SAMPLES must be a positive integer.\n' >&2; exit 2; }

pika_tmp=$(mktemp -d "${TMPDIR:-/tmp}/pika-benchmark.XXXXXXXX")
trap 'rm -rf -- "$pika_tmp"' EXIT
mkdir -p "$pika_tmp/home" "$pika_tmp/state" "$pika_tmp/config" "$pika_tmp/cache"
export HOME="$pika_tmp/home"
export XDG_STATE_HOME="$pika_tmp/state"
export XDG_CONFIG_HOME="$pika_tmp/config"
export XDG_CACHE_HOME="$pika_tmp/cache"
export PIKA_DB_PATH="$pika_tmp/state/pika.db"
export PIKA_UPDATE_CHECK=0

printf 'label\tsamples\tp50_ms\tp95_ms\tmax_ms\n'
scripts/measure-startup.pl native "$pika_binary" "$pika_samples" 2>/dev/null

pika_bytes=$(wc -c < "$pika_binary" | tr -d ' ')
gzip -9c "$pika_binary" > "$pika_tmp/pika.gz"
pika_compressed=$(wc -c < "$pika_tmp/pika.gz" | tr -d ' ')
printf 'binary_bytes\t%s\ncompressed_bytes\t%s\n' "$pika_bytes" "$pika_compressed"
measure_rss() {
    local pika_label=$1
    local pika_program=$2
    local pika_rss_file="$pika_tmp/rss-$pika_label"
    case "$(uname -s)" in
        Darwin)
            /usr/bin/time -l "$pika_program" --version >/dev/null 2> "$pika_rss_file"
            awk -v label="$pika_label" '/maximum resident set size/ {print label "_peak_rss_bytes\t" $1}' "$pika_rss_file"
            ;;
        Linux)
            /usr/bin/time -f '%M' "$pika_program" --version >/dev/null 2> "$pika_rss_file"
            awk -v label="$pika_label" 'NR == 1 {print label "_peak_rss_bytes\t" ($1 * 1024)}' "$pika_rss_file"
            ;;
    esac
}
measure_rss native "$pika_binary"
printf 'os\t%s\nhardware\t%s\n' "$(uname -srv)" "$(uname -m)"
printf 'cargo_profile\trelease opt-level=z thin-lto panic=abort stripped\n'
printf 'lock_sha256\t%s\n' "$(shasum -a 256 Cargo.lock | cut -d ' ' -f 1)"
