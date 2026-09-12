# Native performance evidence

Measurements below were captured on 2026-09-12 on an Apple M1 Pro running
macOS 26.6.2. Both implementations used isolated HOME, XDG, provider, database,
temporary, and tmux-socket paths. The board fixture contained 200 synthetic
conversations at 150×35 cells. No provider model, remote machine, user database,
or transcript was contacted.

The native executable was built from the locked dependency graph with the
release profile in `Cargo.toml`: size optimization, thin LTO, one codegen unit,
abort-on-panic, and stripped symbols. The comparison runtime is the frozen
Python v0.5.0a4 reference recorded in `REFERENCE.md`.

| Contract | Samples | Native Rust | Python v0.5.0a4 | Gate |
| --- | ---: | ---: | ---: | ---: |
| CLI startup p95 | 100 each | 4.54 ms | 145.04 ms | ≤30 ms |
| Cached board first-frame p95 | 30 each | 10.04 ms | 463.34 ms | ≤200 ms |
| Board input redraw p95 / p99 | 1,000 each | 0.132 / 0.150 ms | 8.46 / 8.63 ms | ≤50 / 100 ms |
| Hook fast-path p95 | 100 each | 14.76 ms | 489.25 ms | ≤75 ms |
| Full 200-row local reconciliation p95 | 30 each | 70.39 ms | 2,453.04 ms | ≤500 ms |
| Warm board RSS p95 | 300 s native / 60 s Python | 7.08 MiB | 36.42 MiB | ≤40 MiB |
| Native idle board CPU mean | 300 s | 0.553% of one core | — | ≤1% |
| Executable / gzip | one release build | 4.27 / 2.20 MB | interpreter environment required | ≤50 / 20 MiB |

The 100-launch native startup run had a 3.96 ms p50 and 5.35 ms maximum. The
dedicated board run had an 11.09 ms maximum. The five-minute CPU run repainted
the clock once per second, consumed hook-backed SQLite commits within 250 ms,
and performed the fallback provider/process reconciliation every ten seconds.
Its process-tree RSS maximum was 7.28 MiB, RSS p95 was 7.08 MiB, and its
second-half CPU mean was 0.583%.

`measure_reconcile.py` invokes the public `pika list --no-usage` boundary, so its
numbers include process startup, process enumeration, provider metadata reads,
an isolated tmux query, one SQLite reconciliation transaction, projection, and
formatting 200 rows. Standard output is discarded only after Pika writes it.

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
