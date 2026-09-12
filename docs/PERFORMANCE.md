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
is commit `f3e013c35bb843cbf154033799bf26ec09e8f3b3`; the measured executable's
SHA-256 is `d35bed57af72f6c1275790b45b2a0a27af22813559944a8df0b3c54cebae7aef`.

| Contract | Samples | Native Rust | Python v0.5.0a4 | Gate |
| --- | ---: | ---: | ---: | ---: |
| CLI startup p95 | 100 each | 5.09 ms | 155.08 ms | ≤30 ms |
| Cached board first-frame p95 | 50 each | 14.84 ms | 491.78 ms | ≤200 ms |
| Board input redraw p95 / p99 | 1,000 each | 0.158 / 0.199 ms | 8.74 / 9.17 ms | ≤50 / 100 ms |
| Hook fast-path p95 | 100 each | 14.21 ms | 501.25 ms | ≤75 ms |
| Full 200-row local reconciliation p95 | 30 each | 76.20 ms | 2,472.82 ms | ≤500 ms |
| Warm board RSS p95, worst run | 5 × 300 s | 11.64 MiB | 37.03 MiB | ≤40 MiB native |
| Idle board CPU, mean across runs | 5 × 300 s | 0.626% of one core | 18.203% of one core | ≤1% native |
| Executable / gzip | one release build | 4.60 / 2.37 MB | interpreter environment required | ≤50 / 20 MiB |

The 100-launch native startup run had a 4.55 ms p50; one cold outlier reached
417.55 ms without changing the 5.09 ms p95 gate. The 50-launch
cached-local-plus-remote board run had a 13.64 ms p50 and 15.15 ms maximum.

The steady-state comparison alternated implementation order across five paired
five-minute runs. Every run repainted the clock once per second, made
hook-backed SQLite commits visible within one second, and performed fallback
provider/process reconciliation every twenty seconds. Native CPU ranged from
0.463% to 0.752% by run, versus 16.528% to 20.169% for Python. Worst observed
process-tree RSS was 12.19 MiB native and 40.13 MiB Python. Native averaged
978,706 terminal bytes after the first frame; Python averaged 2,077,755 bytes.
These are aggregate and worst-run results, not a selected run.
Across all five runs, Rust used 29.1× less mean CPU and 3.3× less peak RSS.

The same native binary was also exercised with 2,000 local conversations and
1, 5, and 20 cached offline machines. The complete first frame never waited on
SSH, and adding machines did not remove local rows.

| Cached machines | First-frame p95 | Input p95 | 60 s RSS p95 / max |
| ---: | ---: | ---: | ---: |
| 1 | 25.24 ms | 0.213 ms | 21.52 / 21.94 MiB |
| 5 | 30.83 ms | 0.201 ms | 23.22 / 24.52 MiB |
| 20 | 50.00 ms | 0.212 ms | 23.38 / 24.48 MiB |

This directly clears the 2,000-row gates of a complete first frame within one
second, input p95 within 100 ms, and a bounded memory plateau across the fleet
fan-out.

Provider scaling is separately regression-tested with 2,000 watched Codex rows,
2,000 Claude transcripts, and 2,000 OpenCode roots. Codex/Claude healing advances
at most 16 changed transcript records per cycle under aggregate byte limits;
OpenCode projects descendant activity and lifecycle with two batched recursive
queries rather than per-row queries. Hooks remain the immediate status path.
The federated first-frame contract also supplies 20 cached machines with 2,000
rows each. The board reads a fair 400-row projection per machine under one
8,000-row / 16 MiB aggregate budget. Exact actions retain the authoritative
snapshots. Expert search ranks a complete, revision-bound index of up to 2,000
experts per machine, then materializes only the fair aggregate result slice.

`measure_reconcile.py` invokes the public `pika list --no-usage` boundary, so its
numbers include process startup, process enumeration, provider metadata reads,
an isolated tmux query, bounded SQLite reconciliation transactions, projection,
and formatting 200 rows. Standard output is discarded only after Pika writes it.

Reproduce the evidence:

```sh
scripts/benchmark-native.sh target/release/pika dev/python-v050a4
python3 dev/measure_board.py target/release/pika \
  --python-reference dev/python-v050a4 --samples 50 --input-samples 1000
python3 dev/measure_hook.py target/release/pika dev/python-v050a4 --samples 100
python3 dev/measure_reconcile.py target/release/pika dev/python-v050a4 --samples 30
python3 dev/measure_board_steady.py target/release/pika dev/python-v050a4 \
  --seconds 300 --runs 5 --program both
for nodes in 1 5 20; do
  python3 dev/measure_board.py target/release/pika \
    --samples 30 --input-samples 1000 --rows 2000 --remote-nodes "$nodes"
  python3 dev/measure_board_steady.py target/release/pika dev/python-v050a4 \
    --seconds 60 --runs 1 --program native --rows 2000 --remote-nodes "$nodes"
done
```

These are machine-specific measurements, not promises about provider inference
time. Consultation tests measure Pika's bounded protocol stages with fake
providers; they do not claim that Rust makes a model answer faster.
