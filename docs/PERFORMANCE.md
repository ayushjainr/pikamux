# Native performance evidence

Measurements below were captured on 2026-09-11 on an Apple M1 Pro running
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
| CLI startup p95 | 100 each | 6.29 ms | 154.94 ms | ≤30 ms |
| Cached board first-frame p95 | 30 each | 12.61 ms | 490.47 ms | ≤200 ms |
| Board input redraw p95 / p99 | 1,000 each | 0.118 / 0.146 ms | 8.58 / 8.74 ms | ≤50 / 100 ms |
| Hook fast-path p95 | 100 each | 25.04 ms | 515.52 ms | ≤75 ms |
| Full 200-row local reconciliation p95 | 30 each | 68.63 ms | 2,785.48 ms | ≤500 ms |
| Warm board RSS p95 | 300 s native / 60 s Python | 7.03 MiB | 36.42 MiB | ≤40 MiB |
| Native idle board CPU mean | 300 s | 0.635% of one core | — | ≤1% |
| Executable / gzip | one release build | 4.61 / 2.15 MB | interpreter environment required | ≤50 / 20 MiB |

The 100-launch native startup run had a 4.38 ms p50 and 9.83 ms maximum. The
dedicated board run had a 16.36 ms maximum. The five-minute CPU run repainted the clock once per
second and performed the fallback provider/process reconciliation every ten
seconds; lifecycle hooks remain immediate. Its process-tree RSS maximum was
7.69 MiB, RSS p95 was 7.03 MiB, and its second-half CPU mean was 0.610%.

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
