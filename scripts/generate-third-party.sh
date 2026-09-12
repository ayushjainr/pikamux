#!/usr/bin/env bash
# Generate the dependency inventory shipped beside native release artifacts.
set -euo pipefail

pika_output=${1:-THIRD_PARTY.md}
command -v cargo >/dev/null 2>&1 || { printf 'cargo is required\n' >&2; exit 2; }
pika_tmp=$(mktemp "${TMPDIR:-/tmp}/pika-third-party.XXXXXXXX")
trap 'rm -f -- "$pika_tmp"' EXIT

cargo tree --locked --target all --edges normal,build --prefix none --format $'{p}\t{l}' |
    sed -e '1d' -e 's/ (\*)$//' -e 's/ (proc-macro)//' |
    LC_ALL=C sort -u > "$pika_tmp"

while IFS= read -r pika_license; do
    case "$pika_license" in
        MIT|Apache-2.0|'Apache-2.0 OR MIT'|'MIT OR Apache-2.0'|'Apache-2.0/MIT'|'MIT/Apache-2.0'|\
        '(MIT OR Apache-2.0) AND Unicode-3.0'|'Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT'|\
        'MIT OR Apache-2.0 OR LGPL-2.1-or-later'|BSD-3-Clause|ISC|MPL-2.0|Zlib|\
        'Unlicense OR MIT'|'Unlicense/MIT') ;;
        *) printf 'Unreviewed dependency license expression: %s\n' "$pika_license" >&2; exit 1 ;;
    esac
done < <(cut -f2 "$pika_tmp" | LC_ALL=C sort -u)

{
    printf '%s\n' '# Third-party software'
    printf '\n%s\n\n' 'Pika includes the following runtime and build-time Rust packages. License expressions come from the locked package metadata.'
    printf '%s\n' \
        '| Package | Version | License | Source |' \
        '| --- | --- | --- | --- |'
    while IFS=$'\t' read -r pika_package pika_license; do
        pika_name=${pika_package%% *}
        pika_version=${pika_package#* v}
        printf '| `%s` | `%s` | `%s` | https://crates.io/crates/%s/%s |\n' \
            "$pika_name" "$pika_version" "$pika_license" "$pika_name" "$pika_version"
    done < "$pika_tmp"
} > "$pika_output"
