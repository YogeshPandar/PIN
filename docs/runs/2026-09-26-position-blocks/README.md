# Position block kernel experiment

Source: `a396165`. Format and integration gates: [PB01](../../position-blocks.md).

This is an in-memory kernel benchmark, **not a SQL, GIN, or TIN benchmark**.
Native storage does not use PB01. Six rounds alternate execution order for four
methods. Targets cover early, middle, late and past-end lookups. Input is a
strictly increasing sequence of even positions. Sizes are 32, 10,000 and
1,000,000 occurrences. Each size uses 10,000, 1,000 and 10 repetitions per round,
respectively. Results are medians of per-round average elapsed nanoseconds.
They are warm VM wall timings, not per-query backend CPU or hardware cycles.
The second invocation repeats the same benchmark and is preserved separately.

Lazy linear reads canonical deltas until the first matching occurrence; it does
not validate the unused tail. Validated linear first checks the complete legacy
stream. Open+seek checks the entire block directory and the selected block.
Reused seek assumes a previously checked directory over still-borrowed bytes.
These deliberately expose different integrity/setup work; the lazy baseline is
the conservative comparison. No method copies the payload or accesses PG pages.

| Occurrences | Target | Lazy linear ns | Open+seek ns | Reused seek ns | Repeat open+seek ns | Decoded occurrences |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 32 | 0 | 5.0 | 90.5 | 76.5 | 69.5 | 32 |
| 32 | 32 | 35.5 | 104.5 | 98.5 | 68.0 | 32 |
| 32 | 61 | 39.0 | 69.0 | 56.0 | 68.0 | 32 |
| 32 | 64 | 37.5 | 17.0 | 3.0 | 17.0 | 0 |
| 10000 | 0 | 3.0 | 770.0 | 337.5 | 746.0 | 128 |
| 10000 | 10000 | 9732.0 | 724.5 | 290.5 | 719.5 | 128 |
| 10000 | 19997 | 18797.0 | 519.5 | 51.0 | 479.5 | 16 |
| 10000 | 20000 | 19210.0 | 446.0 | 11.5 | 450.5 | 0 |
| 1000000 | 0 | 11.5 | 42473.5 | 275.5 | 42274.5 | 128 |
| 1000000 | 1000000 | 624099.0 | 43038.0 | 260.0 | 42882.0 | 128 |
| 1000000 | 1999997 | 1270121.0 | 42131.5 | 162.5 | 42423.0 | 64 |
| 1000000 | 2000000 | 1370102.0 | 42007.5 | 39.5 | 42518.0 | 0 |

## Interpretation

Late and past-end seeks can avoid nearly all delta decoding. Early witnesses
regress sharply against lazy linear scanning, and short streams often lose.
Opening a million-occurrence directory still costs tens of microseconds. A
previous exploratory invocation had substantially higher directory-open timings;
VM timing variation means these numbers are not a latency guarantee. The
structural bound, at most 128 decoded occurrences, is the stronger result.
Do not multiply these kernel ratios by the old SQL times to predict a speedup.

Dense-list storage rises from 10,004 to 11,193 bytes at 10,000 occurrences and
from 1,000,004 to 1,117,203 bytes at one million occurrences (see raw CSV for
exact sizes). Independent restart metadata is a trade-off, not free compression.

The next native design must retain the prefix-witness path and supply direct
physical selected-block access with bounded directory lifetime. Putting PB01
inside the same copied linked payload cannot remove the measured memmove cost.
No syscalls, crash recovery, concurrent updates, cache-miss behavior or SQL
end-to-end speedup were measured by this experiment.

## Validation and reproduction

Core suite: 206 passed, zero failed, three pre-existing ignored tests. New block
suite: five tests with generated sorted-vector oracle, extreme values, malformed
bytes, truncation, output atomicity and selected/skipped corruption behavior.
All-target core Clippy passes with warnings denied. This iteration introduces no
native path change, so native lifecycle results remain those of the preceding
prefix-reader checkpoint, rather than a newly claimed PG run.

```sh
cargo test -p pin-core
cargo clippy -p pin-core --all-targets -- -D warnings
cargo run --release -p pin-core --example position_seek_bench > seek.csv
```

Raw CSV, test/build logs, platform metadata and whole-process resource usage are
included. Whole-process resource usage includes the benchmark loop; the first
invocation also includes Cargo. It is not a per-seek CPU measurement. No hardware
performance counters were collected. SHA256SUMS covers every other file here.
