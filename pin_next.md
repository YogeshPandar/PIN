# PIN: the next storage and execution architecture

**Design baseline:** `e0432821d939b1eac873dff3b4a2ff06a3aa6df1`  
**Research and review date:** 26 September 2026  
**Target:** PostgreSQL 18.6, the repository's pinned Rust/pgrx toolchain, native heap tables, initially the existing UTF-8 and 8 KiB-page envelope.  
**Deliverable status:** researched architecture and implementation specification, not an implementation, native qualification run, or measured TIN comparison.

**Reading map:** [Decision](#0-the-decision) · [Measured baseline](#1-establish-the-actual-baseline-before-redesigning-it) · [TIN feature target](#2-define-the-tin-target-precisely) · [Storage](#4-storage-replace-the-wrong-unit-of-allocation) · [Ranking](#7-ranking-implement-the-missing-execution-strategy) · [MVCC](#8-postgresql-visibility-retain-the-proof-replace-the-global-bottleneck-carefully) · [Implementation sequence](#13-concrete-implementation-map-and-release-sequence) · [Migration](#14-format-migration-and-operational-compatibility) · [Adversarial review](#15-adversarial-review-what-was-rejected-and-what-replaced-it) · [Qualification](#16-required-correctness-and-lifecycle-qualification) · [Benchmarks](#17-measurement-plan-distinguish-a-faster-engine-from-a-different-experiment) · [Sources](#19-source-register).

**Companion files:** `PIN_architecture_audit.py` and `PIN_architecture_audit.json` reproduce the evidence calculations and small design models described in section 15.2.

## 0. The decision

PIN should stop evolving primarily as **canonical owner/posting chains with an increasingly capable grouped accelerator attached**. Its destination should be **a packed, positional, CTID-native primary index, with a bounded mutable component and three execution consumers: rows, exact counts, and ranked top-k**.

Do not accomplish that by deleting the owner-generation machinery or switching every experiment on. Preserve the invariants that make the current implementation credible, particularly complete-document publication, owner incarnations, generation-qualified Boolean evaluation, PostgreSQL visibility, and independent semantic oracles. Move lifecycle identity out of the common posting traversal, not out of the system.

The next implementation tranche should deliver four connected changes:

1. **Packed canonical postings and direct bulk construction**, eliminating the demonstrated page-per-small-list pathology while preserving the current correctness boundary.
2. **A genuinely selective positional read format**, separating membership, frequencies, document lengths, and positions. This is the foundation for complete indexed phrase/span execution and native ranking.
3. **Exhaustive SQL BM25 followed by proven block pruning**, with thresholds established only by visible, authorized, fully qualifying rows. Keep `ts_rank_cd` benchmarks as a separate control, not a function to silently replace.
4. **Short publication critical sections and bounded maintenance**, followed by a reviewed group-lifetime protocol that removes the current query-duration, index-wide count/writer interlock.

SIMD is a later multiplier on this architecture. It cannot compensate for reading the wrong representation, validating a whole document to answer a two-term question, scoring every matching heap value, or performing maintenance while holding an index-wide barrier.

The existing `TIN_CPU_ARCHITECTURE.md` and `docs/fts-roadmap.md` already identify several directions. This document extends them into representation choices, publication and retirement protocols, executor contracts, rejection cases, module changes, and acceptance gates. Phase labels in `pin_plan.md` are not evidence that those gates have passed. [E08] [E09] [E11]

### How to read the evidence

- **Observed** means present in the supplied source or recorded results.
- **Recomputed** means independently calculated here from supplied raw data.
- **Proposed** means a design to implement and test, not an existing PIN capability.
- **Conditional** means an optimization is correct only under the listed preconditions.

The counterexamples later in this document attack tempting redesign shortcuts. They are **not six newly discovered bugs in current PIN**.

## 1. Establish the actual baseline before redesigning it

### 1.1 Source identity and work performed

The uploaded `PIN-main (11).zip` has SHA-256:

```text
842fa1b3a764213fa9bd5c868009c838affdfcd5bd73331c1c976a3e2d5be212
```

Its archive comment identifies commit `e0432821d939b1eac873dff3b4a2ff06a3aa6df1`. The connected GitHub branch read returned that same `main` commit, the merge of PR #27. The archive is therefore an appropriate current design baseline, rather than the older revisions mentioned in prior conversations. Public source links below are pinned to this commit. [C00]

I inspected the storage writer, page and document codecs, grouped build/storage/frontier/query paths, ranking oracle, analyzer/parser, PostgreSQL AM and C boundary, grouped count implementation, repository design documents, and the relevant benchmark records. The accompanying audit script recomputes the newest phrase results and page census, verifies the newest raw-data ledger, and executes independent mathematical/counterexample models.

No Rust installation, extension compilation, PostgreSQL execution, TIN execution, crash test, or native concurrency test was performed in this review. Reanalyzing a committed run does not requalify the current merged binary.

### 1.2 What is already implemented

The current implementation has more than a basic inverted index. Avoid wasting the next phase rebuilding these capabilities under new names. [C01] [C05] [C06] [C07] [C08] [C09] [C13] [C14] [C16]

| Area | Actual baseline | Remaining architectural issue |
|---|---|---|
| Identity | Physical heap roots plus durable owner references/incarnations | Canonical membership and lifecycle identity still impose extra storage/traversal work |
| Ordinary search | Bitmap and plain scan infrastructure, exact Boolean/term cases, safe rechecks | The host boundary still matters; a bitmap API is not a count or score API |
| Grouped search | 256-page groups, offset masks, generation checks, liveness, sparse inline grouped entries | Grouped data supplements canonical chains; the reader still loads complete needed bitmap records |
| Mutable suffix | Term-addressed frontier, optional anchors, optional dense owner frontier | Read amplification depends on suffix density and maintenance state |
| Incremental sealing | Experimental immutable delta segments and a bounded segment directory | Sealing/rebuild work is still invoked under strong maintenance barriers |
| Phrases | Merged inline indexed-position proof, including a one-pass validator | Root-level short phrases only; fragmented payloads and more complex expressions need other paths |
| Counting | Opt-in grouped mask consumer, visibility-map checks, optional dirty-page visibility batching | Narrow SQL eligibility and an index-wide lifetime interlock |
| Ranking | Pure exhaustive BM25/statistics/top-k oracle | No integrated SQL-ranked execution or competitive block skipping |
| SIMD | General runtime-selected AVX2 bitmap kernel exists | Grouped fixed masks currently use scalar code; whole-query benefits are unestablished |
| Operations | WAL, VACUUM, recovery and parallel infrastructure with development qualification | Native SQL ranking, standby search and broader production qualification remain incomplete |

The relevant acceleration switches default off. The quoted wins below are not a promise about a default installation. The supported server/toolchain/encoding/page-size envelope is explicit in the README. [C01] [C08] [C09]

### 1.3 Latest phrase measurements, recomputed from raw batches

The one-pass run identifies binary commit `a3050fb8331ec16ae926f557fab3ad6bad9d9c06`, not the final merge commit. It uses six balanced blocks with twelve warm queries per mode per case. The values below are medians of recorded per-batch backend CPU per query. They are not latency percentiles. [E01] [E02]

| Query case | Prior PIN in this same run | One-pass positions | GIN in this same run | GIN / positions |
|---|---:|---:|---:|---:|
| `bravo charlie` phrase | 79.602 ms | 22.471 ms | 218.758 ms | 9.735x |
| `delta echo` phrase | 78.370 ms | 22.533 ms | 218.188 ms | 9.683x |
| Reversed `charlie bravo` phrase | 80.603 ms | 21.860 ms | 218.954 ms | 10.016x |

These controls are intentionally taken from the **same one-pass run**. The run README combines some columns from earlier experimental stages; those earlier prior-PIN medians are not the same paired samples.

The plans are ordinary bitmap heap plans. The improvement comes from proving predicate truth using indexed positions instead of reanalyzing heap text, not from replacing PostgreSQL's visibility check or using a custom aggregate. The fixture is a small synthetic corpus: 20,000 initial rows plus appended rows and lifecycle mutations. It establishes a promising mechanism, not production TIN parity. [E01] [E02]

The separate rebuilt PR #26 control remains important: [E04]

| Equivalent SQL workload | PIN grouped mode | GIN | Interpretation |
|---|---:|---:|---|
| Broad rows, ordered by `id` | 6.473 ms | 8.080 ms | About 1.25x; includes row delivery and ordering |
| Phrase count before indexed-position proof | 79.572 ms | 218.071 ms | Earlier recheck-heavy baseline |
| Top-k using the same `ts_rank_cd` expression | 272.029 ms | 273.169 ms | Essentially parity; both perform heap-side scoring |

This is why a universal statement that “PIN is already 10x” would be wrong, and why tuning the Boolean bitmap alone is not a ranked-search architecture.

### 1.4 The page census is the strongest storage evidence

The post-maintenance census at revision `3a3cbbb582b3f6caa25634223f6c65bb47d487ef` describes a 79,470,592-byte relation containing 9,701 PostgreSQL pages. [E05] [E06]

| Page class | Pages | Recorded useful PIN payload | What it means |
|---|---:|---:|---|
| Free | 4,252 | 68,032 bytes of free-page metadata | Reusable space, not automatically released to the filesystem |
| Sealed per-term postings | 4,137 | 536,489 bytes | About 1.58% payload occupancy relative to allocated page bytes |
| Owners | 605 | 4,846,357 bytes | Densely occupied; not the same small-list allocation problem |
| Dictionary | 512 | 270,412 bytes | Approximately 4 MiB of pages for a small amount of dictionary payload |
| Grouped | 194 | 1,418,572 bytes | Much denser than the per-term posting allocation |
| Metadata | 1 | 4,232 bytes | Small fixed overhead |

Free plus sealed-posting pages occupy **86.476% of physical relation pages** in that census. This does **not** mean 86.476% of query CPU is removable, or that every query reads those pages. Free pages need not be visited by search.

The fresh canonical-only index was 43,393,024 bytes, compared with 44,720,128 bytes for the fresh inline-grouped variant. Canonical storage, rather than grouped bitmap payload, dominates that particular fresh footprint. [E05]

The source explains the pathology: `writer::link_term` keeps the first owner in the dictionary, then allocates a dedicated posting page when a term gets another owner. Sealing continues to retain per-term chains. This is a page-granularity problem before it is a compression-codec problem. [C02] [C03] [C04]

### 1.5 Dirty-page visibility and maintenance are separate limits

In the final PR #25 rework run, fresh broad AND count measured 227 microseconds for PIN versus 2,675 for GIN. After updates/deletes, the corresponding result was 816 versus 3,583. The selective fresh AND count was only 141 versus 181. Do not transfer the broad-count ratio to rare queries. [E05]

A same-binary visibility ablation on the mutated table measured broad AND at 1,447 microseconds with scalar visibility versus 465 with page batching, and broad OR at 1,869 versus 598. The updated AND plan needed only 29 PIN index pages and 111,122 index payload bytes but checked 14,128 dirty-page roots, out of 18,072 candidate roots across 652 heap pages. [E05]

Consequences:

- Reducing repeated buffer and visibility work can matter more than further shrinking already-small query payloads.
- A count win on all-visible data is not a guarantee under continuous writes.
- The global count interlock makes concurrent writer latency a required acceptance metric, even when serial CPU looks excellent.

### 1.6 Read the CPU profiles without overinterpreting them

The latest available phrase profile is the **first indexed-position implementation**, not the final one-pass implementation. Its flat samples include `phrase_matches` 13.99%, posting cursor seek 12.23%, complete document validation 11.78%, and position iteration 11.46%. These are sampled backend CPU shares, not instructions or a current-binary cost decomposition. Do not add inclusive call-tree percentages to flat symbol shares. [E03]

Older G9 measurements found material copying/validation costs and an expensive write path. They also found that much broad-query CPU was in PostgreSQL rather than the grouped evaluator. Those older profiles explain why the project moved toward exact predicate proof and custom consumers; they do not prove that the same percentages remain after later PRs. The VM rejected hardware performance counters, so the evidence contains no measured IPC, hardware cache-miss rate, branch-miss rate, or bare-metal cycle count. [E07]

One particularly valuable negative result: reducing 50,320 `lseek` calls to 200 over forty warm queries did not materially improve the paired phrase CPU measurement. The extra bounded C reader/cache boundary was reverted. Do not resurrect it solely because the syscall count looks bad. [E01]

## 2. Define the TIN target precisely

### 2.1 Published mechanisms, not access to TIN's private implementation

PlanetScale describes CTID-native postings, page/offset bitmaps, page-level Boolean pruning, per-segment liveness, visibility-aware custom execution, and mutable/immutable segments. It also describes transferring compatible immutable bitmap storage during merges instead of renumbering documents. These are useful architectural constraints, not a public specification of its exact codecs, locks, or executor implementation. Its published benchmark uses a much larger corpus and different environment than PIN's fixture; some GIN comparisons use `ts_rank_cd` while TIN uses BM25. Therefore no ratio here is a direct PIN-versus-TIN result. [T01]

The companion search-engine article supports separating positions and frequency data from membership and using score bounds to avoid scoring uncompetitive blocks. The PIN design below independently specifies how to attempt those mechanisms under PIN's existing invariants. [T02]

### 2.2 Feature coverage is broader than phrase search

This is a coverage checklist, not a claim that the named capabilities have identical semantics today. TIN's public language includes the following families. [T04]

| Capability family | PIN at the reviewed commit | Required destination |
|---|---|---|
| Terms, Boolean expressions, phrases | Present | Preserve existing dialect and oracle |
| Prefix completion | Present semantically; not grouped-native | Dictionary-driven grouped expansion |
| Match-all and minimum-match expressions | Not the complete TINQL surface | Explicit document universe and threshold Boolean IR |
| General wildcards, fuzzy matching, regex and term ranges | Not equivalent | Bounded vocabulary enumeration/automata |
| Phrase gaps, alternatives and slop | Not equivalent | Positional/span evaluation |
| Ordered/unordered proximity and enclosing/overlapping spans | Not equivalent | Composable span iterators and witness semantics |
| Position windows and query boosts | Not equivalent | Versioned positional and scoring contracts |
| SQL BM25 and full-score mode | Pure core oracle only | Statement-bound scoring and exact eligible-row top-k |
| Score inspection and maximum score | Not integrated | Explicit score-term selection and scan-level score context |
| Highlighting | Not integrated | Original-text witness mapping and rendering contract |
| Per-field search and combined scores | Not integrated as native ranked search | Field-aware plan binding, not a global CTID score cache |
| Tokenizer options and preview | One fixed profile | Versioned analysis configuration and inspection API |
| Concurrent build/reindex and replica search | Current restrictions apply | Separate DDL/replay qualification stages |

PIN currently accepts unary NOT. TINQL documents exclusion through AND NOT. Compatibility must be an explicit dialect or parser version; “becoming TIN-compatible” must not silently change existing PIN query results. [C12] [T04]

### 2.3 Ranking semantics can change the apparent performance result

TIN distinguishes default score, full score, maximum score, and score inspection. Its default dense-term threshold is 0.10; dense terms are omitted from scoring unless pinned, without changing matching. Full score keeps the query terms. Statistics describe documents still represented in the index, while returned winners obey query visibility. Its runtime scoring controls include term additions/replacement and parameter overrides. [T05]

For PIN, define **two named scoring policies**:

```text
pin_bm25_full_v1        all selected query terms, current exhaustive oracle semantics
pin_bm25_tin_compat_v1  separately tested term selection and parameter behavior
```

These names are proposed contracts, not existing functions. Do not label dense-term omission a transparent optimization of full BM25. It is a different scoring policy.

The existing `Bm25::new` rejects `k1 <= 0`, whereas the documented TIN domain includes zero. Accommodating compatibility requires a deliberate core/API change and boundary tests, not merely exposing the current constructor in SQL. [C14] [T05]

### 2.4 Analysis compatibility also requires a format decision

PIN's profile 1 is Unicode 16 NFC, simple default folding, NFC, then Unicode word segmentation. Its stored query encoding carries a profile identifier. TIN exposes selectable boundary, case/accent, long-token, grapheme, and position-gap behavior. These are not interchangeable token streams. [C15] [C12] [T06]

Proposed policy: retain profile 1 as-is; create a new explicitly versioned analysis profile with all options stored in index metadata. Query analysis must use the index's profile. A change that alters emitted terms, positions, or document length requires REINDEX. A query-time BM25 weight change does not require re-tokenization.

TIN's public documentation also distinguishes one text source per index, combined field scoring, per-partition statistics, and restrictions on some parent-level scoring shapes. PIN should publish equally explicit support boundaries rather than claim unqualified SQL coverage. Lead can help exercise compatible application semantics, but it is a table-scanning development extension, not a TIN performance stand-in. [T03] [T08]

The HN article additionally describes application-level query rewriting for prefix completion and staged fuzzy fallback. Implement that as an optional application/query-builder layer, not as an invisible change to the core meaning of an exact query. [T09]

## 3. The target architecture

### 3.1 One semantic plan, different consumers

```text
SQL expressions and query text
           |
           v
profile-aware parser + immutable semantic query IR
           |
           v
query planner: exactness, score policy, required columns, budget
           |
           v
leased manifest + mutable publication frontier + statistics epoch
           |
           v
term lookup / vocabulary expansion
           |
           v
segment/domain group cursors
  page summaries -> selected offset masks -> optional TF/positions
           |
           +----------------------+-----------------------+
           |                      |                       |
           v                      v                       v
        RowScan                CountScan               RankScan
  exact bitmap/stream       certified masks       bounds + candidate score
  heap fetch and quals      VM or heap batch      heap visibility + quals
  ordinary projection       exact aggregate       eligible-row top-k
           |                      |                       |
           +----------------------+-----------------------+
                                  |
                     normal PostgreSQL upper execution
```

The shared semantic IR defines the predicate. A physical execution plan chooses representations and late materialization. The consumer determines whether individual TIDs, row slots, score values, or only cardinalities are required.

A row scan must not materialize a full result bitmap when a streaming plan is cheaper. A count must not expand masks merely to increment an aggregate transition once per tuple. A ranked scan must not evaluate positions or document text for a block that cannot contain a qualifying winner. However, a top-k score bound never authorizes skipping rows needed by an exact count.

### 3.2 Separate four identities

Use distinct types and on-disk meanings:

```text
RelationGeneration   physical relation incarnation; changes on replacement/rebuild
ManifestGeneration   which searchable components a reader leased
DocumentDomain       cohort in which a root coordinate identifies at most one document
DocumentHandle       durable incarnation of one indexed root version
```

A heap root coordinate is `(block, offset)`. A tuple slot's current `ctid` may identify a HOT child instead of the indexed root. Neither coordinate alone is a globally durable document identity. [P03] [P06]

An immutable merge may change the manifest without changing a document handle. A reused heap coordinate must get a different handle/domain. This separation permits compact physical-coordinate postings while retaining enough identity to make VACUUM, old readers, and later reuse safe.

### 3.3 Retain the current pure-core boundary

`pin-core` should own codecs, semantic plans, Boolean and positional evaluation, score math and bounds, and abstract state-machine tests. It should not contain PostgreSQL pointers or snapshots.

`pin-pg` should own relation and manifest leases, resource lifetime, buffer I/O, WAL publication, snapshots, table-AM calls, CustomScan integration, locks, workers, and SQL permissions. The C shim should remain narrow and batch-oriented where a pinned upstream PostgreSQL operation is needed. [C17] [C11] [E10]

Do not grow a second buffer manager, standalone filesystem segment store, or independent MVCC implementation merely to make the search kernel easier to write. Each would add operational and correctness obligations that the current native-relation architecture avoids.

## 4. Storage: replace the wrong unit of allocation

### 4.1 First bridge: packed canonical postings

The quickest evidence-backed storage change is not a complete new engine. It is replacing the dedicated-page promotion in `mutable/writer.rs::link_term` and the corresponding sealed representation. [C02] [C04]

Proposed record classes:

| List shape | Representation | Promotion rule |
|---|---|---|
| One or a few owners | Inline dictionary payload | Promote by encoded bytes, not a universal document count |
| Small/medium owner list | Record in a shared slotted posting arena | Move to a larger record class or chained chunk when capacity is exhausted |
| Large mutable list | Append-friendly bounded chunks | Seal into immutable physical-coordinate groups |
| Sealed sparse list | Packed coordinates plus skip metadata | Switch codec only when encoded size and query cost justify it |
| Dense sealed group | Page mask and per-page offset containers | Keep TF and positions in separate streams |

Keep a **stable posting handle**. A handle must not become invalid when an arena compacts or promotes a record. Options are a stable slot/indirection entry with a generation or an atomic dictionary-reference replacement under the existing writer barrier. Do not keep external references to raw byte offsets that can move.

The current grouped term key incorporates the canonical dictionary page and offset. Moving dictionary entries would invalidate those keys. The bridge therefore either preserves dictionary entry locations or rewrites the affected references as a generation publication. The eventual primary format should use segment-local logical term ordinals, not arbitrary physical dictionary addresses. [C05] [C06]

Use encoded-byte occupancy and overflow histograms to choose inline and arena thresholds. “Eighteen worked for grouped entries” is not evidence that eighteen is the correct canonical threshold. Small-list packing should be tested across realistic vocabulary tails, not only repeated alpha/bravo fixtures.

### 4.2 Make direct construction the normal bulk build

Current grouped construction derives data from already-written canonical structures. That is useful for migration but need not be the steady-state build architecture. [C08] [C19]

A new-format build should emit sortable records from the heap scan directly:

```text
(profile, normalized term, heap group, root coordinate,
 document handle, term frequency, position payload reference)
```

PostgreSQL-managed external sorting produces term/group runs. A second grouped stream builds document lengths and lifecycle records once per document. The finalization pass writes packed dictionaries, posting descriptors, TF/position extents, liveness, and statistics, then publishes the completed manifest.

Budget the sort and codec work across all workers, not `maintenance_work_mem` independently for each worker. Spill through PostgreSQL-managed facilities. Keep build cancellation and aborted-build cleanup within the existing host contracts.

Do not materialize the expensive legacy posting chains merely to immediately discard them. Conversely, do not skip creating the data needed by a promised legacy fallback. Format capabilities must say which representation is authoritative.

### 4.3 Primary immutable layout

Proposed logical components:

```text
IndexMeta
  format/version/capabilities
  relation-generation and analysis-profile fingerprint
  manifest-root and publication generation
  allocation/recovery roots

Manifest
  immutable component descriptors
  active/frozen mutable descriptors
  statistics-epoch descriptor
  current + retained liveness registrations

TermDirectory
  normalized lexeme -> term ordinal and group-run descriptor
  document-frequency/statistics metadata

GroupDirectoryEntry(term, document-domain, heap-group)
  page-presence summary
  count-validity flags and cardinality summaries
  membership container location and codec
  frequency-block and position-directory references
  optional score-bound metadata reference

DocumentMetadata(document-domain, heap-group)
  root membership / lifecycle stamps
  exact document lengths
  liveness and retirement generation

PayloadExtents
  membership containers
  term frequencies
  positional blocks
  optional original-text mapping checkpoints
```

These are logical schemas. Field widths and byte offsets must be frozen only after codec round-trip, corruption, overflow, and promotion tests. The first implementation should not persist native Rust struct layouts.

Use explicit endianness, checked offsets, typed identifiers, reserved flags, record lengths, codec versions, and alignment-independent readers. A format header must identify enough of the analyzer and heap layout to reject incompatible readers.

### 4.4 A group is not a physical page

Keep 256 heap pages as the first logical grouping candidate because PIN already has that coordinate model. It is not a requirement that an entire group record fit in one PostgreSQL page.

The current maximal bitmap buffer is `72 + 256*68 = 17,480` bytes. A scan allocates term workspaces proportional to that size and then reads the complete needed record before selected offset access. With 64 terms, the bitmap buffers alone can exceed a megabyte. [C05] [C06]

The new directory needs **addressable per-page or small multi-page sub-blocks**. It must be possible to reject 255 heap pages and fetch only the membership data for the one survivor, rather than copying the complete term/group record first.

Proposed hierarchy:

```text
term/group summary: 256 page bits
    -> compact directory of present heap pages
        -> per-page membership container
        -> matching TF slice
        -> per-document position-list reference
```

A sparse page uses sorted offsets or a singleton. A dense page uses a mask. Short runs may use a run encoding. Choose using actual encoded bytes and measured access patterns; do not require conversion of every sparse list to a 512-bit mask.

The internal 512-bit offset container is capacity, not permission to index 512 valid tuple offsets on an 8 KiB heap page. Mask unused bits and validate against the header-derived PostgreSQL maximum exposed by PIN's ABI checks. [C01] [C16]

### 4.5 Separate hot and cold information

The current inline document contains all normalized terms and positions. Proving one phrase requires walking a complete validated document representation. [C13]

The destination should permit:

- Membership-only AND/OR/count without positions or document text.
- BM25 from exact TF and document length without parsing position deltas.
- Phrase/span evaluation from the selected terms' positional blocks only.
- Highlighting from selected visible output rows, not all candidates.
- Lifecycle identity lookup once per relevant document/group, not once per term posting.

Do not duplicate the original text by default. PostgreSQL already owns it, including TOAST. For highlighting a small top-k, reanalyzing only the returned texts can be a good first implementation. An optional original-offset mapping is a later storage/performance tradeoff, not a requirement for Boolean speed.

### 4.6 Dictionary design

The fixed 512-bucket linked dictionary is a poor destination for vocabulary range operations. A lexicographically searchable dictionary is required for efficient prefix, range, wildcard and fuzzy expansion. [C02] [C03] [T02]

Use a block-compressed sorted dictionary with restart points and a small sparse index for immutable components. Compare that with an FST only after measuring vocabulary lookup, footprint, build cost, and merge complexity. Do not adopt an FST because search engines often use one.

Mutable components need durable exact lookup and ordered enumeration. A modest ordered page tree plus packed posting records is a reasonable first target. Retain the old hash dictionary during the packing bridge if that avoids mixing two risky migrations.

Avoid a globally contended “increment corpus DF” write for every term occurrence. Publish exact physical-corpus statistics in coherent epochs assembled from immutable summaries plus a captured mutable summary. Query term lookup across components must be budgeted; hundreds of tiny runs cannot be treated as free.

### 4.7 Metadata is worth space only when it removes work

Store exact raw membership counts, per-page counts where useful, and raw bound inputs. But attach validity:

```text
membership_count
liveness_generation / clean_since_retirement
coordinate-domain identifier
statistics epoch or raw-statistics provenance
```

A raw posting count is not a snapshot-visible SQL count. A page-presence summary is not an exact NOT result. A maximum score under yesterday's `k1`, `b`, or IDF is not necessarily a valid bound today. Later sections specify the certificates that allow those values to skip work.

### 4.8 Packing versus no-copy merge

Dense packing and extent transfer can conflict. A PostgreSQL page containing small records with unrelated lifetimes cannot be freed when just one record becomes obsolete.

Start with immutable allocation classes that group similar merge/lifetime cohorts. Track ownership at a practical extent/page granularity, and measure retained slack. Transfer extents only where lexeme interpretation, document domain, liveness ownership, and payload encoding remain valid. Rewrite overlapping, mixed-incarnation, or heavily dead groups.

Separating a logical segment from an immutable payload extent helps, but it does not make every merge zero-copy. Reducing write amplification is the objective; claiming universal zero-copy is not.

## 5. Query execution: avoid work before accelerating it

### 5.1 Compile semantic truth separately from physical traversal

The current grouped compiler accepts a bounded Boolean subset and sends prefix/phrase cases elsewhere. Retain its independent correctness role, but replace the idea that every query is one identical mask program. [C05] [C12]

Proposed physical operators:

```text
Empty
ExactTermCursor
SparseLeadConjunction
DenseGroupBoolean
VocabularyExpansion
PositionFilter
SpanFilter
ComplementWithinDocumentUniverse
AtLeastN
```

Each operator reports whether its output is exact membership or a conservative candidate set. Its cost includes dictionary startup, component fanout, group descriptors, payload bytes, expected surviving roots, and required late data. These operators are implementation concepts, not a request for a general-purpose optimizing compiler.

A first useful planner can recognize just three common shapes: a rare exact term; a selective conjunction driven by its rarest term; and dense Boolean expressions driven by page masks. Add more shapes only with measured crossover evidence.

### 5.2 Sparse conjunctions need a different path

The current sparse shortcut is specifically a small single-term case. A query such as `very_rare AND very_common` should not automatically pay the same workspace and descriptor costs as a dense conjunction. [C05]

Proposed procedure:

1. Choose the smallest plausible positive lead using physical DF/run summaries.
2. Iterate its root/page coordinates in order.
3. Batch probes for other terms by heap group and page.
4. Evaluate the whole predicate within the same document domain.
5. Fetch position data only for candidates that survive membership.

Use binary/galloping seek for large skew, linear merge for similar sparse lengths, and dense masks where local density makes that cheaper. A query-time policy must account for component fanout and mutable cost, not just total DF.

DF guides work; it does not establish visibility or exact selectivity. Correlated terms can make independent-selectivity estimates very wrong. Start with conservative estimates and instrument actual survivors at every stage.

### 5.3 Page-level AND is safe; page-level NOT is not subtraction

For a conjunction, intersect page-presence masks to find pages worth inspecting. For a disjunction, union them. For a difference, retain the left candidate pages: the two terms can occur on the same page at different offsets.

Likewise, approximate truth is not Boolean truth. Suppose a phrase candidate set contains documents that do not actually satisfy the phrase. Complementing that candidate superset discards valid NOT matches. Use a three-valued representation such as definitely true / definitely false / needs exact evaluation, or conservatively retain the universe until the negative predicate is resolved. The current kernel already recognizes the page-summary difference restriction. Preserve it. [C16] [P11]

An exactness label must survive planner transformations. It cannot be lost because a candidate iterator happens to use the same Rust type as an exact iterator.

### 5.4 Stream the result, not the representation

For row consumers, emit bounded batches of `(root, exactness)` or exact page masks to a PostgreSQL adapter. Use ordinary `TIDBitmap` only when that is the chosen plan, including its lossy-page recheck contract. A custom row scan may retain a page-local mask and produce slots incrementally.

For counts, retain masks to the last possible moment. For rank, maintain a competitive candidate structure rather than sorting every match. These must be separate consumers of the same predicate, not separate implementations of query semantics.

A useful abstract interface is:

```rust
// proposed interface sketch, not compilable repository code
trait GroupSource {
    fn seek_group(&mut self, at_least: HeapGroup) -> Result<Option<GroupHeader>>;
    fn membership(&mut self, term: TermId, page: HeapPage,
                  scratch: &mut PageScratch) -> Result<MembershipView>;
    fn frequencies(&mut self, term: TermId, selected: &RootMask,
                   scratch: &mut FrequencyScratch) -> Result<FrequencyView>;
    fn positions(&mut self, term: TermId, document: DocumentHandle,
                 scratch: &mut PositionScratch) -> Result<PositionView>;
}
```

Do not return references into an unlocked PostgreSQL buffer. Views must be owned scratch slices or remain within an explicitly bounded locked callback. A type name is not a lifetime proof.

### 5.5 Make “lazy” measurable

Add counters for:

```text
groups_considered, groups_skipped
heap_pages_considered, heap_pages_surviving
membership_bytes_read, membership_bytes_decoded
frequency_bytes_read, position_bytes_read
whole_document_validations, owner_resolutions
candidate_roots, exact_roots, heap_visibility_checks
```

For `rare AND common`, verify that common-term offset bytes decrease with the surviving page set. A counter showing fewer logical offsets evaluated is insufficient if `read_bitmap_view` still reads the same full records. The current source makes that distinction concrete. [C05] [C06]

## 6. Indexed positions: finish the work that already produced a large gain

### 6.1 Preserve the merged fast path as an oracle and bridge

The new inline phrase proof is valuable and should not be discarded. It demonstrates that avoiding repeated heap analysis materially changes whole-query CPU. Preserve it behind its existing correctness envelope while adding a format that makes selected position lists directly accessible. [E01] [C13]

Do not characterize indexed phrases as entirely missing. The gap is **coverage and access locality**: fragmented documents, nested Boolean/phrase expressions, extended positional semantics, and avoiding unrelated-document validation on every candidate.

### 6.2 Proposed positional record

For each `(term, document handle)` store exact TF and a location for its monotone position list. Group position blocks by term and nearby heap coordinates, with bounded restart points. A document with a long list can span blocks without forcing a full-document text fallback.

The reader validates a bounded directory entry and the blocks it consumes: identity, lengths, counts, restart bounds, monotonicity, checked arithmetic, and position-domain limits. Whole-index verification remains an explicit maintenance/diagnostic operation.

Moving from “validate every unrelated field in this document on every query” to “validate every consumed structure and offer a full verifier” is an observable corruption-detection policy change. Document it. Do not simply disable checks and call the removed work an optimization.

### 6.3 Exact phrase algorithm

For a phrase of terms `t0 ... tm-1`, a match exists when there is a start position `p` with occurrence `p+i` in every term's positional list.

Choose a short position list as an anchor, translate its positions into potential starts, and seek the other lists monotonically. Repeated terms require distinct occurrence positions satisfying the offsets, not merely presence of one term. A phrase with alternatives or explicit gaps changes the offset constraints; it must not be implemented by an unordered intersection of position sets.

For Boolean phrase existence, stop after the first witness. For highlighting or span composition, enumerate the required witnesses under a separate budget. Do not materialize all pairwise spans when an existence answer suffices.

### 6.4 Span IR

Represent a span as a half-open token interval plus provenance identifying the query parts and original-text mappings needed by the consumer. Compile phrase/proximity/containment operators into composable ordered iterators.

The exact definition of distance, overlap, repeated occurrences, percentage rounding, and token gaps belongs in a versioned semantic test corpus. An efficient merge over span endpoints is useful only after those rules are fixed.

Worst-case span output can be much larger than the number of input positions. Existence evaluation can often avoid this; witness enumeration cannot always do so. A resource limit must produce an explicit error or a documented exact slower path, never silently omit matches.

### 6.5 Highlighting is not a cheap substring replacement

Normalization can change byte lengths and combine or split characters. Token positions in normalized text are not byte offsets into the original PostgreSQL text value. Build original-range mappings during analysis or reanalyze only selected output rows with the identical profile.

The output renderer needs an explicit escaping contract, overlapping-witness merge policy, byte/character boundary handling, and bounded output size. A plain “wrap every matching term” algorithm is not sufficient for positional witness highlighting. TIN also documents query-context inference and ANSI output. [T10]

There is a conformance question in the public highlighting page: the prose about `BEFORE` and its example do not make the set of wrapped witness tokens equally clear. Resolve such cases using runnable reference behavior and record the result; do not invent a private interpretation and call it exact compatibility. [T10]

## 7. Ranking: implement the missing execution strategy

### 7.1 Start with exhaustive SQL BM25

The pure `rank.rs` implementation already computes BM25 over an immutable statistics epoch and caller-approved candidates. Use that as the independent score/top-k reference. [C14]

First integrate an **ExhaustiveRankScan** that:

- obtains one coherent statistics epoch and score-term policy;
- evaluates exact predicate membership;
- resolves PostgreSQL snapshot visibility and the correct HOT tuple;
- evaluates required security and residual qualifications;
- scores every eligible row with exact stored TF/length;
- selects top-k with the specified secondary ordering;
- returns ordinary PostgreSQL slots carrying the score.

This first path may not be fast enough. Its purpose is an executable native oracle for the next optimization. A faster scorer that selects invisible rows is not an intermediate success.

The existing `ts_rank_cd(to_tsvector(...))` comparison remains a useful equal-function benchmark. Native BM25 is a different ranking contract and must have its own exhaustive reference. No optimizer should rewrite the user's `ts_rank_cd` call into BM25.

### 7.2 Pin a coherent statistics epoch

For one scored statement, pin:

```text
StatsEpoch {
  relation generation,
  analyzer/profile,
  corpus document count N,
  corpus total length,
  df for every selected score term,
  component coverage / mutable publication cutoff,
  score policy and query-time parameters
}
```

Do not combine `N` from one manifest with DF from another. Do not let a compaction halfway through execution change IDF or average length for later rows.

A physical-corpus policy is practical: statistics can include stored dead versions until a defined maintenance transition, while result eligibility remains snapshot-exact. State this contract and expose the epoch through diagnostics. Achieving exact snapshot-specific corpus statistics for every transaction is a separate, much more expensive feature; it is not necessary to honestly describe a physical-corpus BM25 policy.

HOT updates that do not change the indexed expression do not create a new indexed document. Empty analyzed texts and NULL values need explicit corpus-membership rules. The existing core uses an average-length fallback for empty totals; preserve or deliberately version that behavior. [C14] [P03]

For a mutable component, capture summary coverage consistently with document publication. A term's DF counts documents containing it, not occurrences. Stats aggregation must not count a source component and its replacement twice.

### 7.3 Score and safe block bound

For selected nonnegative term weights, the proposed full-score baseline is the existing PIN formula:

```text
idf(t) = ln(1 + (N - df(t) + 0.5) / (df(t) + 0.5))

score(d,q) = sum_t boost(t) * idf(t)
                      * tf(t,d) * (k1 + 1)
                      / (tf(t,d) + k1 * (1 - b + b * dl(d)/avgdl))
```

The IDF shape and parameter roles are also documented by Lucene, but exact equality with another engine still depends on its length representation, selected terms, precision, and policy. [C14] [L01]

For a block `B`, store raw `tf_max(t,B)` and `dl_min(B)`. With nonnegative weights, `k1 >= 0`, and `0 <= b <= 1`, a conservative term bound substitutes maximum TF and minimum document length in the saturation expression. Sum the term bounds for a conservative block bound.

A subtle implementation issue: `tf_max` and `dl_min` may come from different documents, so `tf_max > dl_min` is possible. The existing exact scorer rejects `tf > length`. **Do not call that scorer on the synthetic bound pair.** Implement a separately validated bound function whose mathematical domain allows independent extrema.

Start with coarse bounds, then add finer sub-blocks or TF/length Pareto frontiers only when skip rates justify their space and maintenance cost. Poor but safe bounds cause extra work, not wrong answers.

### 7.4 Floating-point correctness is part of the algorithm

Real-number admissibility does not prove that a computed floating-point bound is conservative. Round upper bounds outward and competitive thresholds conservatively. A single arbitrary epsilon or unconditional one-ULP bump is not a general proof for a long arithmetic expression or score sum.

Use one of these explicitly reviewed strategies:

- interval arithmetic with outward rounding at each relevant operation;
- a demonstrated error bound around the exact implementation's operations;
- a safe scaled-integer bound scheme with upward-rounded maxima and downward-rounded thresholds.

Lucene's WAND implementation explicitly handles this direction of rounding; borrow the principle, not unreviewed Java code or its assumed numeric domain. [L02]

Keep uncompressed exact TF/length semantics in the first ranked format. Lossy norms or frequency quantization introduce another bound/score compatibility question and should not be mixed into the initial SQL ranking patch.

### 7.5 The threshold may include only eligible rows

The top-k threshold is established by rows that have passed **all conditions whose failure could remove them from the final result**.

Counterexample:

```text
k = 1
uncommitted row: score 100
visible row in a later block: score 90, block bound 90
```

Using the uncommitted row to establish threshold 100 causes the later block to be skipped, producing a wrong result after the invisible row is discarded. The same error occurs with an RLS-rejected row or an untested residual filter.

It is acceptable to compute a candidate score before heap visibility to avoid some heap fetches. It is not acceptable to let that candidate raise the committed competitive threshold until eligibility is established.

For `ORDER BY score DESC, id ASC`, an equal-score bound cannot be skipped just because `upper_bound <= threshold`. The block might contain a smaller `id`. Skip on a strict score inequality unless the bound also proves the secondary order cannot win. The first implementation should take the conservative strict-inequality rule.

### 7.6 A block-max executor that fits physical grouping

Proposed serial path:

```text
compile predicate and score terms
lease sources and one statistics epoch
initialize top-(offset + limit) heap

for a candidate document-domain/group in a score-aware traversal:
    read predicate summaries and score-bound metadata
    reject impossible predicate groups
    if top heap is full and the proven upper bound is strictly worse:
        skip scoring this group
        continue
    evaluate exact membership on surviving pages
    for a selected root:
        obtain exact TF/length and score
        if it cannot beat the eligible threshold:
            continue
        fetch visible heap version; apply all required quals
        if eligible:
            update top heap with exact tie ordering

sort retained winners; apply OFFSET/LIMIT; project output
```

Add WAND/MaxScore-style sparse disjunction traversal only after the block path agrees with the exhaustive native oracle. The same source iterator can expose `seek`, block maxima, and exact per-document score inputs. Not every query benefits from WAND; dense conjunctions and complex spans may have different best plans.

For all-zero scores, fall back to the required tie-order behavior. Do not turn a zero threshold into permission to return arbitrary rows when the SQL contains a secondary order.

### 7.7 SQL score binding must be explicit

A global `HashMap<ctid, score>` is wrong. The same CTID can occur in different relations, partitions, aliases, snapshots, and rescans. HOT can also make the emitted slot's CTID differ from the root used to find it.

Bind each score expression to a statement-local context identifying the range-table entry, indexed field/expression, query, score policy, and epoch. Prefer carrying a score as a hidden CustomScan output attribute that the planner references, rather than performing an ambient lookup on every SQL function call.

A context-requiring `pin.score(ctid)` API may resemble TIN's surface, but it needs explicit behavior outside a matching scan. The safe fallback for a ranked custom node is **ExhaustiveRankScan with the same score context**, not an ordinary heap plan whose score function has lost its binding.

`max_score` requires the maximum over the defined eligible match domain. It is not necessarily the highest score among an arbitrary limited or prefiltered shortlist. Avoid division by zero in any normalization API when all scores are zero.

### 7.8 LIMIT cannot move freely through SQL

Initial optimized ranked eligibility should exclude shapes whose correctness has not been proven: joins that filter or multiply rows, aggregates, DISTINCT, window functions, arbitrary score transformations, volatile qualifications, row locking, and unsupported parameterized/rescan shapes.

That does not mean those queries can never have score semantics. Compute exhaustive scored rows and let PostgreSQL perform the upper operation, or return a precise unsupported-context error where no correct binding exists. Do not push top-k below a join and hope retrieving `2*k` or `10*k` candidates will repair the result.

`OFFSET` requires at least `OFFSET + LIMIT` eligible winners. `WITH TIES` requires correct boundary handling. Deep offsets can erase the benefit of top-k; report that instead of hiding it.

For multi-field search, combine field contributions under a defined policy and bound their sum. Any early field-specific pruning must remain safe for the combined score. Cross-partition local statistics should be an explicit mode; a later global-statistics option is a separate feature.

### 7.9 Exact count plus ranked results

An exact total hit count requires considering all predicate matches whose visibility is not certified away. Score-based pruning only says a block cannot improve top-k; it says nothing about whether the block contains countable rows.

Offer either two clearly separate consumers sharing immutable metadata, or one executor with distinct membership-count and scoring work. It may skip TF/position work unnecessary for ranking while still executing the exact count branch. Do not sell a competitive-hit count as the total result count.

## 8. PostgreSQL visibility: retain the proof, replace the global bottleneck carefully

### 8.1 Four different facts

Keep these concepts separate:

```text
predicate membership: indexed text satisfies the query
publication: complete index data for a document is searchable
liveness: VACUUM has not declared this indexed incarnation removable
visibility: this statement's PostgreSQL snapshot can see the heap tuple
```

Publication is not transaction commit. Liveness is not snapshot visibility. A deleted-but-not-yet-removable version can still be visible to an old transaction. An aborted published owner can remain in index storage and must be rejected by heap visibility until cleanup.

PostgreSQL's ordinary bitmap route can defer heap checks under MVCC snapshots. The synchronous index route and index-only visibility shortcuts have additional lifetime/ordering obligations. The pinned heap handler follows HOT chains and installs the visible tuple into a buffer-backed slot. Preserve those contracts rather than implementing a superficial xmin/xmax test in Rust. [P01] [P02] [P06]

### 8.2 What current grouped count does

The current count path conditionally obtains a shared lock on the same logical writer lock used exclusively by insertion. It holds that generation interlock across source reads and visibility decisions. It also retains structural protection. This makes anti-reuse reasoning tractable but can block writers for the duration of an accelerated count. If the lock cannot be acquired, the code falls back. [C09] [C10] [C11]

This is a correctness-first implementation, not proof of a scalable steady-state concurrency model. Disabling the lock without replacing its role would be a regression.

### 8.3 Why a VM bit alone is insufficient

PostgreSQL's index-only scan code explains the relevant memory ordering: insertion clears the heap visibility-map bit before publishing the index entry, and the index read/write synchronization makes the clear observable to the reader. Deletion and snapshot acquisition have their own ordering argument. A custom count cannot assume those relationships merely because it reads the same VM function. [P05]

A dangerous scenario is a copied old posting, subsequent VACUUM removal and heap-slot reuse, followed by a VM-based count of the new occupant without a heap snapshot check. Ordinary MVCC heap fetch would reject a too-new replacement; a shortcut must establish why it is safe to omit that fetch.

A lease that only keeps segment bytes allocated does not necessarily keep their liveness current. That is a second, separate problem.

### 8.4 Proposed first scalable protocol: bounded heap-group lifetime guards

Do not start with a lock-free epoch protocol. Start with a deliberately conservative protocol whose scope is **one heap group**, not the whole index or query.

The host introduces a logical lifetime guard keyed by `(index relation generation, heap group)`, in a lock namespace distinct from the current writer and structure tags. Readers use shared mode; retirement/reuse-sensitive publication uses exclusive mode. The guard is not a PostgreSQL heap content lock.

A count reader:

1. Leases a complete source manifest after acquiring its statement snapshot.
2. Acquires the group's shared lifetime guard **before copying membership or liveness** for that group.
3. Reads current effective liveness for every source domain it will use.
4. Computes exact masks and applies them to the count consumer.
5. Uses the VM only after the publication/read ordering contract has been established. Otherwise it performs page-local heap visibility checks.
6. Adds the contribution exactly once, releases pins and the group guard, and moves on.

There must be no wait on a broad index publication lock while holding a heap content lock. The first implementation may process groups in physical order and retain at most one group's lifetime guard, simplifying cancellation and lock-order review.

This is a proposed protocol, not a proven substitute for the current lock. Its native acceptance conditions are in sections 15 and 16.

### 8.5 VACUUM must update retained generations too

Simply changing current `retire()` to clear the active/newest segment is insufficient once scans can retain old manifests while merges publish new ones. The present implementation can rely on stronger structural exclusion; the new one cannot. [C20]

The new manifest/lifecycle system needs a registry of liveness targets for **current, retained, and ready-to-publish** components, indexed by heap group and document identity. A bounded first design updates these targets under the group's exclusive lifetime guard.

Retirement procedure:

```text
obtain PostgreSQL's removable-root decision
acquire group lifetime guard exclusively
find all registered representations of that document incarnation
clear their liveness monotonically through WAL-logged changes
advance the group's retirement generation
complete canonical lifecycle retirement
release the guard
only then allow the index VACUUM operation to finish that removal
```

A copied liveness mask or descriptor cache is reusable only when its retirement generation remains valid. A cached VM answer is not carried from an earlier group visit or statement.

Retired components remain registered while any manifest lease can read them. Otherwise an old reader could consult a structurally valid but permanently stale liveness bitmap. This is an explicit gate before replacing the current structure lock.

Bound the number/bytes of retained generations. Slow readers can retain storage and increase retirement fanout; expose that debt and apply backpressure to maintenance rather than freeing live storage. Do not force a silent semantic truncation to stay within a memory limit.

### 8.6 Document coordinates must never be resurrected inside a domain

Consider an old document containing `a`, followed by reuse of its heap coordinate for a new document containing only `b`. OR-ing the old `a` postings and new `b` postings by coordinate before evaluating `a AND b` invents a document that never existed.

The invariant is:

> Within a document domain, one root coordinate is assigned to at most one document incarnation for that domain's entire lifetime. A cleared live bit is never reset for a different document.

Initially retain the durable canonical owner/identity authority. For mutable grouped storage, track roots ever assigned in an active domain. A collision rotates or creates a new bounded micro-domain for the affected group; it must not reuse the old liveness bit. A frequent-update workload can stress this policy, so record rotations and retained-domain amplification.

Evaluate the complete Boolean/positional predicate in each qualified domain, apply effective liveness, then merge completed matches. Do not merge per-term CTID sets across unqualified generations first. The current `GroupKey` discipline is valuable precedent. [C05] [C06]

Later optimizations may combine domains proven coordinate-disjoint after retirement. That proof is a format/maintenance invariant, not an assumed property of CTIDs.

### 8.7 Dirty-page batching

Retain the current page-batch direction. It reuses a heap buffer, checks multiple root offsets under one shared content lock, and invokes PostgreSQL's HOT visibility machinery. The upstream heap handler is the contract reference. [C10] [P06]

For a count-only consumer, no output tuple deformation is needed beyond what visibility requires. For a row consumer, the required slot and projection work remains. Do not pretend a count-only routine can return ordinary rows without those steps.

Bound how long a heap content lock is held, check interrupts between bounded batches, and test long HOT chains. Generalize to other table AMs only through an explicit capability contract; the first fast path remains native-heap-specific.

### 8.8 Metadata-only exact count certificate

A stored cardinality can replace offset decoding only when all of the following hold:

```text
the counted predicate is exact for this representation;
the document domain and liveness state are current;
no dead/unpublished contribution remains in the stored count;
all covered heap pages have a freshly justified all-visible status;
there is no unevaluated row/security qualification;
combined contributions are proven disjoint, or overlap is resolved exactly;
the lifetime/publication ordering prevents CTID-reuse confusion.
```

For `A OR B`, disjoint page sets can establish no overlap within the certified domain. Overlapping pages require exact offset handling unless richer metadata proves the overlap. A sum of DF values is not a general OR cardinality.

The optimization is especially attractive for broad counts. It should not be bolted onto the ordinary row path, where the rows still have to be produced.

### 8.9 Security and SERIALIZABLE

Current count eligibility deliberately rejects RLS/security-sensitive and serializable cases. Keep that fallback until the new custom path has an explicit proof. [C10]

Heap-elided execution must preserve predicate locking and serialization-conflict behavior. PostgreSQL's index-only node performs predicate locking when it does not fetch the heap. A custom node does not inherit that behavior by resemblance. [P05]

A safe first release can retain the broad index predicate-locking policy used by the AM and restrict aggressive custom shortcuts. Later finer-grained SSI support requires tests of phantoms, negative searches, and concurrent insertions, not only matching row counts.

Do not mark query-dependent ranking/highlighting functions leakproof merely for planner convenience. Security barriers, permissions on underlying indexed expressions, and score/statistics diagnostics need independent review.

## 9. Writes, sealing, and compaction

### 9.1 Preserve complete-document publication

The current write path reserves an owner, stores its complete payload, links every term, then publishes the owner. A query must not see half the terms of a document. An SQL rollback does not require every physical index write to be undone immediately, because PostgreSQL visibility remains authoritative. [C02]

The new format should retain this publication principle:

```text
RESERVED -> PAYLOAD_READY -> TERMS_LINKED -> PUBLISHED
                           \-> abandoned preparation, reclaimable later
```

The final publish marker or descriptor must become visible only after all structures required to answer the document's queries are recoverably linked. New readers synchronize through that publication point. No rank/count path treats a prepared record as published.

Packing can reduce the number of pages and WAL records touched. It does not eliminate the fundamental need to account for each unique indexed term. A “one append per document” design still needs either a searchable term-addressed mutable index or an explicitly bounded scan of new documents.

### 9.2 Do not replace one unbounded frontier with another

A bare append-only document log is excellent for ingestion but poor as the only searchable mutable representation. A broad or rare query then repeatedly analyzes or inspects unrelated recent documents.

The destination is a durable, term-addressed mutable component with packed posting chunks and complete-document publication. The document log/lifecycle record remains useful for recovery, position storage, and dense scans. An in-memory acceleration can be rebuilt from durable state, but cannot be the only record of committed searchable data.

Choose between term-driven and document-driven mutable scans using recorded cost estimates. Retain the current dense-owner frontier as a bridge, not as an excuse for allowing an arbitrarily large suffix. [C07]

### 9.3 Remove synchronous rebuilds from the insertion critical path

`grouped::maintain_delta` explicitly runs with exclusive structural and writer barriers held, and it may choose rebuild or seal. The feature already exists; the needed change is its execution and publication protocol. [C08] [C19]

Proposed maintenance state machine:

```text
ACTIVE
  -> FROZEN_SEARCHABLE
  -> BUILDING_OUTPUT
  -> OUTPUT_REGISTERED_FOR_RETIREMENT
  -> READY_TO_PUBLISH
  -> PUBLISHED
  -> OLD_INPUT_RETAINED
  -> RECLAIMABLE
```

Capture a bounded input set and rotate the active mutable component under a short lock. Continue serving the frozen component while a worker builds immutable output outside that lock. New inserts go to a different searchable active component.

A build failure leaves the frozen source searchable. It must not strand committed rows in a source that neither the manifest nor the frontier scans.

### 9.4 The retirement/publication race must be explicit

A compactor can copy a live bit, then VACUUM can clear the source bit, then the compactor can publish its old copy. Without reconciliation, that resurrects a retired document.

Proposed output registration procedure, per heap group:

1. Build output from leased sources without holding the group lifetime guard.
2. Acquire the group guard exclusively.
3. Reconcile retirements since the source capture, using a bounded retirement journal or authoritative lifecycle records.
4. Register the output's liveness targets before releasing the guard, even though the output is not yet in the searchable manifest.
5. From that moment, VACUUM updates the ready output as well as all retained/current sources.
6. Publish the complete manifest with a short, WAL-logged root change after all groups are ready.

This makes the race a defined handoff rather than a vague requirement to “apply tombstones.” If manifest publication loses a generation compare-and-swap or the worker fails, the unpublished output is unregistered and reclaimed only through the recovery/ownership protocol.

Do not hold a global publication mutex while waiting for arbitrary group guards. Establish and test one lock order. Group registration and final manifest publication must be designed so another worker cannot publish a conflicting retirement target set in between.

### 9.5 Out-of-order publication becomes important when writers are parallel

The current serial writer simplifies a scalar owner frontier. A future sharded writer can reserve owner 100, stall, and publish owner 101 first. Sealing “everything up to maximum published owner 101” can then miss owner 100 when it finally publishes.

A high-water mark is safe only if it denotes a **complete covered prefix**, not the largest identifier observed. Use one of:

- per-lane complete publication frontiers with explicit holes;
- freezing a lane only after all its in-flight reservations resolve;
- immutable document commit records whose inclusion is tracked independently of numeric order.

The query manifest must describe covered and uncovered records without omission or duplicate scoring. Add a deterministic schedule for this before adding concurrent writer lanes.

### 9.6 Shard writers only after removing page amplification

A reasonable first primary-format release can still serialize short publication operations. First measure whether packing and off-critical-path maintenance remove most writer CPU and lock hold time.

If contention remains, shard mutable components into a bounded number of lanes. A document and all its terms must publish in one lane/domain. Avoid a global metadata counter update for every term; allocate identifier ranges and summary deltas with a protocol that tolerates unused reservations.

Do not shard solely by hot vocabulary term: one document then spans many locks and atomic publication becomes harder. A document-based lane assignment keeps the commit unit coherent. Query fanout and hot heap-group reuse still need measurement; more lanes are not automatically faster.

The implementation must account for crashed backends, abandoned reservations, subtransaction aborts, self-visible writes, and pending-lane cleanup. This is a separate concurrency milestone, not a formatting change hidden in a storage PR.

### 9.7 Maintenance scheduling is a control problem

Use more than a fixed owner threshold. Track:

```text
mutable bytes and documents
query cost attributable to mutable components
number and overlap of immutable runs
oldest frozen component age
retained bytes and oldest reader generation
retirement/dead fraction
write amplification and recent worker service rate
```

Use a bounded merge policy with size classes and a cap on overlapping searchable runs per range. Prefer cheap descriptor/extent reuse for disjoint clean groups; rewrite overlapping or fragmented groups when the reduction in future query cost justifies it.

Maintain a debt equation conceptually:

```text
new maintenance debt per interval
  = new mutable/retirement work generated - work completed
```

When debt grows persistently, increase worker service within configured resource limits, apply bounded foreground assistance, or throttle ingestion explicitly. Do not let the only answer be a surprise full-index rebuild in one unlucky insert.

TIN's operational documentation also makes clear that worker availability, memory, readahead, and VACUUM state affect its behavior. PIN needs explicit service budgets and observability rather than copying one set of thresholds from a different implementation. [T07]

### 9.8 WAL choices

Continue using generic WAL for the first packed and primary-format steps where its bounded page-update model fits. Reduce per-document commits by coalescing modifications to the same small set of pages and avoiding unnecessary byte movement within them.

Generic WAL operates on temporary page copies and can log deltas; it does not imply a full-page WAL record for every modification. New/full-image flags, checkpoint FPIs, and delta records must be measured separately. Do not claim that replacing a Rust page copy saves an equal number of WAL bytes. [P07]

A custom WAL resource manager may become justified for compact logical posting operations, replay-specific retirement conflicts, or large update patterns that generic page deltas handle poorly. That decision has costs: redo implementation, record compatibility, consistency masking, tooling, resource-manager identity, and a persistent preload requirement. It is not a free speed switch. [P08]

Do not introduce custom WAL and a new ranked executor in the same first patch. Preserve a smaller review surface.

### 9.9 Reclamation and space accounting

Separate these metrics:

```text
live logical payload
allocated active extents
retired but reader-protected extents
reusable free pages
relation high-water mark
actual filesystem bytes
```

A logical compaction can be successful while `pg_relation_size` stays unchanged. Reuse free pages for all appropriate new-format page classes; do not judge compaction by immediate filesystem shrink alone.

For recovery, every allocation belongs to a build/publication journal or a reachable manifest. On restart, distinguish reachable committed output, searchable frozen input, and abandoned output. Reclamation must never infer unreachability from only the newest manifest while older leases or replay requirements can still need a component.

## 10. Planner and executor integration

### 10.1 Replace the full-traversal cost placeholder

The current `pin_index_cost` explicitly prices a full index traversal through `genericcostestimate`; it has no calibrated term statistics. That is a clear integration gap. [C11]

Proposed cost decomposition:

```text
startup = query analysis + dictionary lookups + source/epoch acquisition

index work = descriptor pages + selected payload bytes
           + sparse seeks + mask operations + optional position/TF work

visibility work = estimated dirty heap pages * page setup
                + estimated dirty roots * HOT/snapshot work

output work = qualifying slots + projection + required sort/aggregate work

parallel work = startup/coordination + total work / useful workers
              + merge/gather cost
```

Price those physical quantities with PostgreSQL's configured cost parameters and measured coefficients appropriate to the target. Do not hardcode an artificial discount to force a custom path to win.

Store summaries useful for estimation: term DF, groups/pages covered, run overlap, average membership bytes, mutable coverage, and sampled visibility state. Stale estimates can choose a slower correct plan; they must not authorize a semantic shortcut.

Prepared queries need a generic-plan fallback when the search text is not known at planning time. Runtime specialization can choose internal cursor strategies while retaining a correct plan contract. Revalidate cached profile, index generation, and SQL binding on rescan or invalidation.

### 10.2 Cost count and rank separately

The current narrow count hook retains the ordinary aggregate as a fallback and credits limited saved work. That is conservative, but not a full cost model for certified page counts. [C10]

Estimate count based on surviving masks, VM-certified pages, dirty-page visibility, and predicate complexity. Estimate rank based on candidate density, scored terms, bound quality, expected winner threshold establishment, and heap qualification.

Do not advertise score order through `amcanorderbyop` without implementing the exact operator/order/recheck contract. A dedicated custom ranked path is the clearer first integration. Do not set `amcanreturn` merely to obtain count acceleration: the current TEXT index does not reconstruct arbitrary original indexed values as an ordinary index-only scan would require. [C17] [P01]

### 10.3 Residual filters and other indexes

For a query combining text and a selective ordinary condition, compare:

```text
text-first candidate stream -> residual filter
ordinary index first -> PIN predicate/score lookup
BitmapAnd/BitmapOr through PostgreSQL
native combined custom path, only for proven eligible shapes
```

A date/category predicate can dominate selectivity. A text-first ranked path can be poor when most high-scoring rows fail it. Gather filter selectivity and observed rejected-candidate counters before attempting more complex integrated filters.

SQL OR between a PIN predicate and a non-PIN condition needs careful deduplication and score semantics. Preserve ordinary PostgreSQL bitmap/upper-plan behavior until a native equivalent is implemented. `work_mem` exhaustion should spill where supported or choose a correct fallback, not return a partial union.

### 10.4 Parallel execution

The repository already has parallel scan/build infrastructure. The next architecture must adapt its work division; it should not declare parallelism absent. [C01] [C17]

Use document-domain/group work units large enough to amortize worker startup, with dynamic allocation when density is skewed. Counts can sum disjoint, visibility-correct local contributions. Ranked workers can maintain local eligible top-k sets and merge under the global ordering, provided partitioning covers each eligible document exactly once and no upper SQL operation invalidates local pruning.

A shared competitive threshold may be stale in the lower direction without losing correctness; it merely skips less work. It must never be raised by ineligible or provisional rows. Publishing a threshold and its tie policy across workers requires explicit synchronization.

Separate latency from resource efficiency. A four-worker query that halves latency while doubling total CPU is not a 2x CPU optimization. Measure leader plus worker CPU and coordination overhead.

### 10.5 Error and cancellation cleanup

Every custom node needs complete initialization, execution, rescan, early shutdown, error, and end-of-query paths. Register host resources with PostgreSQL lifetime machinery. Rust destructors alone cannot be assumed to run across every PostgreSQL error boundary. [P09] [P10] [E10]

Required resources include manifest leases, logical guards, buffer pins, sort tapes, worker state, and score contexts. A rescan must not reuse the previous query's VM result, top-k threshold, parameter-dependent bounds, or captured mutable coverage.

Do not let ordinary planner fallback re-enter a score function bound to a discarded custom node. This is a functional correctness problem, not merely cleanup.

## 11. Feature completion without degrading the fast common path

### 11.1 Versioned analysis profiles

Represent analysis configuration as an immutable fingerprint covering Unicode data version, normalization/folding policy, token boundaries, long-token handling, grapheme handling, position gaps, and limits that affect accepted inputs.

The tokenizer inspection API must call the same profile implementation as index construction and query analysis. Maintain golden cases for accents, combining sequences, emoji, apostrophes, hyphens, supplementary characters, very long tokens, empty strings, NULL, and words retained for phrase matching.

Do not remove stop words from membership merely to make ranking faster. A score policy can omit a term contribution while the positional index still retains it. Analysis changes that affect stored terms/positions require rebuild; score policies remain separately configurable.

### 11.2 Vocabulary expansion

Normalize exact-term inputs according to their profile. Compile wildcard/fuzzy/range expressions into a vocabulary iterator, not an eager unbounded vector of expanded terms.

A first implementation can use sorted dictionary prefix/range traversal and a bounded edit-distance automaton for fuzzy terms. General regex needs a deliberately selected, non-backtracking or otherwise bounded evaluation model. Its exact syntax and normalization rules must be part of the public contract.

An expansion budget measures states, dictionary bytes visited, emitted terms, posting work, and elapsed/cancellation checkpoints. Exhausting it is not permission to take the first thousand dictionary terms and omit the rest. Return a resource error or execute a documented exact spill/slow path.

Cache compiled automata only with query/profile keys and bounded memory. A cached expanded vocabulary additionally needs a dictionary-generation dependency, because committed inserts can add matching terms.

### 11.3 Multi-field and expression indexes

Keep one text source per physical index as the first product model. Combining multiple fields then means combining field-aware query sources, not inventing a new multi-column storage format immediately.

Partial and expression indexes must use PostgreSQL's expression/predicate implication machinery. Do not match indexes by attribute name or a textual string of SQL. For ranking, keep each field's length/statistics and boost interpretation explicit.

If indexes use different root representatives after build/HOT histories, normalize the identity through the visible heap tuple or a proven common-root protocol before combining results. A raw CTID join across independently constructed index result streams is not sufficient without that contract.

### 11.4 Concurrent DDL

`pin_storage_check` currently rejects concurrent builds. Supporting `CREATE INDEX CONCURRENTLY` is not simply removing the check. The build, validation scan, ready/valid catalog states, concurrent insertion coverage, snapshot waits, and failed-build cleanup all need the PostgreSQL protocol. [C11]

Keep ordinary REINDEX as the first format migration. Add concurrent build/reindex only after the direct builder and new mutable publication path have coverage tests for rows changing between build phases. Index replacement must invalidate relation-generation caches and score contexts.

### 11.5 Replication and recovery

Search on a physical standby is currently rejected. Physical replay of index pages and safe concurrent standby reads are distinct capabilities. [C01] [C11]

Primary-backend logical locks do not automatically protect a standby query from replay. Before enabling standby search, specify how replay invalidates/retains old segment pages and how conflicts are resolved or queries cancelled. Feed-back-based horizons, replay LSNs, retention, and cleanup records must form a complete protocol. `hot_standby_feedback = on` alone is not that protocol.

A custom cleanup WAL record may be warranted to carry the necessary conflict information; evaluate that together with the resource-manager decision. Keep the runtime refusal until native replay, old-snapshot, cancellation, failover, and restart tests pass. [P08] [P12]

Logical replication normally transfers table changes, not a magic copy of PIN's physical index internals. Subscriber index maintenance must build its own correct state. Do not confuse generic WAL being ignored by logical decoding with ordinary replicated table changes failing to maintain an index. [P07]

## 12. CPU-level implementation priorities

### 12.1 Remove dependent work first

The expensive pattern to eliminate is:

```text
term lookup -> posting page -> owner reference -> owner page
            -> full document validation -> selected term/position lookup
            -> heap text analysis or scoring
```

The target pattern is:

```text
term/group descriptor -> selected membership page
                     -> optional selected TF/position block
                     -> heap visibility/projection only when required
```

This reduces pointer chasing, copied bytes, repeated validation, and branches by changing what the query needs to touch. The magnitude of the benefit still requires native measurement.

### 12.2 Validation and cache policy

Start with a callback-local cache of **validated immutable descriptors or already-loaded owned page payloads**, not a shared global page cache. PostgreSQL remains the buffer cache.

Keys need relation generation, page/extent identity, format and publication/retirement generation; mutable data additionally needs a coherent version/LSN contract. A cache hit must not reintroduce stale liveness or skip an invalidation on REINDEX, relation replacement, or page reuse.

Separate structural validation from query-local bounds checks. Avoid validating the same copied page repeatedly in adjacent functions when a typed validated view can carry that fact. Preserve checks at every untrusted range transition.

Do not let this become another speculative syscall optimization. Keep a cache only when paired results show a gain larger than noise and the lifetime surface remains reviewable.

### 12.3 Scratch memory

Allocate query workspaces in bounded reusable arenas. Size by the chosen plan and actually needed terms/containers, rather than maximum bitmap bytes for every term. Keep stack allocations small enough for PostgreSQL backend call stacks; large fixed group/member buffers belong in explicitly budgeted storage.

Account for peak memory during decode, merge, output buffering, and fallback transition. Two individually bounded paths can exceed the budget if both are live during a handoff.

Avoid per-candidate `Vec`, string allocation, and normalized-term copying. Intern query terms within the query context, not globally. For small sparse plans, initialization overhead can dominate the actual set operation.

### 12.4 SIMD

PIN already contains runtime-dispatched AVX2 slice kernels, while grouped masks are currently scalar. Reuse the dispatch abstraction instead of adding scattered target-feature checks. [C16] [C18]

Benchmark fixed-size scalar unrolling against AVX2 for page/offset Boolean operations. A 256-bit AND is not automatically the bottleneck once descriptor loads and heap checks are included. AVX2 Boolean operations and AVX-512 vector population-count support are different capabilities; detect the features actually used. Rust documents separate runtime feature names. [R01]

Keep portable scalar kernels as the reference and deployment fallback. Verify alignment-independent loads, tail masks, aliasing assumptions, feature-disabled builds, and mixed CPU environments. Do not ship a binary globally compiled for a deployment CPU feature that some supported hosts lack.

Use assembly inspection and PMU measurements when available. Without exposed counters, report software-clock samples and bytes/operations; do not invent IPC or cache-miss improvements.

### 12.5 I/O and readahead

For cold data, schedule predictable index/heap reads in bounded lookahead batches. Do not bypass PostgreSQL's buffer manager with direct filesystem reads merely to obtain an attractive microbenchmark.

Make readahead conditional on workload and supported PostgreSQL APIs. On an entirely warm index, speculative I/O hints can be overhead. Storage-medium and cache-state measurements must determine whether a prefetch path is enabled. [T07]

Global heap ordering across many components may need a merge of streams or a bitmap. A per-segment ordered stream is not automatically globally ordered. Price the merge and preserve SQL ordering independently of physical traversal.

### 12.6 Amdahl's law gives a testable limit

Let `f` be the fraction of total query CPU removed by an optimization. Even an infinitely fast replacement of that fraction yields at most:

```text
speedup <= 1 / (1 - f)
```

If only 5% of a query is mask arithmetic, making that arithmetic free produces at most about 1.053x. This is a mathematical illustration, not a measured current PIN fraction.

For a 10x total-CPU target, all unavoidable unchanged work must fit within 10% of the old total. Broad row retrieval can be bounded by visibility, slots, projection, ordering and output; native BM25 can change a different amount of work than a Boolean kernel can. Use separate query-class goals instead of a universal multiplier.

## 13. Concrete implementation map and release sequence

### 13.1 Change boundaries

The following names under “proposed addition” are suggested module boundaries, not files already present. Keep each change independently reviewable. New modules should preserve the current concise, invariant-focused comment style.

| Current code or boundary | Change | Expected mechanism | Required evidence before enabling |
|---|---|---|---|
| `mutable/writer.rs::link_term`, `mutable/page.rs`, `mutable/compact.rs` | Add inline small canonical lists and a shared slotted posting arena with stable handles | Remove a dedicated physical page for most short lists; reduce allocation, buffer operations and WAL work | Codec round-trips; stable-handle relocation tests; VACUUM/reuse tests; paired page census and write/WAL measurements |
| `mutable/grouped/storage.rs::read_bitmap_view` and `read_record`; `grouped/scan.rs::scan_snapshot` | Replace whole-record materialization with a page directory and independently addressable offset containers | Read and validate only surviving page containers, not the whole needed term/group record | Exact same results and recheck flags; measured payload/physical pages avoided on disjoint and selective cases |
| `mutable/grouped/build.rs`, `pin-pg/src/grouped.rs` | Direct sorted construction of the primary format, with a separately budgeted document metadata stream | Avoid constructing and rereading the legacy representation during every full build | Build parity, cancellation cleanup, bounded-memory spill, and build CPU/WAL/peak-space measurements |
| `mutable/document.rs` and `query.rs` | Keep current inline proof; add a selected-term positional reader and span IR | Stop reparsing complete owner documents for common phrase/span questions | Fragmented, repeated-term and nested-expression parity; corrupted-block checks; positions/bytes decoded per match |
| `rank.rs` | Retain the exhaustive oracle; add a separate `block_bounds.rs` module | Make safe pruning independently testable rather than burying it in score calculation | Exhaustive comparison for all supported score parameters and adversarial numeric values |
| Proposed `pin-pg/src/ranked.rs` and a small C executor boundary | Add statement-bound exhaustive ranked execution first | Expose indexed TF/length scoring without changing SQL score semantics or losing snapshot checks | Exact score/order/visibility parity; context, rescan, alias and fallback tests |
| Proposed `manifest.rs`, `retirement.rs`, `maintenance.rs` | Make generations, registration, publication, retirement and resource cleanup explicit | Shorten writer barriers without weakening the CTID-reuse proof | Deterministic publication/VACUUM race tests and fault injection at every state transition |
| `pin-pg/src/grouped.rs::maintain_delta`, insertion maintenance entry points | Freeze bounded work and build outside the global publication critical section | Reduce writer stalls and avoid unbounded synchronous rebuild work | Writer latency and maintenance-debt stability under sustained churn |
| `pin_count.c`, `pin_storage.c`, grouped retirement callbacks | Add group-lifetime guards only after retained-generation retirement is implemented | Permit counts and writes on unrelated groups to progress concurrently | Native lock-order, old-reader, root-reuse, cancellation and starvation tests |
| `pin_storage.c::pin_index_cost` and custom path construction | Replace full-traversal estimates with measured work estimates | Select sparse, grouped, count or rank plans for the actual query | Estimated versus observed work; prepared-plan/rescan cases; no forced-planner benchmark claims |
| `analysis.rs`, `query.rs`, SQL option metadata | Add explicit analysis/query profile versions and bounded vocabulary expansion | Broaden features without slowing or changing the existing exact-query contract | Golden tokenizer/dialect tests; expansion limits; REINDEX enforcement |
| AM build/recovery callbacks and storage guards | Qualify concurrent DDL and standby reads as separate milestones | Complete operational compatibility rather than merely making an unsupported path callable | Concurrent validation, crash, replay, old-snapshot and recovery-conflict suites |

The first four rows are grounded in concrete existing paths, not a claim that every proposed replacement has already been implemented. [C02] [C03] [C04] [C05] [C06] [C08] [C13] [C14] [C17] [C19]

### 13.2 Dependency graph

```text
A0: evidence/counters + immutable baseline
  |
  +--> A1: packed canonical bridge ----------------------+
  |                                                      |
  +--> A2: exhaustive SQL BM25 + context/oracle ----------+--> C2: safe block pruning
  |                                                      |
  +--> A3: selected-position interface + semantic tests --+--> C3: native span execution
                                                         |
B1: primary packed format + direct builder ---------------+
  |
B2: manifest/retirement registry + crash-safe publication
  |
B3: off-critical-path maintenance + bounded debt
  |
C1: group-lifetime guard replaces query-wide count barrier
  |
D: broader parallelism, fields, concurrent DDL, standby qualification
```

Some interface and oracle work can proceed in parallel. The lifecycle dependencies cannot: **do not remove the global count guard before the registry/retirement protocol exists**, and **do not enable block pruning before exhaustive ranked execution is correct**.

### 13.3 The first implementation tranche

**A0 — A reproducible baseline and observable work.** Freeze the source revision, build identity, SQL, GUCs, corpus and raw-result schema. Add per-query counters for dictionary lookups, component probes, descriptors/bytes read, offset containers decoded, positions decoded, candidate roots, heap visibility checks, emitted rows, score evaluations and bound skips. Counters should be optional or low-overhead and must be disabled or separately measured for final throughput tests. Report fallback reasons, not only successful fast-path counts.

**A1 — Packed canonical bridge.** Implement small inline lists and packed posting arenas without changing the owner/publication model. Include an offline integrity inspector and a page census. Demonstrate that the thousands of nearly empty posting pages disappear on the existing fixture and that high-frequency terms still have efficient chunked access. Include a corpus with mostly singleton terms, one with many two-document terms, and one with very frequent terms. Do not choose a fixed threshold solely because it wins the existing six-word synthetic corpus.

**A2 — Exhaustive SQL BM25.** Expose a correct score context and exhaustively enumerate eligible matches using existing payloads initially. This is a feature and oracle milestone, not a promised speed milestone. It prevents the storage redesign from optimizing a score interface that cannot survive aliases, joins or rescans. Keep the new custom path opt-in until its context/fallback behavior passes the native suite.

**A3 — Selective positions and reader APIs.** Define and test the selected-position interface before committing to its final encoding. Implement inline and fragmented adapters over the current representation, then the packed positional codec. Measure the whole-query effect independently from grouping/count changes.

These four units produce useful results even before a primary-format rewrite is complete. They also supply the missing measurements needed to tune the new format.

### 13.4 The primary-format tranche

**B1** introduces the packed primary representation, adaptive posting containers, stable dictionary identities, directly addressable TF/positions, and direct external-sort build. Retain legacy readers for old indexes; do not require dual representation inside every newly built index unless a fallback actually needs it.

**B2** introduces the manifest/registration/retirement state machine and durable ownership of all newly allocated extents. Keep conservative locking while qualifying its crash and old-reader behavior.

**B3** moves sealing and compaction outside the writer critical section, with bounded complete publication frontiers, retirement catch-up, debt accounting and cancellation-safe worker ownership. Only after that should write lanes be sharded. Moving an expensive operation to a worker without bounding its debt is not completion.

**C1** replaces the global count/writer interlock with the group-lifetime protocol. An implementation may choose to retain the conservative mode on older formats and use the new protocol only on the new primary format. That is simpler than forcing every legacy path to participate in a new lifetime model.

**C2/C3** add competitive ranked pruning and complete indexed phrase/span execution, using the same semantic, score and visibility oracles. Enable each independently so regressions can be attributed.

### 13.5 Release gates rather than a calendar promise

Each stage must pass four gates: semantic correctness, lifecycle correctness, bounded resource use, and a paired measurement demonstrating its intended mechanism. The first two are hard blockers. An optimization with no measurable gain can remain disabled or be removed even when its implementation is correct.

Set a regression budget before running the acceptance experiment. For example, the team may adopt a proposed 5% whole-query CPU regression budget on designated unaffected classes, with repeated balanced blocks and uncertainty intervals. That number is a suggested project policy, not a measured PIN characteristic and not permission to hide a severe p99 or writer regression behind an average.

Do not enable all new paths in one final switch. Preserve independently controllable feature gates through qualification, then simplify the supported configuration surface after the mechanisms are stable.

## 14. Format migration and operational compatibility

### 14.1 Explicit version policy

Use a new incompatible major storage format for the primary-layout change. Its metapage must carry the format version and capability bits required to interpret every component. Do not infer format from whether a page “looks like” a familiar record.

The first supported migration is **REINDEX with the new extension binary**. Old-format readers may remain available for an explicit transition window, but new writers must not silently mix incompatible records into an existing old-format index. Prefer one clear policy per release:

```text
old format + new binary: supported legacy read/write path, or explicit REINDEX-required error;
new format + new binary: new authoritative primary representation;
new format + old binary: refuse to open, never guess;
analysis profile change: REINDEX required;
query-time score parameter change: no re-tokenization, but bounds recomputed for that parameter context.
```

Specify which of the two old-format policies is chosen. “Readable during migration” is not enough if ordinary inserts still call an incompatible writer. A bridge release can support old writes; a later release can require REINDEX before enabling new functionality.

### 14.2 REINDEX and cache identity

Rebuild into a new relation identity, validate it, and let PostgreSQL perform the appropriate replacement. All caches and executor contexts must include the relation/format generation, not only the index OID. A reused OID or relfilenode is not an everlasting identity.

Concurrent REINDEX remains unavailable until the concurrent-build protocol is qualified. Do not advertise “online migration” merely because a background compactor exists. The current source has explicit restrictions and the new builder must satisfy PostgreSQL's index lifecycle, not circumvent it. [C11] [C17] [P01]

Publish a preflight estimator for peak build space: old live index, retained generations, new output, temporary sort spill, and WAL. Free pages in the old relation do not automatically eliminate external build-space requirements.

### 14.3 Upgrade, downgrade and backups

An extension SQL upgrade and a physical index rewrite are distinct operations. Document the order: install compatible binaries, update SQL metadata where necessary, rebuild indexes, verify format/capabilities, then enable the new executor paths.

A downgrade after writing the new format requires a supported reader or a rebuild under an appropriate binary; changing only the extension version string is unsafe. Physical restore/replay requires binaries that understand the stored format. Include the on-disk format and analysis profile in diagnostics and benchmark manifests.

Keep a recovery-safe conservative scan path **within the new format**. A “fallback” cannot depend on legacy canonical chains that a primary-only index no longer stores. Missing required capabilities should produce a clear error or a planner-selected independent table scan, not incomplete results.

## 15. Adversarial review: what was rejected and what replaced it

### 15.1 Review rounds

The design was reviewed against successive classes of failure. The table records the resulting revisions; these are design attacks, not assertions that the current implementation contains the rejected shortcuts.

| Round | Attractive first idea | Failure | Revised design |
|---|---|---|---|
| Representation | Merge all segment term bitmaps by raw CTID, then run Boolean operations | Old and new occupants of the same coordinate can manufacture a conjunction | Evaluate complete predicates within incarnation-qualified domains; merge completed matches |
| Representation | Page-mask NOT is just bitwise complement | A page containing one matching tuple can also contain many nonmatching tuples; lossy candidates are not exact truth | Preserve exact/unknown states and compute offset-level exclusion against the valid document universe |
| Storage | CTID-native means every merge is a no-copy concatenation | Overlap, incompatible encodings, updated liveness and statistics still require reconciliation | Transfer only compatible, proven-safe extents; rewrite overlapping or incompatible ranges |
| Counting | A manifest lease plus an all-visible page is sufficient | A byte-valid old bitmap can still contain a retired/reused coordinate | Group-lifetime protection plus up-to-date retirement for every retained generation |
| Counting | Sum term DF or per-component candidate counts | Overlap and duplicate incarnations overcount | Count only disjoint certified contributions or resolve overlap exactly |
| Ranking | Put every high-scoring index candidate in the top-k heap | Invisible or filtered candidates can raise the threshold and hide the true visible winners | Only snapshot-visible, authorized, fully qualifying rows establish the threshold |
| Ranking | Skip when upper bound equals the current kth score | An equal-score row can win on the SQL tie-breaker | Strict score inequality unless the bound also proves the tie-order result |
| Ranking | Store one precomputed block score bound | Runtime parameters, term weights and changed corpus statistics invalidate it | Store raw conservative sufficient statistics and evaluate bounds under the pinned score/statistics context |
| Ranking | Reuse `term_score(tf_max, dl_min)` for the bound | The synthetic maxima/minima pair may not be a physically possible document and can violate exact-score validation | Separate bound arithmetic with its own domain, monotonicity proof and conservative rounding |
| Maintenance | Compact what is live when the copy begins | VACUUM can retire a root before publication; the output can resurrect it | Register output before publication and catch up retirement under the group protocol |
| Publication | Seal through the largest assigned owner ID | Concurrent publication can have holes below that ID | Seal only a completed frontier, or explicitly track and resolve holes |
| Operations | WAL replay works, so standby reads can be enabled | A reader on a standby has its own cleanup/reuse and snapshot hazards | Separate replay durability from replica-read lifetime qualification |

The strongest revisions are about **which work may legally be skipped**, not simply about making a loop shorter.

### 15.2 The executable design audit

`PIN_architecture_audit.py` produced the accompanying JSON on the supplied repository. It:

- checked all **175** entries in the latest raw-data checksum ledger, with no missing or mismatched files;
- recomputed the same-run phrase medians and the page-census fractions;
- checked **77,525** exact-rational candidate cases for the proposed nonnegative BM25 factor bound;
- checked **30,000** seeded Boolean-mask/cardinality cases;
- demonstrated six deliberately broken alternatives: cross-incarnation conjunction, unsafe complement, invisible ranking threshold, score-tie pruning, missed retirement during compaction, and overlap double-counting.

The rational test checks the algebraic factor under its modeled domain; it is **not** an IEEE-754 rounding proof, a complete BM25 implementation test, or a claim of exhaustive coverage of every legal numeric input. The Boolean models are not the production kernels. The retirement models contain no real PostgreSQL locks. This audit is useful for preventing design mistakes, not for certifying a native implementation.

Run it against the extracted repository with Python 3.10 or newer:

```sh
python PIN_architecture_audit.py /path/to/PIN-main \
  --output PIN_architecture_audit.json
```

It reads the repository and writes only the requested audit output. The checksum ledger is evidence integrity, not proof that the recorded experiment itself was correct. [E12]

### 15.3 Invariants to turn into assertions and native tests

Use explicit identifiers in code review and tests:

| Invariant | Required property |
|---|---|
| I1 — Complete publication | No query combines only part of one newly indexed document with another publication state |
| I2 — Incarnation isolation | A domain coordinate is never reassigned; predicates do not combine different incarnations |
| I3 — Retirement coverage | Every published or retained readable generation receives relevant retirement before heap reuse is permitted |
| I4 — Reader lifetime | Referenced extents and current liveness remain valid for the operations performed under the lease/guard |
| I5 — Statistics coherence | Every score and bound in one execution uses the same declared statistics and parameter context |
| I6 — Eligible threshold | Only fully eligible rows can raise a competitive threshold |
| I7 — Conservative skipping | Bounds, exactness flags and cardinality certificates never exclude a possible qualifying result |
| I8 — Resource bounds | Decode, expansion, merge, retained generations and maintenance debt have explicit limits and non-silent failure paths |
| I9 — Durable publication | A crash exposes either a complete old state or a complete recoverable new state, not a partially published document/manifest |
| I10 — Host semantics | Snapshot visibility, HOT, security, predicate locking, rescan and cancellation obey the supported PostgreSQL contract |

The group protocol is still a proposal. Its final lock classes, lock ordering, transaction/resource ownership and replay participation need native implementation review. A plausible state diagram is not a concurrency proof.

## 16. Required correctness and lifecycle qualification

### 16.1 Independent semantic oracles

Retain multiple levels of oracle. The parser/analyzer has golden token and AST expectations. Pure predicate/rank kernels have independent exhaustive implementations. Native SQL tests compare complete ordered row identities, not only counts. Storage fuzzers operate on arbitrary bytes and malformed length/offset combinations.

Do not let both the optimized path and its “reference” call the same newly written positional or pruning routine. That can make a shared bug appear to pass every comparison.

For each supported query family test empty documents, empty analyzed queries, absent terms, singletons, duplicate terms, repeated phrases, Unicode normalization, maximum positions, long and fragmented documents, query-depth limits, and exact error behavior. A bounded implementation must reject or fall back explicitly, never quietly truncate an expanded term set or positional witness set.

### 16.2 Deterministic concurrency schedules

Add test-only pause points around the real native state transitions. At minimum force these schedules:

**Publication and VM ordering.** Pause an insert after the heap VM clear but before index publication, then interleave another reader, a same-transaction reader and a newly acquired snapshot. Also pause immediately after publication. Verify that every reader obeys its snapshot and that the VM shortcut observes the required synchronization.

**Copied postings and recycled roots.** Pause a count before group-guard acquisition, after source copying, and after VM testing. Concurrently delete, VACUUM and attempt heap-slot reuse. The revised guard must either prevent the unsafe reuse window or cause the reader to refresh the relevant source/liveness state. Test a retired manifest held by the reader, not only the current manifest.

**Compaction retirement catch-up.** Copy a root to an unpublished output, retire it in the inputs, register the output, and attempt publication. Repeat with retirement between registration and publication. In every schedule the output must not resurrect the retired incarnation.

**Publication holes.** Stall owner 100 after reservation, publish owner 101, freeze/seal the component, then release owner 100. Both committed documents must remain searchable exactly once. Exercise cancellation/abort of the stalled owner as well as successful completion.

**Old snapshots.** Hold a repeatable-read snapshot across committed updates/deletes, VACUUM and compaction. A root still required by the global visibility horizon cannot be retired merely because it is invisible to a newer transaction. Test both old and new readers through the same maintenance cycles.

**HOT and non-HOT changes.** Exercise unindexed-column HOT updates, indexed-expression changes, non-HOT updates, multiple HOT-chain members, self-visible inserts, rollback, savepoints and speculative-insertion cleanup where applicable. Distinguish the index root from the visible tuple's `ctid` in identity and score-context tests. PostgreSQL's heap-fetch path is the reference for the supported native heap implementation. [P03] [P06]

**Lock/resource failure.** Cancel at every guard/lease acquisition, fail a worker, invalidate a relation, force a statement error, and rescan a parameterized plan. Assert no leaked pins, locks, shared work registrations or stale score contexts. Test that a long reader cannot starve unrelated-group writes indefinitely and that maintenance backpressure does not deadlock with a query waiting for the same resource.

### 16.3 Ranked execution adversaries

Place the mathematically highest-scoring documents in transactions invisible to the search snapshot. Put others behind failing residual filters and RLS policies. Verify that lower-scoring eligible documents still fill `LIMIT k`.

Test exact-score ties with a secondary SQL ordering key, multiple score expressions, rescans, self joins, same CTIDs in different relations, partitions, parameter changes and prepared statements. Test `LIMIT 0`, zero matching rows, all-zero scores, `OFFSET`, `WITH TIES`, row locking, aggregation and joins that change multiplicity. Unsupported early-pruning shapes must use a correct context-aware exhaustive plan.

For numeric bounds include zero and extreme legal parameters, minimal/maximum TF and lengths, dense-term elision boundaries, weighted terms, statistics transitions, non-finite input rejection, and cases where a rounded bound sits within a few representable numbers of the kth score. Fuzz the actual floating-point implementation against a higher-precision conservative reference.

A performance-safe bound must never return NaN. An overflow or unproved range should yield a safe unbounded result and disable pruning, not wrap or produce a smaller bound.

### 16.4 Crash and replay matrix

For every durable transition, inject failure before WAL insertion, after WAL insertion but before page flush, after some dependent page flushes, and after manifest publication. Cover new document publication, component freeze, output registration, manifest replacement, retirement and free-list insertion. Use an actual crash/restart harness rather than only returning an error from a function.

After recovery, compare complete query results with a table-derived oracle, check reachability/ownership of every extent, validate liveness and statistics epochs, and run VACUUM and a subsequent write workload. A format that can answer a read after recovery but corrupts on the next allocation has not passed.

Test checkpoint boundaries, torn or corrupted input handling within the platform's supported durability envelope, backup/restore and extension-version compatibility. Keep generic WAL initially unless a separately justified custom resource manager materially improves measured costs. [P07] [P08]

Replica-read qualification must additionally cover replay concurrent with long readers, cleanup conflicts, cancellation, feedback settings and retained generations on the standby. The accepted outcome may be a documented recovery-conflict error; it may not be a silently incorrect count or score. [P12]

### 16.5 Security and SQL integration matrix

Before broadening custom-path eligibility, test RLS, security-barrier views, column and table privileges, expression/partial indexes, SERIALIZABLE phantoms, predicate locks on empty-result searches, and planner predicate implication. Query diagnostics must not expose hidden term/document statistics to callers who lack the necessary table permissions.

Verify `EXPLAIN` identifies the actual method, eligibility restrictions and fallback reason. `EXPLAIN ANALYZE` should expose the key work counters when requested, including heap visibility checks and score evaluations. Avoid diagnostics whose own cost changes the selected plan or silently becomes part of normal query latency.

## 17. Measurement plan: distinguish a faster engine from a different experiment

### 17.1 Four separate comparison tracks

**Track A — PIN implementation equivalence.** Compare new paths against the current PIN semantic oracle and, where relevant, the prior native implementation. Hold the analysis profile, query meaning, snapshot and result shape constant. This establishes that representation and execution changes preserve the declared semantics.

**Track B — Fair PostgreSQL GIN overlap.** Use query families with demonstrably equivalent tokenization and matching. Keep the same heap projection, residual predicates, ordering, limit and visibility state. Continue the matched `ts_rank_cd` experiment as a control. A native PIN BM25 result is not a semantics-preserving acceleration of `ts_rank_cd`; label that comparison as a different ranking workload.

**Track C — Exact PIN BM25 acceleration.** Compare exhaustive indexed BM25 with block-pruned BM25 under identical statistics, parameter, score-term and tie-order policies. Test full scoring and dense-term-elided scoring separately. Verify complete ordered top-k identities and scores before timing. This track isolates the benefit of pruning without borrowing speed from changed score semantics.

**Track D — Actual TIN comparison.** Run the real TIN engine on the same corpus and a declared compatible feature subset. Record engine versions, configuration, analysis behavior, query rewriting, scoring mode, hardware/resource limits and maintenance state. Separate client/network latency from server execution and CPU where accessible. When the service does not expose an equivalent metric, say so; do not derive backend CPU from request latency. PlanetScale's published benchmark is useful workload context but is not a substitute for this track. [T01]

For semantics that cannot be made equivalent, publish the differences and compare relevance/coverage separately. Lead is useful for development compatibility checks, not Track D performance. [T03]

### 17.2 Corpus and query coverage

Retain the current small fixture as a rapid regression corpus. Add a medium realistic text corpus for iteration and a large corpus whose index exceeds the specified memory budget for qualification. Record document count, input bytes, term vocabulary, document-length distribution, language/profile mix and frequency distribution; a row count alone does not characterize FTS work.

Include documents with rare identifiers, common words, repeated phrases, long positional lists, multilingual normalization, and highly skewed vocabulary. Include a corpus rich in two-document terms to expose packing quality and a common-term corpus to expose bitmap/ranking limits. Preserve the corpus and query-generator seed, and publish the exact query trace.

The query matrix should cover:

| Family | Important variants | Primary question |
|---|---|---|
| Sparse terms/conjunctions | Absent term, rare/common mix, multiple rare terms | Does startup/dictionary work dominate; does rare-led execution avoid broad scans? |
| Broad Boolean | AND, OR, nested NOT, minimum-match | Are group and offset pruning effective without incorrect coarse subtraction? |
| Phrase/span | Common/common, rare/common, reversed, repeated, gaps, nested spans | Are positions fetched only for surviving candidates; is witness work bounded? |
| Vocabulary expansion | Prefix, wildcard, regex, fuzzy, adversarial large expansion | Is vocabulary enumeration bounded and costed; is there an explicit fallback/error? |
| Ranking | k of 10/100/1000, full/elided terms, score ties, parameter variants | How many blocks/documents are skipped; what makes the threshold rise? |
| Filtered ranking | Highly selective residual filter, RLS fallback, multiple fields | Is the eligible winner set exact; does late filtering defeat pruning? |
| Ordinary rows | Narrow/wide projection, ordered/unordered output | How much CPU remains in heap/slot/projection/output work? |
| Exact counts | All-visible versus dirty pages, disjoint versus overlapping terms | Can certified metadata replace decoding; what visibility cost remains? |

Record actual term DF/selectivity instead of calling a query “rare” based only on its name. A rare result can still require broad intermediate work.

### 17.3 Lifecycle states are part of every result

Measure fresh build with high all-visible coverage; an accumulated mutable suffix; heavy updates/deletes with dirty heap pages; after VACUUM but before compaction; after compaction; and after REINDEX. Add an old snapshot that forces retention, and sustained read/write traffic that prevents the system from being measured only in its cleanest state.

Test warm shared-buffer, warm OS-cache but cold shared-buffer, and genuinely cold or memory-constrained conditions separately. A PostgreSQL shared-buffer read is not automatically a physical storage read. Document the cache preparation procedure and never mix these states into one headline ratio.

For writes, hold durability settings constant. Account for GIN's pending-list cleanup and PIN's deferred maintenance rather than ending the measurement with one engine's debt still unpaid. GIN explicitly supports a pending-list fast-update path; its later cleanup is part of the operational cost. [P11]

Measure a fixed offered workload long enough to show whether debt stabilizes, then report the cost to drain the remaining maintenance debt. If throughput is high only because the index grows an unbounded unmerged suffix, the steady-state result has failed even before query latency collapses.

### 17.4 Metrics and attribution

Collect backend CPU, all parallel-worker CPU, foreground latency, worker maintenance CPU, memory high-water marks, lock waits, WAL bytes, page images where measurable, relation sizes, physical/index payload reads and maintenance debt. Report foreground and maintenance costs separately and together. Do not attribute a reduction in foreground CPU to reduced total work when the work merely moved to another process.

For profiles, preserve the sampling event, frequency, symbol files, build flags, loss count and scope. Use flat/self samples for attribution; inclusive call-tree percentages overlap and cannot be summed as independent costs. Profile one-pass positions directly before drawing further conclusions from the older first-pass profile. [E03]

`/proc/<pid>/schedstat` is the recorded mechanism behind the supplied backend CPU measurements. Preserve the raw sampling and per-batch arithmetic, and separately report client wall time. Do not reinterpret six batch medians as a distribution of individual-request p99. [E01] [P13]

Use hardware counters only when available and trustworthy in the environment. Report unavailable PMU access rather than inventing IPC, branch-miss or cache-miss evidence. Microbenchmarks of codecs and SIMD remain useful, but whole-query CPU and write/maintenance effects decide whether to ship the change.

### 17.5 Statistical and reproducibility discipline

Use balanced randomized or alternating blocks, identical warmup, a pinned workload seed and repeated runs. Preserve failed, fallback and slow queries. Show medians and uncertainty for paired effects; report tail latency from individual request samples with the sample count and workload concurrency.

As a practical proposed minimum for a stable p99 investigation, collect enough independent workload observations to populate the tail substantially; 10,000 requests provide only about 100 observations above the empirical 99th-percentile threshold, and dependence or workload skew can require many more. Do not claim a tight tail estimate from the current small CPU-batch runs.

Every benchmark bundle should include:

```text
source/merge SHA and actual tested binary SHA
server, extension, compiler and build flags
hardware/VM/CPU feature and resource limits
all relevant PostgreSQL and PIN settings
schema, index definitions and analysis/scoring profile
corpus identity and query trace
maintenance/cache state and offered write rate
full result-identity/score checks
plans, raw per-run timings and work counters
CPU/profile metadata and lost samples
WAL/storage/debt before and after, including drain cost
reproduction commands and checksums
```

Interpret ablations on one binary and one data state separately from cross-build comparisons. The existing dirty-page off/on experiment is a good model of isolating one mechanism; the cross-build table answers a different question. [E05]

### 17.6 Definition of TIN-level qualification

Use a matrix, not one number:

```text
feature semantics and coverage
  × corpus/query class
  × result consumer: rows / count / top-k
  × read-only or concurrent-write state
  × memory/cache budget
  × latency, total CPU, reads, WAL, storage and maintenance debt
```

The desired 10x-over-GIN result is a per-class measured goal. TIN-level performance additionally requires a real TIN comparison on agreed workloads, not merely hitting the same ratio against GIN on a different fixture. State which classes reach the target, which regress, and which are bounded by PostgreSQL heap/executor work or by unavoidable output size.

Feature parity means matching documented behavior and error/fallback contracts, including operational behavior. It does not require guessing TIN's private implementation. Where TIN documentation leaves semantics ambiguous, retain a conformance test and record the observed version-specific behavior before advertising compatibility.

## 18. Remaining design risks and the final recommendation

### 18.1 Risks that measurement or implementation must resolve

**Packing versus update contention.** Shared arenas can replace space waste with hot-page contention. Use measured contention and promotion thresholds to choose arena allocation lanes; do not assume packing alone scales writes.

**Retained-generation retirement fanout.** The proposed reader protocol avoids a query-duration global barrier, but long readers can increase both retained storage and VACUUM's update set. Bound and expose that cost. If the registry becomes the bottleneck, evaluate a shared versioned retirement structure only with an equally explicit lifetime proof; do not replace one bottleneck with unreviewed lock-free machinery.

**Micro-domain growth under coordinate reuse.** Never-reused coordinates simplify correctness but can create many domains under churn. Compaction can consolidate domains only after proving incarnation/liveness compatibility. Measure domain count and merge cost, and retain stable owner identity while this behavior is being qualified.

**Score-bound usefulness.** Coarse TF/length bounds may be very loose on broad queries or long documents. Start safe, measure bound rejection rates and only then add finer impact metadata. Native scoring can still help by avoiding heap text work, but a near-zero skip rate is not a block-max success.

**Statistics overhead.** A coherent epoch must cover both immutable data and the captured mutable frontier without double-counting. Excessively frequent rebuilds of exact physical statistics can erase write gains; excessively stale statistics must not be silently described as current. Define the epoch policy and expose it.

**Feature-driven worst cases.** Full span witnesses, large regex/fuzzy expansions, wide projections, deep offsets and exact count plus rank can require substantial work even with an excellent index. Make the work explicit and bounded; do not invent a semantics-changing shortcut to meet a benchmark target.

**Platform and production surface.** The initial native-heap/UTF-8/8 KiB/toolchain envelope is intentional. Broader platforms, arbitrary table AMs, replicas and concurrent DDL need their own tests. Feature flags do not convert unqualified behavior into production support.

### 18.2 Final recommendation

The highest-value next phase is **not another isolated SIMD or syscall patch**. It is the transition from redundant, pointer-heavy storage to an authoritative packed index whose membership, frequency, position and lifecycle layers can be accessed independently.

Start with packed canonical storage and observable read work because the supplied census and profiles justify them. Build exhaustive SQL BM25 alongside that work because ranked execution is a missing capability, not merely a slow existing one. Then use the primary format to support selective positions, safe competitive pruning and bounded maintenance. Replace the global count lifetime barrier only after the retained-generation retirement protocol is implemented and tested.

That sequence preserves the demonstrated phrase mechanism, addresses the demonstrated storage pathology, and creates the missing ranked-search and concurrency mechanisms. It is a concrete route toward the requested performance and feature target. The design review and mathematical audit improve its credibility; only native implementation, lifecycle qualification and matched measurements can establish the performance result.

## 19. Source register

All PIN code/evidence links below are pinned to the reviewed archive commit. A benchmark's own tested-binary revision can differ and is recorded in its raw data. PostgreSQL source citations use commit `724edf9bde9d356724ad384a2e196edc3c9f80f7`, verified through the official repository tag `REL_18_6`; documentation links use the PostgreSQL 18 manual. PlanetScale documentation was reviewed on 26 September 2026 and may change independently of a particular deployed TIN version.

The reference labels distinguish **current PIN code (C)**, **supplied experiment/design evidence (E)**, **PostgreSQL/host primary sources (P)**, **PlanetScale primary sources (T)**, and **Lucene/Rust primary sources (L/R)**. Proposed algorithms and protocols are this design's recommendations; a linked source does not imply that the source implements every proposal here.

### 19.1 PIN code and current implementation

| Reference | Source and purpose |
|---|---|
| [C00] | PIN reviewed merge commit |
| [C01] | Supported envelope, defaults and limitations |
| [C02] | Canonical document publication and per-term linking |
| [C03] | Canonical page layouts and posting records |
| [C04] | Canonical sealing and compaction |
| [C05] | Grouped candidate evaluation and payload loading |
| [C06] | Grouped descriptors, inline roots and bitmap records |
| [C07] | Mutable frontier handling |
| [C08] | PostgreSQL grouped maintenance and experimental gates |
| [C09] | Rust grouped count integration |
| [C10] | Custom count planner, visibility and executor boundary |
| [C11] | PostgreSQL storage, locking, WAL and cost boundary |
| [C12] | Current query syntax, representation and limits |
| [C13] | Document validation and inline indexed phrase proof |
| [C14] | Exhaustive BM25/statistics/top-k oracle |
| [C15] | Current Unicode analysis profile |
| [C16] | Grouped scalar Boolean kernels |
| [C17] | PostgreSQL index AM callbacks and capabilities |
| [C18] | Portable and dispatched SIMD slice kernels |
| [C19] | Grouped immutable build |
| [C20] | Grouped VACUUM/retirement implementation |

### 19.2 Supplied benchmark, profile and design evidence

| Reference | Source and purpose |
|---|---|
| [E01] | Latest phrase experiment and its limits |
| [E02] | Same-run one-pass raw phrase batches |
| [E03] | First-pass CPU-clock profile, not a one-pass profile |
| [E04] | Merged PR 26 controls and lifecycle measurements |
| [E05] | PR 25 rework, visibility ablation and size analysis |
| [E06] | Physical page census |
| [E07] | Earlier native CPU-profile evidence and environment |
| [E08] | Existing TIN-oriented CPU architecture document |
| [E09] | Existing PIN implementation plan |
| [E10] | Repository PostgreSQL API evidence ledger |
| [E11] | Existing feature roadmap |
| [E12] | Latest raw evidence SHA-256 ledger |

### 19.3 PostgreSQL and host primary sources

| Reference | Source and purpose |
|---|---|
| [P01] | Index scanning contracts |
| [P02] | Index locking and concurrent operation |
| [P03] | Heap-only tuple behavior |
| [P04] | Visibility map semantics |
| [P05] | Index-only scan VM ordering and predicate locking |
| [P06] | Native heap index fetch and HOT visibility |
| [P07] | Generic WAL records and restrictions |
| [P08] | Custom WAL resource managers |
| [P09] | Custom scan provider interface |
| [P10] | Custom scan execution methods |
| [P11] | GIN design, consistency and pending-list maintenance |
| [P12] | Hot standby visibility and recovery conflicts |
| [P13] | Linux scheduler statistics definitions |

### 19.4 PlanetScale primary sources

| Reference | Source and purpose |
|---|---|
| [T01] | TIN architecture and published benchmark context |
| [T02] | Search-engine storage and execution mechanisms |
| [T03] | TIN setup, SQL surface and development compatibility |
| [T04] | TINQL query families and operator semantics |
| [T05] | Scoring modes, parameters, score context and visibility |
| [T06] | Index options and tokenizer configuration |
| [T07] | Maintenance and operational guidance |
| [T08] | Current public feature and operational limitations |
| [T09] | HN application query rewriting and fallback |
| [T10] | Highlighting and witness behavior |

### 19.5 Independent implementation references

| Reference | Source and purpose |
|---|---|
| [L01] | Lucene BM25 similarity contract |
| [L02] | Lucene WAND conservative score-bound rounding |
| [R01] | Rust runtime CPU feature detection |

<!-- Pinned and official source destinations. -->

[C00]: https://github.com/YogeshPandar/PIN/commit/e0432821d939b1eac873dff3b4a2ff06a3aa6df1 "PIN reviewed merge commit"
[C01]: https://github.com/YogeshPandar/PIN/blob/e0432821d939b1eac873dff3b4a2ff06a3aa6df1/README.md "Supported envelope, defaults and limitations"
[C02]: https://github.com/YogeshPandar/PIN/blob/e0432821d939b1eac873dff3b4a2ff06a3aa6df1/crates/pin-core/src/mutable/writer.rs#L131-L181 "Canonical document publication and per-term linking"
[C03]: https://github.com/YogeshPandar/PIN/blob/e0432821d939b1eac873dff3b4a2ff06a3aa6df1/crates/pin-core/src/mutable/page.rs "Canonical page layouts and posting records"
[C04]: https://github.com/YogeshPandar/PIN/blob/e0432821d939b1eac873dff3b4a2ff06a3aa6df1/crates/pin-core/src/mutable/compact.rs "Canonical sealing and compaction"
[C05]: https://github.com/YogeshPandar/PIN/blob/e0432821d939b1eac873dff3b4a2ff06a3aa6df1/crates/pin-core/src/mutable/grouped/scan.rs#L247-L317 "Grouped candidate evaluation and payload loading"
[C06]: https://github.com/YogeshPandar/PIN/blob/e0432821d939b1eac873dff3b4a2ff06a3aa6df1/crates/pin-core/src/mutable/grouped/storage.rs#L379-L466 "Grouped descriptors, inline roots and bitmap records"
[C07]: https://github.com/YogeshPandar/PIN/blob/e0432821d939b1eac873dff3b4a2ff06a3aa6df1/crates/pin-core/src/mutable/grouped/frontier.rs "Mutable frontier handling"
[C08]: https://github.com/YogeshPandar/PIN/blob/e0432821d939b1eac873dff3b4a2ff06a3aa6df1/crates/pin-pg/src/grouped.rs "PostgreSQL grouped maintenance and experimental gates"
[C09]: https://github.com/YogeshPandar/PIN/blob/e0432821d939b1eac873dff3b4a2ff06a3aa6df1/crates/pin-pg/src/grouped_count.rs "Rust grouped count integration"
[C10]: https://github.com/YogeshPandar/PIN/blob/e0432821d939b1eac873dff3b4a2ff06a3aa6df1/crates/pin-pg/cshim/pin_count.c "Custom count planner, visibility and executor boundary"
[C11]: https://github.com/YogeshPandar/PIN/blob/e0432821d939b1eac873dff3b4a2ff06a3aa6df1/crates/pin-pg/cshim/pin_storage.c "PostgreSQL storage, locking, WAL and cost boundary"
[C12]: https://github.com/YogeshPandar/PIN/blob/e0432821d939b1eac873dff3b4a2ff06a3aa6df1/crates/pin-core/src/query.rs "Current query syntax, representation and limits"
[C13]: https://github.com/YogeshPandar/PIN/blob/e0432821d939b1eac873dff3b4a2ff06a3aa6df1/crates/pin-core/src/mutable/document.rs "Document validation and inline indexed phrase proof"
[C14]: https://github.com/YogeshPandar/PIN/blob/e0432821d939b1eac873dff3b4a2ff06a3aa6df1/crates/pin-core/src/rank.rs "Exhaustive BM25/statistics/top-k oracle"
[C15]: https://github.com/YogeshPandar/PIN/blob/e0432821d939b1eac873dff3b4a2ff06a3aa6df1/crates/pin-core/src/analysis.rs "Current Unicode analysis profile"
[C16]: https://github.com/YogeshPandar/PIN/blob/e0432821d939b1eac873dff3b4a2ff06a3aa6df1/crates/pin-kernels/src/grouped.rs "Grouped scalar Boolean kernels"
[C17]: https://github.com/YogeshPandar/PIN/blob/e0432821d939b1eac873dff3b4a2ff06a3aa6df1/crates/pin-pg/src/am.rs "PostgreSQL index AM callbacks and capabilities"
[C18]: https://github.com/YogeshPandar/PIN/blob/e0432821d939b1eac873dff3b4a2ff06a3aa6df1/crates/pin-kernels/src/lib.rs "Portable and dispatched SIMD slice kernels"
[C19]: https://github.com/YogeshPandar/PIN/blob/e0432821d939b1eac873dff3b4a2ff06a3aa6df1/crates/pin-core/src/mutable/grouped/build.rs "Grouped immutable build"
[C20]: https://github.com/YogeshPandar/PIN/blob/e0432821d939b1eac873dff3b4a2ff06a3aa6df1/crates/pin-core/src/mutable/grouped/vacuum.rs "Grouped VACUUM/retirement implementation"
[E01]: https://github.com/YogeshPandar/PIN/blob/e0432821d939b1eac873dff3b4a2ff06a3aa6df1/docs/runs/2026-09-26-phrase-cpu/README.md "Latest phrase experiment and its limits"
[E02]: https://github.com/YogeshPandar/PIN/blob/e0432821d939b1eac873dff3b4a2ff06a3aa6df1/docs/runs/2026-09-26-phrase-cpu/raw/phrase-onepass/result.json "Same-run one-pass raw phrase batches"
[E03]: https://github.com/YogeshPandar/PIN/blob/e0432821d939b1eac873dff3b4a2ff06a3aa6df1/docs/runs/2026-09-26-phrase-cpu/raw/phrase-first-perf/report.txt "First-pass CPU-clock profile, not a one-pass profile"
[E04]: https://github.com/YogeshPandar/PIN/blob/e0432821d939b1eac873dff3b4a2ff06a3aa6df1/docs/runs/2026-09-26-phrase-cpu/raw/merged-baseline/summary.json "Merged PR 26 controls and lifecycle measurements"
[E05]: https://github.com/YogeshPandar/PIN/blob/e0432821d939b1eac873dff3b4a2ff06a3aa6df1/docs/runs/2026-09-25-pr25-rework/README.md "PR 25 rework, visibility ablation and size analysis"
[E06]: https://github.com/YogeshPandar/PIN/blob/e0432821d939b1eac873dff3b4a2ff06a3aa6df1/docs/runs/2026-09-25-pr25-rework/page-layout.json "Physical page census"
[E07]: https://github.com/YogeshPandar/PIN/blob/e0432821d939b1eac873dff3b4a2ff06a3aa6df1/docs/runs/2026-09-24-g9-cpu-profile/README.md "Earlier native CPU-profile evidence and environment"
[E08]: https://github.com/YogeshPandar/PIN/blob/e0432821d939b1eac873dff3b4a2ff06a3aa6df1/TIN_CPU_ARCHITECTURE.md "Existing TIN-oriented CPU architecture document"
[E09]: https://github.com/YogeshPandar/PIN/blob/e0432821d939b1eac873dff3b4a2ff06a3aa6df1/pin_plan.md "Existing PIN implementation plan"
[E10]: https://github.com/YogeshPandar/PIN/blob/e0432821d939b1eac873dff3b4a2ff06a3aa6df1/docs/api-evidence.md "Repository PostgreSQL API evidence ledger"
[E11]: https://github.com/YogeshPandar/PIN/blob/e0432821d939b1eac873dff3b4a2ff06a3aa6df1/docs/fts-roadmap.md "Existing feature roadmap"
[E12]: https://github.com/YogeshPandar/PIN/blob/e0432821d939b1eac873dff3b4a2ff06a3aa6df1/docs/runs/2026-09-26-phrase-cpu/RAW_SHA256.txt "Latest raw evidence SHA-256 ledger"
[L01]: https://lucene.apache.org/core/10_3_0/core/org/apache/lucene/search/similarities/BM25Similarity.html "Lucene BM25 similarity contract"
[L02]: https://github.com/apache/lucene/blob/releases/lucene/10.3.0/lucene/core/src/java/org/apache/lucene/search/WANDScorer.java#L40-L135 "Lucene WAND conservative score-bound rounding"
[P01]: https://www.postgresql.org/docs/18/index-scanning.html "Index scanning contracts"
[P02]: https://www.postgresql.org/docs/18/index-locking.html "Index locking and concurrent operation"
[P03]: https://www.postgresql.org/docs/18/storage-hot.html "Heap-only tuple behavior"
[P04]: https://www.postgresql.org/docs/18/storage-vm.html "Visibility map semantics"
[P05]: https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/backend/executor/nodeIndexonlyscan.c#L120-L245 "Index-only scan VM ordering and predicate locking"
[P06]: https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/backend/access/heap/heapam_handler.c#L115-L173 "Native heap index fetch and HOT visibility"
[P07]: https://www.postgresql.org/docs/18/generic-wal.html "Generic WAL records and restrictions"
[P08]: https://www.postgresql.org/docs/18/custom-rmgr.html "Custom WAL resource managers"
[P09]: https://www.postgresql.org/docs/18/custom-scan.html "Custom scan provider interface"
[P10]: https://www.postgresql.org/docs/18/custom-scan-execution.html "Custom scan execution methods"
[P11]: https://www.postgresql.org/docs/18/gin.html "GIN design, consistency and pending-list maintenance"
[P12]: https://www.postgresql.org/docs/18/hot-standby.html "Hot standby visibility and recovery conflicts"
[P13]: https://www.kernel.org/doc/html/latest/scheduler/sched-stats.html "Linux scheduler statistics definitions"
[R01]: https://doc.rust-lang.org/std/macro.is_x86_feature_detected.html "Rust runtime CPU feature detection"
[T01]: https://planetscale.com/blog/introducing-tin "TIN architecture and published benchmark context"
[T02]: https://planetscale.com/blog/anatomy-of-a-postgres-search-engine "Search-engine storage and execution mechanisms"
[T03]: https://planetscale.com/docs/postgres/search/get-started "TIN setup, SQL surface and development compatibility"
[T04]: https://planetscale.com/docs/postgres/search/tinql "TINQL query families and operator semantics"
[T05]: https://planetscale.com/docs/postgres/search/scoring "Scoring modes, parameters, score context and visibility"
[T06]: https://planetscale.com/docs/postgres/search/reference/indexes "Index options and tokenizer configuration"
[T07]: https://planetscale.com/docs/postgres/search/operations "Maintenance and operational guidance"
[T08]: https://planetscale.com/docs/postgres/search/reference/limitations "Current public feature and operational limitations"
[T09]: https://planetscale.com/blog/searching-hn-with-tin "HN application query rewriting and fallback"
[T10]: https://planetscale.com/docs/postgres/search/highlighting "Highlighting and witness behavior"
