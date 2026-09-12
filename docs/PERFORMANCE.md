# Performance

Measurements from the release candidate on an Apple M1 Pro, macOS 26.6.2,
using synthetic conversations and a fixed terminal size:

| Operation | p95 | Samples |
| --- | ---: | ---: |
| Command startup | 4.66 ms | 100 |
| Board first frame, 200 conversations | 13.36 ms | 50 |
| Board input redraw | 0.153 ms | 1,000 |
| Lifecycle hook | 14.18 ms | 100 |
| Reconcile 200 conversations | 78.77 ms | 30 |

With 2,000 conversations across 20 cached nodes, first-frame p95 was 60.34 ms
and input p95 stayed below 0.26 ms. Peak memory in those scale runs stayed below
24.5 MiB.

Five five-minute idle runs on the preceding runtime candidate averaged 0.63%
CPU and peaked at 12.2 MiB RSS. The final delta affected explicit update
checks and release verification, not the idle board.

The measured optimized macOS executable was 4.6 MB, or 2.37 MB compressed.
Hardware, history size, provider latency, and network conditions affect
real-world timings; these are local synthetic measurements.

To measure command startup and binary size on your machine:

```bash
cargo build --release --locked
scripts/benchmark-native.sh target/release/pika
```
