# G4 streaming query execution

Implemented on G3 base `03cb582b66eb35806c5c1f8ce2b4ade536850851`.
Qualification and review are tracked in [PR #8](https://github.com/YogeshPandar/PIN/pull/8).
No on-disk format, WAL, lock order, dependency or compiler pin changes are required.

## Scope

`mutable::scan_query` streams necessary query conditions from mutable and sealed
posting chains. The PostgreSQL bitmap callback uses it inside the existing G3
shared structural barrier. All candidates retain the mandatory heap recheck.

The standalone technical blueprint and the ZIP mentioned in the request were not
available in this session. G3's repository documentation also records that absence.
This file defines the implemented G4 work explicitly; it does not assert completion
of unknown original roadmap requirements.

| Query | Posting operation | Remaining heap work |
| --- | --- | --- |
| `a AND b` | Intersect both owner streams | MVCC and exact predicate |
| `a OR b` | Merge both streams, deduplicate owners | MVCC and exact predicate |
| `a OR (a AND c)` | Independent cursors for each active term occurrence | MVCC and exact predicate |
| `"a b"` | Intersect required terms | Order, adjacency, MVCC |
| `"a a"` | Required term only | Multiplicity, adjacency, MVCC |
| `a AND NOT b` | Stream `a` | Negation and MVCC |
| `a* AND b` | Stream `b` | Prefix and MVCC |
| `NOT (a OR b)` | Stream the owner universe, including empty documents | Negation and MVCC |

Never subtract an approximate set. Prefix expansion and positional verification
inside the posting executor are not implemented. The existing scalar matcher
retains exact semantics. Multiple PostgreSQL scan keys still select one
conservative cover; PostgreSQL rechecks all index conditions against the heap.

## Cursor and ordering contracts

Ordering uses stable owner `(page, slot)` coordinates, not reusable posting-page
numbers or heap TIDs. Equal coordinates must have equal incarnations. Consecutive
owners in one stream must increase in both coordinate order and incarnation,
including the dictionary-first owner and page transitions. Only final candidates
load canonical owner pages and check publication, liveness and exact identity.

Repeated terms in different Boolean branches have independent cursor positions.
Sharing a mutable cursor between `a` and the nested `a AND c` branch would let the
intersection skip rows required by the outer union. Direct duplicate operands
can simplify before cursor assignment; unrelated branches cannot share state.

Each active cursor owns at most one encoded page. `OwnedPostings` resumes the
existing checked decoder using byte offsets and integer state. It retains no
self-referential slice, raw pointer or result-sized owner array. AND advances to
common owners; OR returns the minimum owner. The flat continuation stack avoids
recursive evaluation and recursive destruction, including deeply nested queries.

A private canonical owner-page copy can predate a concurrent append. A posting
whose slot exceeds that copy's owner count triggers one fresh page read. A slot
still missing after refresh remains an error. Existing identity mismatches are
never silently refreshed away.

## Memory and work bounds

The budget charges actual capacities for plan nodes, temporary maps, reachability
flags, cursor storage and continuation scratch. Each cursor includes its private
page storage; the input query, fixed stack page scratch, allocator overhead and
host-owned bitmap are outside this budget. Temporary planning allocations are
charged conservatively for peak usage. PostgreSQL currently supplies the existing
1 MiB query-execution budget.

If cursor planning exceeds its budget, all partial state is dropped before the
existing `CandidatePlan` term-cover fallback runs. Fallback can emit more
candidates or duplicates; the host bitmap deduplicates TIDs and rechecks them.
Fallback never truncates matches and never retries after I/O or output has begun.
A budget too small even for the fallback returns an error.

All executor vectors are reserved before scanning. Candidate iteration uses no
result-sized Rust collection or per-candidate allocation in this executor; host
I/O and the caller's sink have their own allocation contracts. Each active
occurrence decodes postings incrementally rather than rescanning page prefixes.
Page validation still checks the complete loaded payload before traversal.

The reader captures each posting tail and a relation-size traversal bound under
the shared structural barrier. Compaction takes the exclusive barrier before the
writer lock. Ordinary inserts may append after capture; newly inserted rows may
appear or be omitted, but pre-existing rows cannot be lost or duplicated by that
append. Host cancellation is checked on page I/O and every 256 interpreter steps.
The barrier ends before PostgreSQL fetches and visibility-checks the heap tuples.

## Usage

```sql
CREATE INDEX articles_pin ON articles USING pin(body);
SELECT id FROM articles
WHERE body OPERATOR(pin.@@@) pin.parse_query('rust AND postgres');

SELECT id FROM articles
WHERE body OPERATOR(pin.@@@) pin.parse_query('"zero copy" OR (rust AND NOT java)');
```

These use the existing operator and query format. Planner choice remains cost
based; qualification explicitly checks that a bitmap index plan was selected.
No index-only, ordered, parallel or non-MVCC scan capability is enabled.

## Review and qualification

`g4_query.rs` covers 393 generated query forms against an independent document
oracle, exact positive Boolean results, repeated clauses, phrase lossiness,
owner identity versus reused heap coordinates, 4,000-row sealed/mutable mixtures,
VACUUM/compaction, bounded fallback, corruption, cancellation and deep trees.
`g4_interleaving.rs` checks append/cache refresh, captured-tail stability and a
genuinely missing owner slot using deterministic I/O interleavings.

The fixed I/O fixture has 2,001 documents containing `a`, only one containing `b`.
For `a AND b`, the old cover emits 2,001 candidates; the new intersection emits
one and reads one canonical owner page. `a AND missing` reads no owner pages.
This is a deterministic work-count assertion, not a latency or throughput result.
It does not imply fewer posting reads for every distribution or query shape.

At code head `6020d5bc50288c6dc029d76f974284a22db83756`, G0 boundary run 179
passed all 80 non-ignored Rust tests, core Clippy with warnings denied, rustdoc,
and 16 Python tests. Three pre-existing Unicode conformance tests were ignored
by that job; the separate G1 workflows were skipped. One host closure formatting
difference remained and is corrected in the following checkpoint. Consult the
PR for final-head formatting and PostgreSQL job results, not these earlier counts
as evidence that a later revision passed.

`tests/sql/g4_queries.sql` runs through the existing PostgreSQL qualification
harness. It checks sequential/bitmap equality, NULL and empty input, phrases,
Boolean overlap, savepoint rollback, own writes, updates/deletes, multi-key heap
rechecks, and sealed/mutable tails. G4 results are checked after the ordinary
restart and again after the crash matrix. These tests need the pinned PostgreSQL
18.6 CI environment; no Rust or PostgreSQL toolchain is installed in the editing VM.

Self-review checked decoded queries (which reparse source into a tree), independent
cursor ownership, vector bounds, canonical refresh, error propagation and the
unchanged host recheck path. Independent concurrency/unsafe-boundary review and
representative warm/cold-cache latency, throughput and memory measurements remain
release gates. No bare-PostgreSQL or Tin performance parity is claimed.

## Official contracts

- [PostgreSQL 18 scanning](https://www.postgresql.org/docs/18/index-scanning.html)
- [PostgreSQL 18 locking](https://www.postgresql.org/docs/18/index-locking.html)
- [Pinned bitmap dispatch and statistical count](https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/backend/access/index/indexam.c#L757-L783)
- [Rust 1.98.1 fallible reservation](https://doc.rust-lang.org/1.98.1/std/vec/struct.Vec.html#method.try_reserve_exact)
- [Rust 1.98.1 checked slices](https://doc.rust-lang.org/1.98.1/std/primitive.slice.html#method.get)
- [Rust conditional chains](https://doc.rust-lang.org/reference/expressions/if-expr.html#let-chain)

The entry in [api-evidence.md](api-evidence.md#g4query01-streaming-bitmap-query-execution)
records the material-boundary obligations and review status.
