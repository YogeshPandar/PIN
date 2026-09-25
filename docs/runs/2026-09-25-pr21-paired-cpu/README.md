# PR #21 owner frontier CPU review

Code under test: draft [PR #21](https://github.com/YogeshPandar/PIN/pull/21),
`2a5818fd66cfe5a5e9622d19798f2fee61ef7d37`. The local review checkout
fetched this exact commit. The extension binary reports the same revision.
This report does not establish 10x-over-GIN performance or TIN parity.

## Method

PostgreSQL 18.6 was built from upstream commit
`724edf9bde9d356724ad384a2e196edc3c9f80f7` with assertions and OpenSSL,
but without readline because that development library was unavailable here.
The exact PR extension was installed into a private PostgreSQL prefix. Its
SHA-256 was `1c58e324d05a6da6fc6a1f7fc0afc5954059897377c8b98c8d83a896bd0ba6b5`.
The owner-off and owner-on runs used that same binary and separate disposable
clusters on the same four-vCPU VM. Both enabled grouped storage, grouped scan,
and frontier anchors; only `pin.enable_owner_frontier` changed. Full revision,
registered GUC, bitmap-index plan, and exact row-identity checks passed in both
runs. Each run completed its archived driver manifest.

The fixture began with 16,384 documents, then received 1,000 unrelated writes
and 4,096 related writes. Each stage sampled rare, selective AND, broad AND,
and broad OR queries. Backend on-CPU time comes from Linux `/proc` schedstat,
with two one-second samples per query and engine. The cluster used durable
settings, disabled autovacuum, and had warm reads. The driver measured the full
PostgreSQL query backend, including index and heap work. It did not measure
client CPU, p95/p99, cold-cache behavior, or steady concurrent production load.

The two separate-cluster runs had substantial environmental drift: even GIN's
CPU measurements changed between clusters. Therefore their direct off/on
ratios are not a reliable estimate of the feature gain. A tighter follow-up
restarted the completed owner-on cluster after its grouped rebuild, inserted
another 4,096 related documents, and alternated owner-off, owner-on, and GIN
in the same backend on the same 25,576-row fixture. Four order-balanced blocks
per query class executed 100 prepared queries per mode and block. Each mode
used a bitmap-index plan and returned the same count; the full separate-cluster
runs had already checked exact row identities. The table below uses the median
of those four backend-CPU blocks, in microseconds per query.

## Same-backend long-delta result

| Query | Owner off | Owner on | GIN | PIN gain | PIN on / GIN |
| --- | ---: | ---: | ---: | ---: | ---: |
| Rare term | 3,820 | 3,736 | 2,760 | 1.02x | 1.35x |
| Selective AND | 4,250 | 2,577 | 2,678 | **1.65x** | 0.96x |
| Broad AND | 6,222 | 5,076 | 7,244 | 1.23x | 0.70x |
| Broad OR | 5,792 | 4,611 | 4,917 | 1.26x | 0.94x |

Selective AND improved in every order-balanced block: 1.63x, 1.65x, 1.75x,
and 1.54x. The rare query has only one changed term and should avoid this
strategy; its measured 1.02x difference is within the scope of run variation.
Broad OR had variable gains of 1.05x to 1.61x across the four blocks. This
experiment supports enabling the owner path for dense multi-term deltas in a
controlled qualification, not enabling it by default for every workload.

The direct PR-versus-main gain was not measured in this run. Earlier main
numbers used another fixture and sampling session, so they are not a paired
comparison. The separate owner-off/on archives retain fresh, unrelated-write,
related-write, and rebuilt stage results with their GIN controls; their changing
GIN CPU measurements are why the same-backend follow-up was added.

## Write and storage cost

The default-off read gate introduces no new on-disk format or WAL record. In
the one-batch fixture of 1,000 inserts plus VACUUM, the two PR runs recorded
identical cluster WAL totals: 3,869,752 bytes for PIN and 944,392 for GIN.
Their index sizes were 368,640 and 147,456 bytes, respectively. Write plus
maintenance backend CPU varied with the host: PIN/GIN was 131.9/60.3 ms in
the owner-off run and 175.2/83.2 ms in the owner-on run. These CPU numbers
support roughly 2.1x to 2.2x higher PIN cost in this small fixture. They do
not isolate steady-state insert or update throughput.

## Assessment and remaining gates

PR #21 removes a measurable amount of CPU from the 4,096-related-write
multi-term path. Selective AND reaches approximately GIN CPU on the paired
fixture, while rare-term search remains slower. The path still reads owner
payloads and emits ordinary PostgreSQL bitmap TIDs. It does not establish a
TIN-like compact mutable segment, index-only count, ranked top-k, or large-scale
search performance.

Keep the new GUC default off while measuring the 512-incarnation threshold,
small and large deltas, longer documents and fragments, concurrent reads and
writes, memory pressure, p95/p99 latency, and write amplification. Profile
owner-page reads, membership parsing, bitmap emission, and heap work before
choosing a persistent frontier layout. Compare against GIN and TIN on the same
corpus and hardware before making a cross-engine performance claim.

## Raw evidence

`owner-off-evidence.tar.gz` contains the full isolated runner evidence,
including source archive, extension binary, build and PostgreSQL logs, SQL,
plans, row-identity checks, per-sample CPU, write/WAL/size data, and manifests.
`owner-on-evidence.tar.gz` contains the same driver outputs and local cluster
configuration for the fresh owner-on run. `same-backend-toggle.tar.gz` contains
the exact paired script, full per-block measurements, and run log. Archive
hashes are in `SHA256SUMS`. Raw cluster data directories are not archived.
