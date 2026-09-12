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
is commit `d8c91a0357183d058ce3ef146af44a5dbefa8221`; the measured executable's
SHA-256 is `e6dd522bef9b091dab286a93a0bf1d947f936ac17c9d06eda03261c5e0b4bad9`.

| Contract | Samples | Native Rust | Python v0.5.0a4 | Gate |
| --- | ---: | ---: | ---: | ---: |
| CLI startup p95 | 100 each | 4.74 ms | 145.83 ms | ≤30 ms |
| Cached board first-frame p95 | 50 each | 15.23 ms | 473.11 ms | ≤200 ms |
| Board input redraw p95 / p99 | 1,000 each | 0.147 / 0.174 ms | 8.34 / 8.61 ms | ≤50 / 100 ms |
| Hook fast-path p95 | 100 each | 14.46 ms | 506.17 ms | ≤75 ms |
| Full 200-row local reconciliation p95 | 30 each | 74.83 ms | 2,300.57 ms | ≤500 ms |
| Warm board RSS p95, worst run | 5 × 300 s | 12.11 MiB | 40.16 MiB | ≤40 MiB native |
| Idle board CPU, mean across runs | 5 × 300 s | 0.827% of one core | 20.009% of one core | ≤1% native |
| Executable / gzip | one release build | 4.57 / 2.35 MB | interpreter environment required | ≤50 / 20 MiB |

The 100-launch native startup run had a 4.04 ms p50 and 7.46 ms maximum. The
50-launch cached-local-plus-remote board run had an 11.84 ms p50; one cold
outlier reached 381.90 ms without changing the p95 gate.

The steady-state comparison alternated implementation order across five paired
five-minute runs. Every run repainted the clock once per second, made
hook-backed SQLite commits visible within one second, and performed fallback
provider/process reconciliation every twenty seconds. Native CPU ranged from
0.695% to 0.951% by run, versus 18.784% to 21.525% for Python. Worst observed
process-tree RSS was 12.58 MiB native and 40.41 MiB Python. Native averaged
930,242 terminal bytes after the first frame; Python averaged 2,106,315 bytes.
These are aggregate and worst-run results, not a selected run.
Across all five runs, Rust used 24.2× less mean CPU and 3.2× less peak RSS.

The same native binary was also exercised with 2,000 local conversations and
1, 5, and 20 cached offline machines. The complete first frame never waited on
SSH, and adding machines did not remove local rows.

| Cached machines | First-frame p95 | Input p95 | 60 s RSS p95 / max |
| ---: | ---: | ---: | ---: |
| 1 | 23.52 ms | 0.196 ms | 22.52 / 22.83 MiB |
| 5 | 27.84 ms | 0.200 ms | 23.50 / 23.56 MiB |
| 20 | 41.80 ms | 0.209 ms | 23.55 / 23.56 MiB |

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
8,000-row / 16 MiB aggregate budget; exact actions and expert search retain the
full authoritative snapshots.

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
