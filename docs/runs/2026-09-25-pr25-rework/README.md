# PR 25 rework: storage size and grouped count CPU

Date: 25 September 2026. Branch: `codex/pr25-rework`. The four measured
checkpoints are identified by the `source-head.txt` in each raw benchmark
directory. These are local development measurements, not a TIN comparison or a
production certification.

## Reproduction and provenance

An isolated PostgreSQL 18.6 cluster on this GCP VM used 8 KiB pages, durable
WAL settings, one persistent backend, warm cache, 20,000 synthetic text rows,
six alternating blocks of ten queries per engine, two warmups, and complete
ordered row-identity checks against the PostgreSQL `simple` dictionary. The
experimental grouped storage, delta seal, count, VM, and page visibility paths
were enabled for the PIN grouped mode. The normal PostgreSQL GIN index was the
control. Each run's `environment.json`, source hash ledger, SQL, EXPLAIN plans,
per-block backend `schedstat` CPU, client latency, write measurements, and
identity oracles are in [raw-benchmarks.tar.gz](raw-benchmarks.tar.gz). The
paired page visibility script and raw results are also there. The result is
backend CPU per query, including executor work; six short samples make the
reported p95/p99 only descriptive. The VM did not expose hardware PMU counters.

Native lifecycle qualification on the final build passed with grouped page
visibility on and off, including count result identity, HOT changes, deletion,
VACUUM, restart, cached plans, and RLS fallback. Its SQL and output are in
[native-qualification.tar.gz](native-qualification.tar.gz), status `passed`.
The standalone delta SQL also passed. Pure Rust tests: 13 passed, 1 ignored.
Python tests: 147 passed. Workspace clippy passed with warnings denied. This
does not replace an independent review of the new C buffer and FFI path, nor
old-snapshot, fault-injection, and concurrent-write stress testing.

## Results

Median backend CPU per query, microseconds. Ratios compare PIN grouped to GIN
within the *same* run. Builds and VM noise differ between run columns.

| State and query | PR 25 base PIN / GIN | Inline 18 PIN / GIN | Page batch PIN / GIN |
| --- | ---: | ---: | ---: |
| Fresh broad AND | 295 / 3004 | 228 / 2446 | 227 / 2675 |
| Fresh broad OR | 281 / 3314 | 270 / 2842 | 264 / 3017 |
| Long delta broad AND | 1048 / 3940 | 918 / 3565 | 504 / 4517 |
| Long delta broad OR | 1182 / 4920 | 1180 / 4344 | 439 / 4008 |
| Updated/deleted broad AND | 4176 / 4559 | 2953 / 3370 | 816 / 3583 |
| Updated/deleted broad OR | 4417 / 4556 | 4370 / 4039 | 1151 / 4117 |
| Rebuilt broad AND | 286 / 3409 | 233 / 3217 | 272 / 3705 |

On the same final binary and mutated table, alternating page visibility off/on
gave broad AND medians **1447 / 465 microseconds** and broad OR **1869 / 598
microseconds**. Both modes returned the same count and used the grouped plan.
This paired check isolates about a 3.1x CPU reduction for dirty-page visibility
on that table; the cross-build figures above should not be read as an exact
single-change speedup.

The final broad AND updated plan read 29 PIN index pages and 111,122 index
payload bytes, then checked 14,128 heap roots because their pages were not
all-visible. The grouped result had 652 heap pages and 18,072 candidate roots.
This is why the updated case was CPU heavy even when its indexed query payload
was small. The page batch reduced repeated buffer operations by checking all
offsets on a dirty heap page under one lock. It remains opt-in and default off.

## Why the index is large

The fresh PIN index fell from 120,070,144 bytes in the base build to 44,720,128
bytes with inline grouped postings. GIN was 778,240 bytes on the same tiny,
synthetic fixture. The long-delta PIN index fell from 217,448,448 to 79,454,208
bytes; GIN was 1,392,640 bytes. Raising inline capacity from six roots to
eighteen made almost no difference on this corpus. A separate canonical-only
index measured 43,393,024 bytes for the fresh 20,000 rows, so grouped postings
are not the remaining fresh-size bottleneck.

After the final benchmark's VACUUM and a checkpoint, the 79,470,592-byte PIN
index had the following page census. The exact raw counts are in
[page-layout.json](page-layout.json). This is one post-maintenance snapshot;
it does not describe all workloads.

| Page kind | Pages | Used PIN payload bytes | Mean used bytes/page |
| --- | ---: | ---: | ---: |
| Free | 4,252 | 68,032 | 16 |
| Sealed per-term postings | 4,137 | 536,489 | 130 |
| Owners | 605 | 4,846,357 | 8,011 |
| Dictionary | 512 | 270,412 | 528 |
| Grouped | 194 | 1,418,572 | 7,312 |

The free and sealed posting pages together reserve about 68.7 MB, 86.5% of
this relation's physical size. Free pages are available for reuse but are not
necessarily read by a query. Thousands of sealed posting pages hold only a few
hundred kilobytes of useful payload. The current writer allocates a dedicated
posting page when a term gets a second owner, and sealing retains one per-term
page chain. The 512 dictionary buckets also reserve a page each. The size
problem is principally page granularity and write amplification; it is not
proof that queries read the whole index.

PlanetScale's published 85 GB, 150-million-document test reports a 50.7 GB TIN
index versus 28.0 GB GIN. Its TIN advantage therefore cannot be reduced to
smaller total index size. TIN attributes speed to compressed CTID-native page
and offset bitmaps, page-level Boolean operations, exact metadata, selective
visibility checks, and efficient segment merges. Its corpus, hardware, query
mix, and TIN binary are not used in this run.

## Next architecture work

1. Store small canonical posting lists inline or in densely packed posting
   arenas, with a bounded promotion path to page/offset bitmaps. Preserve owner
   incarnation and MVCC invariants across HOT updates and recycled CTIDs.
2. Seal and merge into compact, sorted, CTID-native segments without one page
   per rare term. Reclaim or truncate unused tail pages safely under PostgreSQL
   WAL and VACUUM rules. Measure bytes allocated and WAL per document.
3. Reduce owner and dictionary lookup work for normal row-returning searches.
   The fast COUNT path is a narrow SQL shape; it does not establish 10x GIN for
   phrase, top-k, fuzzy, wildcard, multiuser writes, or row retrieval.
4. Before changing defaults, independently review C locking and error paths,
   test old snapshots, crash/recovery, concurrent writers, and read replicas,
   then benchmark a natural-language corpus with controlled memory pressure.

Reference contracts: [TIN architecture](https://planetscale.com/blog/introducing-tin),
[TIN index anatomy](https://planetscale.com/blog/anatomy-of-a-postgres-search-engine),
[PostgreSQL index AM](https://www.postgresql.org/docs/18/index-functions.html),
[visibility map](https://www.postgresql.org/docs/18/storage-vm.html), and the
exact source and local proof ledger in [api-evidence.md](../../api-evidence.md).
