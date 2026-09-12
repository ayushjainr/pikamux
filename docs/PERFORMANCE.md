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
Python v0.5.0a4 reference recorded in `REFERENCE.md`.

| Contract | Samples | Native Rust | Python v0.5.0a4 | Gate |
| --- | ---: | ---: | ---: | ---: |
| CLI startup p95 | 100 each | 4.20 ms | 137.81 ms | ≤30 ms |
| Cached board first-frame p95 | 30 each | 10.85 ms | 461.88 ms | ≤200 ms |
| Board input redraw p95 / p99 | 1,000 each | 0.116 / 0.131 ms | 8.33 / 8.64 ms | ≤50 / 100 ms |
| Hook fast-path p95 | 100 each | 15.41 ms | 475.64 ms | ≤75 ms |
| Full 200-row local reconciliation p95 | 30 each | 78.37 ms | 2,259.89 ms | ≤500 ms |
| Warm board RSS p95 | 300 s native / 60 s Python | 10.77 MiB | 36.42 MiB | ≤40 MiB |
| Native idle board CPU mean | 300 s | 0.905% of one core | — | ≤1% |
| Executable / gzip | one release build | 4.43 / 2.28 MB | interpreter environment required | ≤50 / 20 MiB |

The 100-launch native startup run had a 3.67 ms p50 and one 374 ms scheduler
outlier; its p95 remained 4.20 ms. The cached-local-plus-remote board run had
a 13.32 ms maximum. The five-minute CPU run repainted
the clock once per second, made hook-backed SQLite commits visible within one second,
and performed the fallback provider/process reconciliation every twenty seconds.
Its process-tree RSS maximum/p95 were 11.59/10.77 MiB, and its second-half CPU
mean was 0.877%.

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
  --seconds 300 --program native
```

These are machine-specific measurements, not promises about provider inference
time. Consultation tests measure Pika's bounded protocol stages with fake
providers; they do not claim that Rust makes a model answer faster.
