# Exact predicates through PostgreSQL's bitmap executor

## Measured problem

The first local common-term plan spent about 3.8 ms producing 15,000 index
candidates and about 208 ms in the bitmap heap phase. The previous streaming
predicate change improved repeated common-term throughput by about 10%, leaving
most of the gap to GIN. Inspecting the AM showed why: pin_bitmap_add passed
`recheck = true` for every root. Each visible matching document was parsed and
normalized again, even when term membership was already established by postings.

PostgreSQL's index AM contract distinguishes candidate membership from exact
predicate membership. Both still use PostgreSQL heap visibility. The executor
can skip repeating an exact predicate without skipping MVCC, HOT resolution,
residual filters, projection or row security. The upstream GIN text-search
consistent function likewise distinguishes definite from possible matches.
Pinned source references and reviewed boundaries are in API evidence AM02.

## Implemented proof propagation

1. The existing term cursors compare complete normalized dictionary terms and
   combine postings by canonical owner identity, not bare heap coordinates.
2. A successful positive term/AND/OR plan establishes membership for that query.
   On ordinary pages, resolving its owner validates the incarnation and
   published/live state. Direct sealed pages use a coordinate and local live bit
   copied from a published owner during compaction; VACUUM clears it before reuse.
3. The pure scan emits the root and its predicate-recheck obligation together.
   Its existing callback API remains conservative and discards the proof.
4. Phrases, prefixes and negation remain candidates. Any memory-budget fallback
   also discards proof before emitting, because a necessary-condition cover is
   insufficient to prove AND membership.
5. The AM forces rechecking whenever it selected only one of multiple scan keys.
   Null keys still produce no matches. Plain index scans are unchanged.
6. The bitmap sink keeps homogeneous batches and forwards the obligation to
   tbm_add_tuples. PostgreSQL retains recheck requirements when combining bitmaps
   or replacing exact tuple offsets with lossy page entries.
7. Core checks heap visibility with the statement snapshot. There is no new VM
   access, no heap-free count, and no separate transaction-visibility system.

`pin.enable_exact_bitmap` defaults on. Setting it off forces the previous
predicate rechecks, providing an operational fallback and same-binary ablation.
It is a SUSET execution control, captured once per bitmap sink, and changes no
SQL meaning or disk format. Existing indexes need no rebuild for this change.

## Correctness obligations

A published posting can belong to an uncommitted or aborted insertion; it is
still subject to the ordinary heap snapshot check. An owner liveness bit is not
an MVCC visibility fact. VACUUM and structural lifetime protocols are unchanged.
HOT successors cannot change the indexed value; indexed-value changes create
new indexed versions. For asynchronous bitmap scans, PostgreSQL requires an
MVCC snapshot so a subsequently reused heap slot cannot become an older visible
row. Pin's existing scan validator enforces that requirement.

The pure differential matrix checks every certified root against the document
oracle. It includes repeated terms and overlapping Boolean trees. The forced
256-byte AND fallback deliberately emits a false-positive candidate and must
mark it for recheck. Existing tests cover owner incarnations, sealed/mutable
sources and compaction.

`tests/sql/exact_bitmap.sql` checks actual row identities and function-call
counts, not just EXPLAIN's always-present Recheck Cond label. It requires zero
predicate calls on eligible exact pages and positive calls for approximate
queries, multi-key scans, forced rechecks and actual bitmap lossification.
Updates, rollback, VACUUM and replacement inserts exercise the host boundary.
The broader recovery, parallel, snapshot and RLS suites remain required.

## Where this fits relative to TIN

[TIN's architecture](https://planetscale.com/blog/introducing-tin) emphasizes
avoiding work with physical tuple identifiers, page-oriented posting operations
and specialized execution. This change removes repeated document processing
using PostgreSQL's existing executor contract. It does not implement TIN's
index-only count strategy or establish parity with TIN's measurements.

The next bottleneck must be measured after eliminating rechecks. Likely areas
for investigation are owner-page decoding, per-posting calls, bitmap construction
and the cost of producing every tuple for count queries. A direct count path
must separately prove exact predicates and current visibility, with a heap
fallback for uncertified pages. Page-group counting should avoid enumeration
only where that proof holds. SQL top-k needs visible, residual-filter-eligible
rows before raising its pruning threshold; oversampling is not an exact solution.

The existing count/VM controls remain experimental. New ranking, new visibility
shortcuts, shared-payload compaction and unsafe operations retain their separate
review and qualification gates. No TIN or world-fastest claim follows from a
single small corpus.
