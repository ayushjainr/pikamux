# Native performance evidence

Measurements below were captured on 2026-09-12 on an Apple M1 Pro running
macOS 26.6.2. Both implementations used isolated HOME, XDG, provider, database,
temporary, and tmux-socket paths. The board fixture contained 200 synthetic
local conversations plus one cached remote conversation at 150×35 cells. A
fake offline SSH executable proved that the complete first frame included the
remote sentinel before reconciliation; no provider model, network, remote
machine, user database, or transcript was contacted.

The native executable was built from the locked dependency graph with the
release profile in `Cargo.toml`: size optimization, thin LTO, one codegen unit,
abort-on-panic, and stripped symbols. The comparison runtime is the frozen
Python v0.5.0a4 reference recorded in `REFERENCE.md`. The measured native source
is commit `c4ae6153b99ebfe4ff0be4afab2d562e4e070865`; the measured executable's
SHA-256 is `60c1a13967d53338201449815402cda7c8c55351649f492dd75265eadef6d68c`.

| Contract | Samples | Native Rust | Python v0.5.0a4 | Gate |
| --- | ---: | ---: | ---: | ---: |
| CLI startup p95 | 100 each | 4.74 ms | 137.16 ms | ≤30 ms |
| Cached board first-frame p95 | 30 each | 11.11 ms | 454.81 ms | ≤200 ms |
| Board input redraw p95 / p99 | 1,000 each | 0.117 / 0.137 ms | 8.26 / 8.64 ms | ≤50 / 100 ms |
| Hook fast-path p95 | 100 each | 13.07 ms | 473.13 ms | ≤75 ms |
| Full 200-row local reconciliation p95 | 30 each | 74.80 ms | 2,352.12 ms | ≤500 ms |
| Warm board RSS p95, worst run | 5 × 300 s | 11.91 MiB | 39.41 MiB | ≤40 MiB |
| Idle board CPU, mean across runs | 5 × 300 s | 0.925% of one core | 19.637% of one core | ≤1% native |
| Executable / gzip | one release build | 4.49 / 2.31 MB | interpreter environment required | ≤50 / 20 MiB |

The 100-launch native startup run had a 3.75 ms p50. One sample reached
382.40 ms while p95 remained 4.74 ms; the outlier is retained here rather than
discarded. The cached-local-plus-remote board run had a 13.60 ms maximum.

The steady-state comparison alternated implementation order across five paired
five-minute runs. Every run repainted the clock once per second, made
hook-backed SQLite commits visible within one second, and performed fallback
provider/process reconciliation every twenty seconds. Native CPU ranged from
0.799% to 1.049% by run, versus 19.227% to 20.412% for Python. Worst observed
process-tree RSS was 12.56 MiB native and 39.92 MiB Python. Native emitted
929,617 terminal bytes after the first frame in every run; Python averaged
2,041,555 bytes. These are aggregate and worst-run results, not a selected run.

Provider scaling is separately regression-tested with 2,000 watched Codex rows,
2,000 Claude transcripts, and 2,000 OpenCode roots. Codex/Claude healing advances
at most 16 changed transcript records per cycle under aggregate byte limits;
OpenCode projects descendant activity and lifecycle with two batched recursive
queries rather than per-row queries. Hooks remain the immediate status path.

`measure_reconcile.py` invokes the public `pika list --no-usage` boundary, so its
numbers include process startup, process enumeration, provider metadata reads,
an isolated tmux query, bounded SQLite reconciliation transactions, projection,
and formatting 200 rows. Standard output is discarded only after Pika writes it.

Reproduce the evidence:

```sh
scripts/benchmark-native.sh target/release/pika dev/python-v050a4
python3 dev/measure_board.py target/release/pika \
  --python-reference dev/python-v050a4 --samples 30 --input-samples 1000
python3 dev/measure_hook.py target/release/pika dev/python-v050a4 --samples 100
python3 dev/measure_reconcile.py target/release/pika dev/python-v050a4 --samples 30
python3 dev/measure_board_steady.py target/release/pika dev/python-v050a4 \
  --seconds 300 --runs 5 --program both
```

These are machine-specific measurements, not promises about provider inference
time. Consultation tests measure Pika's bounded protocol stages with fake
providers; they do not claim that Rust makes a model answer faster.
