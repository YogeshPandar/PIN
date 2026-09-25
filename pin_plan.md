# Pin
## PostgreSQL-native, Rust-first full-text search: research, architecture, and implementation blueprint

**Research date:** 17 September 2026  
**Document status:** Technical design and implementation charter; not an implemented or benchmarked extension  
**Initial target:** PostgreSQL 18, upstream heap storage, UTF-8, 64-bit Linux  
**Engineering objective:** High-throughput, low-latency transactional search with bounded memory and PostgreSQL-native durability  
**Project name:** `Pin`; PostgreSQL extension and access method: `pin`

---

## Executive decision

**Build Pin as its own PostgreSQL index access method, with a PostgreSQL-managed storage layer, a mostly safe Rust search engine, and narrowly scoped CustomScan acceleration.** Do not start by embedding an independent search server inside PostgreSQL. Do not start by copying a proprietary implementation, inventing another MVCC system, or writing SIMD before establishing the storage and visibility contracts.

The proposed engine uses physical heap tuple identities, adaptive postings organized by heap-page groups, a searchable mutable tier, compressed immutable segments, lazy positional decoding, and bounded top-k execution. PostgreSQL remains responsible for transactions, snapshots, heap visibility, relation lifecycle, buffer management, and WAL replay. Pin is responsible for the searchable representation and for proving that every shortcut preserves those contracts.

The highest-leverage performance opportunities are to avoid work: prune whole groups before decoding postings, avoid materializing entire result sets, decode positions only when required, avoid fetching columns that will not be returned, and avoid heap visibility checks only when a carefully validated visibility-map protocol permits it. Unsafe Rust is a tool for specific demonstrated bottlenecks, not the organizing principle of the product.

**Two optimizations are deliberately gated:** heap-free exact counts and copy-avoiding segment merges. Both are plausible targets, but their concurrency and recovery protocols need implementation-level proof and adversarial tests. The first production-capable path must remain correct without them. Concurrent Pin index reads on hot standbys have a separate replay/lifetime qualification gate; physical recovery and post-promotion use are distinct from that feature.

“Tin-class performance” is a benchmark objective, not an established result. “Zero bugs” is not a credible advance guarantee. Pin's enforceable standard is no known data-loss, wrong-result, security, or memory-safety defect at release, reproducible testing, bounded resource use, transparent limitations, and aggressive regression prevention.

### How to read this document

**PostgreSQL contract** means a requirement established by the cited official documentation or source. **Pin decision** means a proposed design choice. **Experiment / gate** means a promising choice that must earn its place through measurement and correctness evidence. SQL examples describe the intended API, not commands that work today.

The research includes all six sections of PostgreSQL 18's **Index Access Method Interface Definition**, the Internals map, and selected storage, executor, transaction, WAL, and Rust references. It does not constitute a line-by-line audit of the entire PostgreSQL source tree. Source paths on `REL_18_STABLE` and upstream `main`/`develop` are research references; implementation must replace them with immutable release commits in its evidence ledger. [PG-Internals] [PG-AM] [PG-AM-API] [PG-AM-Functions] [PG-AM-Scan] [PG-AM-Lock] [PG-AM-Unique] [PG-AM-Cost]

## Contents

1. [Product scope and success criteria](#1-product-scope-and-success-criteria)
2. [What Tin establishes—and what it does not](#2-what-tin-establishesand-what-it-does-not)
3. [PostgreSQL internals: the map Pin needs](#3-postgresql-internals-the-map-pin-needs)
4. [Non-negotiable invariants](#4-non-negotiable-invariants)
5. [SQL semantics, analyzers, and public API](#5-sql-semantics-analyzers-and-public-api)
6. [System architecture and ownership boundaries](#6-system-architecture-and-ownership-boundaries)
7. [Physical identity, HOT, and tuple-slot reuse](#7-physical-identity-hot-and-tuple-slot-reuse)
8. [On-disk layout and adaptive postings](#8-on-disk-layout-and-adaptive-postings)
9. [Insertion, publication, and searchable mutable state](#9-insertion-publication-and-searchable-mutable-state)
10. [WAL, recovery, and persistent state machines](#10-wal-recovery-and-persistent-state-machines)
11. [VACUUM, liveness, generations, and compaction](#11-vacuum-liveness-generations-and-compaction)
12. [Boolean, phrase, and streaming execution](#12-boolean-phrase-and-streaming-execution)
13. [Visibility-aware execution and exact counts](#13-visibility-aware-execution-and-exact-counts)
14. [BM25, pruning, and exact visible top-k](#14-bm25-pruning-and-exact-visible-top-k)
15. [Index AM callback and capability contract](#15-index-am-callback-and-capability-contract)
16. [Planner and CustomScan integration](#16-planner-and-customscan-integration)
17. [Rust, FFI, memory, and unsafe standards](#17-rust-ffi-memory-and-unsafe-standards)
18. [SIMD, I/O, parallelism, and throughput](#18-simd-io-parallelism-and-throughput)
19. [Memory budgets and operational controls](#19-memory-budgets-and-operational-controls)
20. [Repository structure and file responsibilities](#20-repository-structure-and-file-responsibilities)
21. [Implementation phases and acceptance gates](#21-implementation-phases-and-acceptance-gates)
22. [Correctness, crash, security, and concurrency testing](#22-correctness-crash-security-and-concurrency-testing)
23. [Benchmarking and evidence standards](#23-benchmarking-and-evidence-standards)
24. [Packaging, upgrades, replication, and operations](#24-packaging-upgrades-replication-and-operations)
25. [Open-source licensing and contributor governance](#25-open-source-licensing-and-contributor-governance)
26. [Implementation documentation policy and API register](#26-implementation-documentation-policy-and-api-register)
27. [Design review: rejected shortcuts and open decisions](#27-design-review-rejected-shortcuts-and-open-decisions)
28. [Release definition and technical-lead priorities](#28-release-definition-and-technical-lead-priorities)
29. [Primary-source register](#29-primary-source-register)

---

## 1. Product scope and success criteria

### 1.1 The first product

Pin should initially be a transactional full-text index for PostgreSQL `text`, with Boolean matching, exact phrase matching, versioned analysis, and BM25-based ranking. The query predicate must have a correct sequential-evaluation implementation so that index choice never changes query meaning.

The initial supported environment is deliberately narrow: PostgreSQL 18; built-in heap table AM; UTF-8 databases; 64-bit Linux; standard 8 KiB PostgreSQL pages. Build-time and runtime checks must reject incompatible assumptions. x86-64 has a scalar baseline and later CPU-dispatched acceleration; AArch64 earns support through the same differential and integration suites. Support for another PostgreSQL major version is a port, not a feature flag assumed to work.

The first index is single-column and nonunique. Expression and partial indexes are valuable and should follow PostgreSQL's normal build and predicate machinery, with dedicated tests. Partitioned tables eventually use one physical index per leaf; identical TIDs in different leaves are different identities. Initial optimized ranking/count paths can remain restricted to one eligible base relation while ordinary PostgreSQL plans handle more complex queries.

Do not make JSON indexing, vectors, distributed search, custom table storage, fuzzy matching, arbitrary regex, multilingual stemming packs, covering stored documents, or logical document-ID management prerequisites for the first release. A strong transactional core is more valuable than a large feature list with fragile semantics.

### 1.2 Supported semantics versus supported acceleration

An optimization not being available must usually mean **use the correct fallback**, not reject an otherwise valid SQL query.

| Query or environment | Baseline behavior | Acceleration requirement |
|---|---|---|
| Boolean predicate on indexed text | Ordinary AM/heap path or sequential predicate | Page-group posting operations |
| Exact phrase | Exact positions or SQL recheck | Lazy positional verification |
| `COUNT(*)` of a search predicate | Core aggregate over visible matching rows | Narrow upper-plan count node, with visibility proof |
| Ranked `ORDER BY ... LIMIT` | Exhaustive scoring and core sort/limit | Exact eligible top-k CustomScan |
| Residual SQL filters | Core executor evaluates them | Push down only with equivalence proof |
| RLS/security-barrier queries | Preserve PostgreSQL security evaluation | No custom shortcut initially |
| Serializable transactions | Ordinary correctly integrated index path | Custom path disabled until SSI audit |
| Row locking, modification, EvalPlanQual | Ordinary executor path | No custom shortcut initially |
| Hot-standby/recovery-time Pin index reads | Disabled until replay/lifetime protocol is qualified; see section 24.3 | Explicit standby conflict/retention proof |
| Unsupported heap layout/table AM | Explicit diagnostic before unsafe access | Separate supported port |

This matrix is a proposed product contract, not a claim of implemented capabilities.

### 1.3 Performance objectives must be multidimensional

Measure latency, sustained throughput, memory, index size, write amplification, and foreground-write latency together. A read benchmark that consumes all RAM, starves VACUUM, or accumulates compaction debt is not success.

The performance dossier must answer: how many bytes and pages are read per query; how many candidate roots are considered; how many are heap-fetched; how much positional data is decoded; how much work is pruned; how much private memory is retained per backend; how many WAL bytes are written per document; and whether the maintenance backlog is stable under sustained load.

Set numerical release targets after the first reproducible baseline. The proposed regression policy is to investigate a repeatable 5% change in a primary throughput or tail-latency metric and to reject unexplained large regressions. That percentage is an engineering trigger, not a claim about achievable performance or statistical significance.

### 1.4 Toolchain decision

As researched on 17 September 2026, official PostgreSQL pages identify 18.6 as the current PostgreSQL 18 minor, and Rust's official release announcement identifies Rust 1.98.1. Those are candidate validation targets, not a tested Pin compatibility matrix. [PG-Preload] [Rust-Release]

Use `pgrx` as the default PostgreSQL binding/tooling layer, with narrowly reviewed direct `pg_sys` calls and a small C shim when C macros, inline functions, or error boundaries make that the most reliable choice. The retrieved pgrx documentation and its development manifest did not resolve to one unambiguous matching release across all packages. Therefore **G0 must select and test one immutable, matching set of `pgrx`, `pgrx-pg-sys`, and `cargo-pgrx`, then commit the lockfile and exact toolchain**. Do not put `latest` or a floating development branch into the release specification. [Pgrx] [Pgrx-PgSys] [Pgrx-Manifest] [Cargo-Lock]

## 2. What Tin establishes—and what it does not

PlanetScale's article, published 16 September 2026, describes TIN using physical tuple identifiers, two-level page-oriented bitmaps, live-document tracking, custom execution, and mutable/immutable segments. It reports substantial benchmark gains and describes merge behavior that avoids rewriting some postings. These are useful public design clues, not access to its complete implementation. [Tin-Blog]

The published tests include a 150-million-document Stack Exchange corpus and a specified PostgreSQL/hardware configuration. Treat all reported results as vendor measurements. Ranking, workload semantics, feature coverage, hardware allocation, and maintenance state must be matched before claiming a fair comparison. The accompanying public benchmark repository is useful experimental input, not an independent validation of the server internals. [Tin-Blog] [Tin-Bench]

**Pin's independent interpretation:** physical locality and execution specialization are promising. However, implement our own on-disk format, recovery state machines, query semantics, and safety proofs. Do not infer an undocumented lock protocol, index-only visibility guarantee, precise memory budget, or proprietary source-code structure from a blog. PlanetScale's product documentation establishes its product interface; Pin's API need not copy that interface when a different design is safer. [Tin-Docs] [Tin-Start]

The rest of this document is an original proposed architecture grounded primarily in PostgreSQL and Rust contracts. It is not a reconstruction of Tin's private code.

## 3. PostgreSQL internals: the map Pin needs

### 3.1 Follow a query through the server

The relevant path is SQL parsing and analysis, rewrite/security processing, planning, executor initialization, scan execution, tuple qualification, projection, and aggregation/sorting. A search engine embedded at the scan layer does not acquire permission to bypass the rest of SQL. PostgreSQL's internals overview and CustomScan interfaces provide the integration map. [PG-Overview] [PG-CustomScan]

For Pin, the planner should recognize a supported operator and estimate an ordinary index path. Later, hooks may offer a better custom path. The executor provides the statement snapshot and lifetime. The AM produces candidate root TIDs, while the heap/table AM resolves visible tuples. Residual filters, security conditions, joins, row locking, and output expressions remain governed by the plan.

**Implication:** design `Candidate`, `VisibleMatch`, and `OutputRow` as different stages. A posting hit is not yet a visible row, and a visible text match is not necessarily eligible for a final SQL top-k.

### 3.2 Heap pages and line pointers

PostgreSQL pages have a standard header and item identifiers; tuple position is addressed through the line pointer rather than by retaining a raw address inside a page. Pin must use supported page/buffer access patterns and respect page layout metadata. [PG-Page]

**Pin decision:** group postings by heap block and line-pointer offset. This makes common-term Boolean work amenable to small bitmaps and makes visibility checks naturally batchable by heap page. Do not assume that a logical document retains a fixed address across updates or rewrites.

### 3.3 Buffer management

Shared buffers are PostgreSQL's principal in-server cache for relation pages. A buffer pin protects buffer residency and participates in some cleanup protocols; it is not equivalent to holding a content lock or obtaining an immutable Rust slice. The buffer manager's source documentation is part of the required storage audit. [PG-Buffer-Source]

**Pin decision:** store index data in PostgreSQL index-relation blocks and use shared buffers. Keep small decoded query metadata privately; avoid duplicating the entire compressed index in a second unaccounted process cache. Borrow bytes only while the lock/lifetime contract justifies the borrow. Copying a small header can be preferable to retaining a long-lived pin.

### 3.4 MVCC, HOT, and VACUUM

PostgreSQL controls which tuple version is visible to a snapshot. HOT can keep an index entry pointing at a root while a later tuple in the same page is the visible version. VACUUM coordinates removal of obsolete index references before heap slots become reusable. These are the core constraints on a physical-TID search index. [PG-MVCC] [PG-HOT] [PG-HOT-Source] [PG-Vacuum-Source]

**Pin decision:** use PostgreSQL's visibility machinery rather than reconstructing xmin/xmax rules in Rust. Treat our liveness metadata as an index-maintenance fact, not a visibility oracle. Preserve old versions required by transactions, even when that creates maintenance pressure.

### 3.5 Visibility map

The heap visibility map records all-visible and all-frozen information using two bits per heap page. Its absence of an all-visible indication is a reason to check the heap, not evidence that every row is invisible. [PG-VM]

**Pin decision:** acquire VM status through PostgreSQL's access protocol first. A batch of correctly acquired page facts may then feed Rust bitmap kernels. Direct vector loads from VM storage are not an acceptable initial shortcut.

### 3.6 WAL and storage lifecycle

Extensions can use generic WAL or a custom resource manager. Generic WAL supports physical changes to standard PostgreSQL pages without requiring an extension-specific redo routine. PostgreSQL's relation layout and lifecycle should remain the authority over index files. [PG-WAL-Extensions] [PG-Generic-WAL] [PG-Files]

**Pin decision:** start with generic WAL, persistent reachability metadata, and explicit publication states. Do not create an independently committed filesystem index, then hope transaction callbacks keep it aligned with the heap.

### 3.7 Why not just a GIN operator class?

GIN already provides inverted indexing and is an important correctness and performance baseline. Its internal posting representation and update strategy are controlled by GIN, not freely replaceable by an operator class. [PG-GIN] [PG-FTS-Indexes]

Pin's reason to exist is control over physical page-oriented containers, segment lifecycle, positional/scoring data, and streaming ranked/count execution. A custom AM has a larger correctness burden; accept it only because these controls are central to the product, not because custom code is inherently faster.

### 3.8 TOAST, large text, and expression evaluation

Indexed `text` may be compressed or stored out of line. Obtain values through PostgreSQL's varlena/TOAST interfaces; do not interpret a `Datum` as a flat Rust string without detoasting and validating the representation. [PG-TOAST] [PG-C-Functions]

**Pin decision:** analyze each indexed value once per insertion/build callback, borrow normalized input where safe, and avoid repeated detoasting for each term. Put explicit limits on document bytes, terms, query depth, and positions. A rejected oversized document must abort its change cleanly, never leave an accepted row silently unindexed.

## 4. Non-negotiable invariants

These are **Pin's design requirements**. Every storage or executor change must identify which invariants it preserves and how they are tested.

| ID | Invariant | Typical failure it prevents |
|---|---|---|
| I01 | Every successfully indexed eligible heap version is discoverable once PostgreSQL permits a scan to observe it. | Missing own writes; delayed-searchability bugs |
| I02 | A published document has complete Boolean, TF, length, and promised positional data. | Half-indexed documents; incorrect phrases |
| I03 | A root TID is interpreted only within its relation physical generation and source incarnation. | Cross-relation confusion and slot reuse |
| I04 | Heap visibility comes from PostgreSQL or a proven equivalent fast-path protocol. | Uncommitted or stale rows returned |
| I05 | Liveness removal uses PostgreSQL VACUUM authority and reaches every reader-reachable copy. | Old terms resurrected after TID reuse |
| I06 | Each query's manifest gives complete, non-duplicated logical coverage of its searchable sources. | Missing or double-counted documents |
| I07 | Reclamation never frees a page or segment still reachable by a valid reader or recovery state. | Use-after-free, broken replay |
| I08 | No raw page reference outlives its buffer/content-validity guarantee. | Aliasing UB and stale reads |
| I09 | Durable metadata never points to uninitialized or incompletely durable payload. | Crash-created dangling references |
| I10 | Planner acceleration preserves SQL qualification, security, ordering, and limits. | Wrong rows or policy bypass |
| I11 | A top-k threshold is raised only by visible, fully eligible rows. | Invisible high scores hide valid results |
| I12 | Every pruning bound is conservative under the query's frozen scoring definition. | False-negative ranked results |
| I13 | Query, build, and maintenance memory are bounded and observable. | Backend multiplication exhausts memory |
| I14 | Cancel, ERROR, abort, and backend death release transient resources or leave reclaimable durable state. | Leaks, deadlocks, permanent maintenance debt |
| I15 | On-disk bytes are validated before unchecked decoding or pointer formation. | Corruption becomes memory unsafety |
| I16 | Optimized and reference paths implement the same operator/analyzer version. | Plan-dependent query semantics |
| I17 | Background workers improve performance but are not required for already accepted writes to be searchable. | Worker outage loses search results |
| I18 | Unsupported states fail explicitly; they never silently downgrade durability or correctness. | Misleading production guarantees |

## 5. SQL semantics, analyzers, and public API

### 5.1 Proposed user experience

The intended first API is deliberately explicit. These examples are a design target, not an existing extension installation procedure:

```sql
-- Proposed API. The server package and required preload configuration come first.
CREATE EXTENSION pin;

CREATE TABLE documents (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    body text NOT NULL,
    published boolean NOT NULL DEFAULT true
);

CREATE INDEX documents_body_pin
    ON documents USING pin (body pin.text_ops);

SELECT id
FROM documents
WHERE body OPERATOR(pin.@@@) pin.parse_query('rust AND postgres');

SELECT count(*)
FROM documents
WHERE body OPERATOR(pin.@@@) pin.parse_query('"buffer manager"');

SELECT id,
       pin.score(body, pin.parse_query('rust OR postgres'),
                 'documents_body_pin'::regclass) AS relevance
FROM documents
WHERE body OPERATOR(pin.@@@) pin.parse_query('rust OR postgres')
ORDER BY relevance DESC
LIMIT 20;
```

The operator name and exact signatures are subject to G0 API validation. Schema-qualified operators/functions avoid accidental `search_path` binding. Do not expose a session-global `score(ctid)` that means “the score from whichever search ran last.”

`pin.score` must have a correct exhaustive implementation using the provided document value, typed query, and explicit corpus context. Its optimized replacement is legal only when the planner proves that those inputs correspond to the indexed expression and active scan. Corpus-dependent scoring is not an immutable function.

### 5.2 Stable operator meaning

A dangerous design is to change tokenization through index reloptions while leaving `text @@@ query` unchanged. Then a sequential scan and index scan can disagree about the same SQL expression.

**Decision:** the first `pin.text_ops` binds one explicitly versioned analysis profile. `pin.parse_query` produces a typed, validated query carrying a semantic version/profile identifier. Storage tuning may be reloptions; analyzer identity may not be an invisible storage-only choice. Additional profiles require an explicit typed or expression-index API whose sequential meaning remains identical.

A query type should have a bounded, versioned binary encoding and stable text round-trip. PostgreSQL input/receive paths validate all lengths, node counts, enum values, and encoding. Do not rely on an unconstrained general-purpose serializer as the hot-path or long-term on-disk ABI.

### 5.3 Initial text analysis

Use a documented Unicode segmentation and normalization profile. UAX #29 defines segmentation behavior; UAX #15 defines normalization. Locale-specific dictionary segmentation and stemming are separate features, not automatic consequences of “Unicode support.” [Unicode-Segmentation] [Unicode-Normalization] [Unicode-ICU]

A reasonable initial Pin profile is Unicode word segmentation, an explicit normalization/case-folding policy, no hidden stopword list, and no implicit stemming. Decide the precise transformation order and supported Unicode data version in an ADR. Unicode normalization is not implemented by calling Rust's ordinary lowercase function. Existing maintained libraries may be used after source, license, and conformance review; “Rust-first” does not require reimplementing every Unicode table.

The analyzer must produce term bytes, token positions, and document length in one pass where practical. Preserve position gaps when filters are later introduced. An ASCII fast path must be bit-for-bit equivalent to the general implementation for its eligible input, not a different analyzer.

### 5.4 Query semantics that must be written down

Define Boolean precedence, parentheses, phrase quoting and escaping, repeated terms, pure negation, empty input, stopword-only input under future profiles, unknown terms, prefix expansion, maximum expansion, and maximum nesting. A bounded iterative parser is preferable to unbounded recursion on user input.

Pure `NOT term` must be evaluated against the indexed non-null document universe, including empty documents when the API defines them as eligible. It must not complement every possible TID bit. The SQL predicate should be strict for `NULL` inputs, so SQL's null semantics remain distinct from an empty document.

Prefix matching can initially be a bounded dictionary operation. Expansion beyond a configured resource limit should fail with a clear error or use a documented exact slower path. Never silently drop expanded terms and return incomplete results. Fuzzy, regex, and wildcard features wait until their worst-case work is bounded.

### 5.5 Native text versus `tsvector` compatibility

A future `tsvector`/`tsquery` compatibility mode must preserve PostgreSQL's actual representation semantics, including its documented position and size limits. It cannot recover positions that were already discarded during `tsvector` construction. [PG-FTS-Limits]

Native Pin text indexing may use wider positions and its own analysis profile. Keep that a separate promise. Do not claim PostgreSQL FTS compatibility solely because both systems implement Boolean search.

### 5.6 Diagnostic API

Plan read-only, privilege-aware inspection functions such as `pin.index_info(regclass)`, `pin.explain_query(...)`, and `pin.check_index(regclass, thorough => false)`. Diagnostics should report format/analyzer versions, segment counts, physical bytes, liveness debt, statistics epoch, and enabled fast paths. They must not expose raw document terms or corpus statistics across an authorization boundary by default.

Administrative compaction or verification functions require appropriate index/table ownership checks. Never accept an arbitrary filesystem path as an index identity.

## 6. System architecture and ownership boundaries

### 6.1 Four layers, not one large unsafe crate

```text
SQL operator / typed query / score / diagnostics
                  |
       PostgreSQL integration layer
  AM callbacks | planner paths | CustomScan
  snapshots | slots | resources | permissions
                  |
          Storage adapter and protocol
  buffers | WAL | manifest | publication | VACUUM
                  |
          Pure Rust search and codecs
  analysis | AST | containers | Boolean | positions
  BM25 | bounds | top-k | scalar/SIMD dispatch
```

The pure engine must run under ordinary Rust tests without a PostgreSQL installation. The storage adapter translates PostgreSQL-owned pages into short-lived validated views or private batches. The planner/executor layer owns SQL semantics. None of these layers should silently reach around another layer's contract.

### 6.2 Storage ownership

**PostgreSQL owns:** heap tuples, transaction status, snapshots, relation locks, relation files, shared buffers, WAL ordering/replay, physical replication infrastructure, and relation creation/removal.

**Pin owns:** index-page payloads, document publication metadata, term dictionaries, posting containers, positions, document lengths, generation manifests, safe liveness bookkeeping, query iterators, and cost/diagnostic metadata.

**The SQL caller owns:** the requested semantics, transaction, privileges, and result consumption rate. Pin must not convert a query into an independent transaction or silently change isolation to gain speed.

### 6.3 Proposed internal types

Use semantic wrappers, not interchangeable integers:

| Type | Meaning and constraints |
|---|---|
| `RelationGeneration` | Database/relation identity plus validated physical index generation; invalidated on rewrite/reindex/drop |
| `RootTid` | Valid heap block and root offset within that relation; never a global document ID |
| `VisibleTid` | Actual tuple version returned by the heap fetch; not substituted for the root key |
| `SegmentId` / `ManifestGeneration` | Monotonic, checked identifiers for index source ownership |
| `DocumentRef` | Segment/document-incarnation locator plus root TID; prevents bare-TID resurrection |
| `TermId` | Dictionary-local identifier; only meaningful with its dictionary/segment |
| `Position` / `TokenCount` | Explicit bounded integer domains; no silent truncation |
| `CandidateBatch` | Untrusted-for-visibility, bounded collection of possible roots |
| `VisibleMatch` | Heap-verified or fast-path-certified match, still subject to SQL eligibility |
| `EligibleMatch` | Visible match with every condition needed for local top-k already satisfied |
| `StatsEpoch` | Immutable scoring context; not a PostgreSQL snapshot and not an exact row count |
| `PinnedPage` / `LockedPageView` | Distinct resource and borrowing lifetimes |

Use `Result` for pure parsing/storage validation errors and translate them into PostgreSQL errors only at guarded boundaries. Avoid allocating formatted error strings in per-posting loops; preserve enough structured context for a useful diagnostic when an error is actually raised.

### 6.4 Preload and process model

**Proposed first deployment contract:** require `shared_preload_libraries = 'pin'` for the supported server configuration. This makes shared control-state initialization, hooks, generation registrations, and maintenance scheduling explicit. Validate that initialization happened and report a useful error otherwise. A no-preload reduced mode is a separate later feature, not an implicit promise.

The shared state is bounded coordination metadata, not the authoritative index contents. A restarted server reconstructs durable index state from relation pages and WAL. Query correctness cannot depend on a lost backend-private cache. PostgreSQL's preloading and background-worker contracts govern initialization and worker startup. [PG-Preload] [PG-BGWorker]

Use PostgreSQL processes for server work. No Tokio runtime, Rayon pool, or arbitrary Rust worker threads calling PostgreSQL APIs. A pure computation experiment on private immutable memory could eventually use threads, but it must demonstrate a benefit over PostgreSQL workers and cannot touch server pointers or resource state. It is not part of the initial architecture. [Pgrx-README]

## 7. Physical identity, HOT, and tuple-slot reuse

### 7.1 Encode coordinates explicitly

A PostgreSQL item pointer identifies a block and offset. Use PostgreSQL accessors or a narrow shim to convert it into Pin's validated coordinate type. Do not reinterpret the bytes of `ItemPointerData` as a packed native-endian 48-bit integer: C representation, block subfields, alignment, and disk endianness are distinct concerns. [PG-ItemPointer-Source]

Pin can internally compare a logical key assembled from validated `u32` block and `u16` offset values. Persist explicitly encoded fields. Invalid offsets, invalid block values, out-of-range line pointers, and arithmetic overflow are errors before a pointer or bitmap index is formed.

`ctid` is not a user-facing durable primary key. An UPDATE that is not HOT, `VACUUM FULL`, `CLUSTER`, or a table rewrite can change physical identity. Use a logical key for application pagination or durable references, and scope any internal physical tie-breaker to the active query/relation generation. [PG-HOT] [PG-Files]

### 7.2 Preserve the HOT root

The table-AM index-fetch API can follow a HOT chain, and it may change the TID passed to it to identify the actual tuple version. The exact-version tuple-fetch API is not a substitute for this index-fetch operation. [PG-TableAM-Source] [PG-HeapHandler-Source]

**Required implementation behavior:** retain `root_tid`; pass a copy to `table_index_fetch_tuple`; record any resulting physical version as `visible_tid`. Attach posting TF/positions to the indexed root/document incarnation. Do not accidentally look up scoring data using the changed visible TID.

During a build, use PostgreSQL's table index-build scan so that HOT-root mapping and visibility decisions come from the core. Do not scan a single convenient snapshot and create entries for whatever `ctid` happens to be returned. Concurrent builds have additional broken-HOT and validation behavior that the core source explicitly handles. [PG-HOT-Source] [PG-HeapHandler-Source]

### 7.3 A slot-reuse example every implementer must understand

```text
1. Root TID (heap block 42, offset 7) describes document A containing "alpha".
2. A becomes removable; PostgreSQL supplies that root to index VACUUM.
3. Pin removes A's live reference from every reader-reachable source.
4. PostgreSQL may now reuse that heap slot for document B containing "beta".
5. B is a new document incarnation, even though its coordinate is (42, 7).
```

A global bitmap keyed only by `(42,7)` cannot be cleared for A and then set for B while old `alpha` postings still interpret it as their own liveness bit. That would resurrect `alpha` for B. Liveness belongs to a document incarnation/source generation, with safe handling before cross-segment union.

Likewise, compaction must not combine A's positions with B's TF just because their TIDs compare equal. Removal and replacement are different facts, not a set-union opportunity.

### 7.4 Reader pin discipline

PostgreSQL distinguishes synchronous tuple-at-a-time scans from asynchronous bitmap scans. Its locking contract protects the transition from an index entry to the heap against VACUUM removal and slot reuse; MVCC bitmap scans have a different contract. [PG-AM-Lock]

For Pin's plain `amgettuple`, the proposed authoritative document-owner leaf is the pinning anchor. Acquire the relevant owner pin before trusting liveness, validate the document incarnation under the content protocol, and retain the pin while the returned item can still be used for its synchronous heap fetch. VACUUM must obtain the corresponding cleanup permission before making the root removable. A plain exclusive content lock that ignores reader pins is not enough.

This owner-leaf arrangement is a **Pin protocol to prove**, not a feature supplied automatically by `IndexAmRoutine`. It must be demonstrated equivalent to the required index/heap interlock. Until then, implement the ordinary MVCC bitmap path, not a tuple-at-a-time scan with guessed safety.

### 7.5 HOT-related decisions

Set `amsummarizing = false`: a per-tuple inverted index does not acquire BRIN-style summarizing semantics by placing tuples into a page bitmap. Treat `indexUnchanged` as a hint whose use must satisfy PostgreSQL's contract; it is not permission to skip a required new physical index entry. Do not infer visibility from an unchanged indexed value. [PG-AM-API] [PG-AM-Functions]

## 8. On-disk layout and adaptive postings

### 8.1 Format principles

The on-disk format is a public compatibility commitment. Write a format specification before optimizing its encoding. Its first version should contain:

| Page/payload family | Required purpose |
|---|---|
| Metapage | Magic, format version, supported feature bits, page-size expectation, generation, manifest root |
| Manifest pages | Searchable mutable and immutable source descriptors, ownership/reclamation state |
| Allocation journal/free-space metadata | Durable allocation tracking, reusable extents, orphan recovery |
| Mutable dictionary and posting pages | Searchable incoming document fragments and term lookup |
| Document-owner directory | Root/incarnation, publication state, liveness location, length/position references |
| Immutable term dictionary | Exact term lookup and bounded prefix traversal |
| Group directory | Heap-group ranges, skip information, payload locations |
| Posting containers | Adaptive page and tuple-offset membership |
| TF/length/impact data | Ranking input and conservative pruning metadata |
| Positional streams | Exact positions, accessed lazily |
| Statistics epochs | Versioned corpus-scoring metadata with defined semantics |

Keep the standard PostgreSQL page header valid, including the used/free-space boundaries relevant to generic WAL. Use page-type tags and generation identifiers; never trust a page only because its block number was once valid. PostgreSQL's page layout and generic-WAL rules constrain the wrapper; Pin defines the payload. [PG-Page] [PG-Generic-WAL]

### 8.2 Header and decoder rules

Use fixed-width little-endian integers for Pin payloads, independently of C struct layout. Specify reserved bytes and zero them. Include lengths, counts, encoding tags, and version/feature checks. Validate every addition, multiplication, offset range, and nesting depth before constructing a view.

A decoder must distinguish unsupported format, invalid encoding, and structural corruption. Unknown incompatible feature bits fail closed. No “best effort” skipping of corrupt postings in a query advertised as exact. An administrator can rebuild from the heap, but a query must not silently report partial results.

Store no process addresses, `usize`, Rust discriminant layout, `Vec`, `String`, vtables, or native `repr(C)` structures as the portable payload. `repr(C)` solves a particular ABI problem, not a durable serialization problem. [Rust-Layout]

### 8.3 Heap-page groups

Start with a tunable experiment around **256 heap pages per group**, then benchmark 128/256/512 against representative distributions. For the 256-page candidate:

```text
group_id       = heap_block / 256
page_in_group  = heap_block % 256
page bitmap    = 256 bits = 32 bytes
```

The group identifier is a physical heap coordinate, not an assumption about consecutive logical documents. Holes and sparse regions must be cheap.

Within a page, offset membership uses the valid heap line-pointer domain. PostgreSQL's `MaxHeapTuplesPerPage` depends on page size and tuple-header/item-identifier sizing; do not use a guessed universal constant. For the supported standard configuration, a 512-bit private scratch bitmap is a convenient capacity, but that is **not a requirement to persist 64 bytes for every sparse page**. [PG-HeapTuple-Source]

### 8.4 Container selection

Use a small, benchmarked set of encodings rather than a different clever codec for every case:

| Distribution | Candidate representation | Main tradeoff |
|---|---|---|
| One or very few roots for a term | Inline coordinate(s) | Avoid directory/container overhead |
| A few offsets in one page | Singleton or short sorted offset array | Very cheap sparse intersection |
| Moderate offsets | Delta/bit-packed list | Decode cost versus bytes |
| Dense offsets | Bitmap | Cheap Boolean operations and popcount |
| Long regular spans | Run encoding | Only valuable when real data has runs |
| Sparse page membership within a group | Sorted page indices | Avoid a fixed bitmap cost for rare terms |
| Dense group page membership | Fixed page bitmap | Fast page-level pruning |

Choose using total encoded bytes **including tags, headers, offsets, and alignment**, plus measured decode cost. Avoid a single giant bitmap indexed by the full physical address space. Keep the scalar implementation authoritative and test every pairwise mixed-container operation.

### 8.5 Layout for lazy work

The lookup sequence should be dictionary → group metadata → page presence → tuple offsets → TF/length → positions. A conjunction that fails at the page layer should never touch positional payloads. A count query that does not need ranking should not decode TF/length merely because they share a convenient object in Rust.

Balance this against locality: tiny per-document records can pack length and common TF inline, while large positional streams remain separate. Benchmark columnar streams versus small interleaved records. The objective is bytes actually touched per operation, not purity of a columnar or row-oriented ideology.

Eliminating an internal document-ID-to-TID map does not eliminate all metadata lookups. Pin still needs safe publication/liveness ownership and scoring data. Account for that cost honestly in size and performance reports.

### 8.6 Term dictionaries

Store exact normalized term bytes. A hash may accelerate lookup, but a hash collision may never imply equality. Avoid exposing attacker-controlled hash-flooding behavior in unbounded shared critical sections.

Initial immutable dictionaries can use sorted prefix-compressed blocks with a sparse top-level directory. A finite-state transducer is an experiment for dictionary memory and prefix traversal, not a mandatory dependency. Mutable dictionaries need predictable insertion under contention and bounded lookup over active sources.

Term IDs are local to their dictionary. During compaction, reconcile term identity by bytes or a validated translation table; never assume equal numeric IDs in different segments represent the same term.

### 8.7 Document lengths and positions

Store exact TF and length in the first correctness format using explicitly bounded integer types. Reject overflow rather than saturating silently. If compact lossy norms are introduced later, define a new scoring format/semantics and prove that bounds remain conservative.

Position streams must preserve repeated occurrences and the analyzer's gaps. Field-aware indexing, when added, must prevent a phrase from crossing a field boundary unless explicitly defined. Long documents need bounded decode checkpoints so a phrase query does not allocate every position in the document.

## 9. Insertion, publication, and searchable mutable state

### 9.1 Mutable data must be genuinely searchable

An append-only ingestion log with no term lookup merely moves latency to readers. Pin needs bounded searchable mutable sources: term-directory lookup, candidate posting access, complete-document publication checks, and controlled fanout.

Begin the correctness prototype with one writer shard if that simplifies the protocol, but design source identifiers and manifests for multiple shards. A later document-based sharding function should spread concurrent writers without serializing every common term behind one global lock. Hashing only the current heap page can concentrate append-heavy writes; compare routing strategies rather than assuming that page-local routing always helps.

Mutable sources should seal at bounded byte/document thresholds. A query must not scan an ever-growing number of tiny sources because background merging is behind. Admission control and bounded foreground maintenance are required before sustained-throughput claims.

### 9.2 Document publication state machine

Proposed states:

```text
ALLOCATED -> FRAGMENTS_WRITTEN -> COMPLETE/PUBLISHED
                    |
                    +-> ABANDONED/ORPHAN -> RECLAIMABLE
```

This is index publication, not transaction commit. A published index document may belong to an uncommitted or subsequently aborted heap tuple; PostgreSQL visibility still decides whether a scan returns it.

The insertion sequence should be:

1. Validate and analyze the value outside shared critical sections, within its memory budget.
2. Reserve durable storage and an incarnation-qualified document-owner record.
3. Write term, TF, length, and position fragments through the WAL/storage protocol.
4. Establish every searchable dictionary/posting reference required for that document.
5. Publish one small complete-document state change only after the payload is recoverably valid.
6. Return successful insertion only after a subsequent eligible scan can discover the complete document.

This is a proposed protocol. G2 must prove crash behavior and ordering at each step. Large documents can exceed one WAL record's page-registration capacity, so correctness cannot depend on modifying every page in one atomic record.

### 9.3 Partial publication must not leak into query meaning

Readers may encounter fragments from incomplete documents but must ignore them consistently across all terms. A Boolean AND must not treat a half-published fragment as a real match; a NOT query must not include an incomplete document in its universe. A page-level count must not include an unpublished owner record.

Publication checks must use a defined memory-ordering and buffer-lock protocol. A Rust atomic in a private cache is not evidence that another backend's dictionary, postings, and completion flag became visible together.

### 9.4 Statement lifecycle and own writes

`aminsertcleanup` can reuse or release insertion state held through the statement. It must not be the sole point where accepted documents become searchable, and it is not a substitute for WAL. PostgreSQL exposes this callback as part of the AM lifecycle; Pin must fit that lifecycle rather than invent a commit-only ingestion barrier. [PG-AM-Functions] [PG-AMAPI-Source]

Test INSERT followed by SELECT in the same transaction, triggers, savepoint rollback, speculative insertion/`ON CONFLICT`, aborted transactions, and COPY. Avoid a dependency on user transaction commit callbacks to construct the essential inverted representation: the heap already provides transaction visibility.

### 9.5 Lock-duration target

Analysis, sorting, compression, and large memory allocation occur outside content locks. Shared locks protect bounded metadata operations. Avoid WAL construction that can unexpectedly allocate or throw while a critical section assumes nonfailure.

Do not hold a global manifest lock while waiting for I/O or a cleanup lock. Establish a documented lock order, then test it under cancellation and worker termination. Writer throughput depends as much on predictable lock duration as on the posting codec.

## 10. WAL, recovery, and persistent state machines

### 10.1 Generic WAL first

Use `GenericXLogStart`, `GenericXLogRegisterBuffer`, and `GenericXLogFinish` through a reviewed storage wrapper. Modify the returned page image, not an unrelated pointer to the original buffer. Follow the required lock and page-layout rules; the wrapper must make it difficult to mix generic-WAL updates with ad hoc dirty-buffer manipulation. [PG-Generic-WAL]

The maximum registered pages is a compiled PostgreSQL constant. In the inspected PG18 header, `MAX_GENERIC_XLOG_PAGES` is defined through `XLR_NORMAL_MAX_BLOCK_ID`. **Do not hard-code the historical assumption that every generic WAL record is limited to four pages.** Still design multi-record publication because no finite per-record capacity covers arbitrary documents or merges. [PG-Generic-Header]

Generic WAL is the baseline because it reduces the extension-specific recovery surface. Measure its full-page-image and delta costs under checkpoint pressure; it is not automatically the lowest-WAL solution.

### 10.2 Persistent reachability is the atomic boundary

Large payloads are prepared while unreachable from the active manifest or complete-document record. A bounded root/publication update makes them searchable. Each preparation step must be recoverable or discoverably orphaned after crash.

An allocator must never declare an extent reusable merely because it is absent from the newest active manifest: it may belong to an in-progress operation or a retired generation still used by readers. The allocation journal and ownership metadata must permit recovery to distinguish these cases.

Use checked generation increments and explicit overflow policy. Reusing a block address without a generation check is an ABA risk. Generation counters are not timestamps and must not be interpreted as transaction visibility.

### 10.3 Required recovery cases

| Crash point | Required recovered state |
|---|---|
| Before fragment WAL completion | No complete searchable document; partial storage discoverable as orphan |
| After fragments, before publication | Unreachable data may be reclaimed safely; no half-document match |
| After publication record | Complete document discoverable; heap MVCC decides visibility |
| During source freeze | Exactly one recoverable ownership state; no missing writer coverage |
| During merge output preparation | Old sources remain valid; incomplete output is not selected |
| After manifest replacement | New source set is complete; old sources retained until safe |
| During reclamation/free-list update | No extent simultaneously free and reachable |
| During VACUUM liveness update | Replay preserves required removal before later heap-slot reuse |

A backend abort and a server crash are different tests. Many index changes are not physically undone on transaction abort; they remain safe because the heap version is invisible and later removable. Pin's publication and orphan handling must reflect that distinction.

### 10.4 Custom WAL requires an explicit benefit and recovery decision

A custom resource manager may reduce WAL or improve recovery representation, but adds redo, description/identification, masking/consistency, registration, packaging, and operational obligations. PostgreSQL requires such resource managers to be registered at startup and available when their WAL must be replayed. A production ID must not be an uncoordinated experimental choice. [PG-Custom-Rmgr]

Adopt custom WAL only after profiling demonstrates a material benefit under equal durability, or a separately reviewed correctness/operational requirement—such as a chosen standby conflict protocol—justifies its extra recovery surface. It is not a default prerequisite for the initial primary-only query path. The release must include restart, standby, PITR, backup restoration, and extension-version compatibility tests. Never implement a private second WAL with independent commit order.

### 10.5 Logged, unlogged, temporary, and build paths

Initial correctness work may reject unlogged and temporary indexes explicitly. Supporting unlogged relations requires a valid empty/init-fork path and restart-reset tests. Supporting temporary relations requires session-local buffer/lifecycle handling and exclusion from shared background work. `ambuildempty` cannot simply be ignored because the main benchmark uses logged tables. [PG-AM-Functions] [PG-Files]

Bulk-build optimizations must use PostgreSQL's sanctioned build/storage paths for the target release. Do not disable `fsync`, omit required WAL, or treat `COPY` as permission to weaken durability. Benchmark “build after bulk load” separately from maintaining an index during ingestion. [PG-Populate] [PG-WAL-Internals]

## 11. VACUUM, liveness, generations, and compaction

### 11.1 Three independent concepts

**Publication:** all index data for an incarnation is complete and searchable.  
**Liveness:** the incarnation has not been removed by index VACUUM.  
**Visibility:** PostgreSQL says the heap version is visible to this snapshot.

Do not combine them into one `is_live` boolean. An uncommitted document can be published and live but invisible. A long-running snapshot can need a version that is obsolete for new transactions. A removed incarnation must never become live again when its bare TID is reused.

### 11.2 VACUUM's authority

`ambulkdelete` receives PostgreSQL's removal callback. Use it to decide which root references can be removed. `amvacuumcleanup` may be called without a prior bulk-delete pass and must handle analysis-only and partially completed maintenance conditions correctly. [PG-AM-Functions]

PostgreSQL's heap VACUUM has multiple passes and can perform multiple index-cleanup rounds; it may also skip index cleanup in some modes or failsafe situations. Pin may not assume “one complete callback per vacuum command.” [PG-Vacuum-Source] [PG-Vacuum]

### 11.3 Liveness directory proposal

Each source contains a document-owner directory, with publication and liveness qualified by document incarnation. Immutable postings can remain physically unchanged while the liveness directory is updated through WAL.

A per-page offset bitmap may be an efficient liveness encoding for a source, but its bit must refer to that source's incarnation—not to whichever document occupies the coordinate today. The directory also supplies the authoritative pin/cleanup anchor for tuple-at-a-time and later heap-free paths.

VACUUM should traverse document ownership once, not repeatedly invoke the expensive root-removal decision separately for every term occurrence. Common terms should not multiply the basic dead-document bookkeeping cost. Posting-space reclamation can follow through compaction.

### 11.4 Retired generations are still real

A query may have captured an old manifest while compaction publishes a new one. Keeping old physical pages allocated is necessary but not sufficient: **their liveness view must also remain safe if heap slots become reusable**.

Therefore VACUUM must reach every current or retired-but-reader-reachable source containing a removable incarnation, or enforce an alternative proven barrier that prevents reuse until those readers are safe. “The old query has a manifest reference” is not an MVCC proof.

The initial design should prefer explicit reachable-generation registration plus shared liveness/removal coordination. Registrations belong to resource lifetimes and must be cleaned up on ERROR and backend death. Backend identity requires a generation-qualified process identity, not a PID alone.

### 11.5 Copying compaction first

The first merger should read a frozen source set, copy/re-encode postings and document metadata into a new source, validate coverage, and atomically publish the replacement manifest. This gives a simpler reference implementation and a baseline for write amplification.

The merger must reconcile VACUUM removals that occur while it is constructing output. Use a deletion journal, publication-time revalidation, or another explicit protocol. Merely copying liveness at merge start can resurrect a document that VACUUM removed before publication.

A manifest replacement must select either the old logical coverage or the new coverage, not both accidentally. If an implementation temporarily exposes overlapping sources, deduplication must be incarnation-aware and must not mix different documents at the same TID. Prefer avoiding overlap in the first implementation.

### 11.6 Freeze and writer coordination

A source is immutable only after all in-flight writers assigned to it have either completed their publication or left a safely abandoned record. Move new writers to a new source through a bounded state transition; do not label a source immutable while another backend still mutates its posting pages.

Freeze must not wait for SQL transaction commit. Published index entries for uncommitted heap tuples can be carried into the immutable tier, with visibility still decided by PostgreSQL. Otherwise a single long transaction could indefinitely pin the entire writer tier.

### 11.7 Copy-avoiding merges: later gate

TID-based postings may make payload reuse possible, but identity stability alone is insufficient. A source extent can be transferred or shared only when its encoding and dictionary dependencies remain valid, liveness ownership is correct, references cannot be reclaimed prematurely, and crash replay preserves the ownership transition.

Required proof covers shared subpages, TF/position references, dictionary remapping, reference accounting, partial output, cancellation, readers of retired manifests, and free-list consistency. Benchmark total WAL, bytes rewritten, fragmentation, and later read amplification—not just the merge's CPU time.

This optimization must remain independently disableable. No release should depend on an unproved zero-copy merge for data correctness.

### 11.8 Maintenance under pressure

Long queries and old snapshots can retain sources or dead heap versions. Expose that debt; do not solve it by deleting still-needed data. Bound new-source fanout, throttle optional merges, and use explicit writer backpressure when necessary.

Avoid starving VACUUM with long-lived owner pins. Keep fast-path batches short and bounded, release pins promptly, and never wait for a client to consume network output while holding an unnecessary cleanup-blocking pin. Cleanup-lock acquisition and multi-page batching need a specific deadlock audit.

## 12. Boolean, phrase, and streaming execution

### 12.1 Iterator contract

The pure engine should expose bounded, forward-only iterators that can advance to a group/page/offset target without reconstructing all previous results. The execution order is logical query normalization, term lookup, group pruning, page pruning, offset operations, publication/liveness validation, optional positions, and visibility/SQL qualification.

Make each iterator's ownership explicit. It may retain immutable segment references and small metadata, but it may not retain a raw shared-page slice after its validity window. The storage adapter supplies validated views or copies; the engine does not call PostgreSQL while halfway through a pure container operation.

### 12.2 Conjunction and disjunction

For AND, use estimated selectivity and available skip metadata to advance the cheapest promising stream first. Intersect page-presence sets before offset sets; skip absent groups immediately. Avoid “decode everything and then intersect a `HashSet`.”

For OR, union page groups and offset sets while deduplicating the same eligible root incarnation. Do not add term cardinalities to compute union size. For mixed expressions, preserve the AST's Boolean meaning; distributive rewrites that explode clause count require a budget.

Selectivity metadata is a planning hint, not a correctness condition. An underestimated term frequency may make a query slower; it must not suppress matches. Prefix expansion limits and cancellation must be checked independently of selectivity estimates.

### 12.3 Negation and the document universe

Store membership for complete, indexed non-null documents even when they contain no terms. A NOT iterator subtracts matching roots from that bounded universe. Never complement unused bitmap offsets or include roots from unrelated segments by accident.

Apply incarnation-specific liveness before combining sources. Physical duplicates caused by an explicit migration protocol require special handling; they are not ordinary OR duplicates. A mutable publication record and an immutable copied record must not count as two documents.

### 12.4 Phrase matching

Intersect candidate terms first, then load positional streams for surviving documents. Check exact relative positions, repeated terms, and analyzer-defined gaps. A phrase like `"a a"` needs two correctly positioned occurrences; two membership bits for the same term are not enough.

Maintain a small position window or streaming merge rather than materializing every position list for large documents. Positional checkpoints should permit bounded seeks. Empty phrase, escaped quotes, stopword gaps, and future field boundaries need explicit fixtures.

### 12.5 PostgreSQL bitmap baseline

The first integration should support `amgetbitmap` and let PostgreSQL perform heap visibility and executor qualification. A `TIDBitmap` can become lossy under memory pressure, so SQL rechecks must remain correct. The AM's exactness declaration must describe the actual search operation, not the desired final design. [PG-AM-Scan]

A conservative first implementation may request rechecks even when a later exact posting path will avoid them. The sequential predicate is therefore a production component, not a test-only convenience. The pure engine should still stream into the bitmap rather than first allocating an independent full result set.

### 12.6 Cancellation and bounded work

Check PostgreSQL interrupts at batch boundaries and during long term/position traversals through the guarded adapter. Make cancellation latency part of testing. A query with a huge `k`, an enormous prefix vocabulary, or a pathological phrase must not enter a minutes-long unchecked CPU loop.

No budget may be enforced by silently returning fewer results. Use a correct spill path, a less specialized PostgreSQL plan, or a clear resource-limit error. A timeout is an error, not an approximate result.

## 13. Visibility-aware execution and exact counts

### 13.1 The hierarchy of fast paths

Implement three distinguishable paths:

**V0: heap-verified.** Produce candidates and ask PostgreSQL to fetch visible HOT-chain tuples. This is the correctness baseline.

**V1: visibility-assisted row retrieval.** Skip visibility work only under a proven all-visible protocol, but fetch the heap when output columns or residual conditions require it.

**V2: heap-free exact counting.** Count exact eligible live membership for certified all-visible pages without materializing rows; use V0 for uncertified pages.

A count node is not an ordinary index-only scan and does not justify claiming that Pin can reconstruct the original indexed text.

### 13.2 Why the visibility map cannot be treated as a cached Boolean array

The PostgreSQL index-only executor documents ordering relationships between index publication, clearing visibility-map state, and observing snapshots. `visibilitymap_get_status` explicitly leaves important concurrency responsibilities to its caller. The safe protocol is more than reading an all-visible bit. [PG-IndexOnly-Source] [PG-VM-Source]

**Pin requirements:** establish the relevant index-publication ordering before trusting VM status; use an active supported MVCC snapshot; distinguish all-visible from all-frozen; treat missing VM state conservatively; and never reuse a privately cached all-visible result across arbitrary later changes. Do not take a SIMD slice over shared VM bytes and assume that Rust atomics or a buffer pin repairs the protocol.

### 13.3 Exact-count equation

For a query `q` and its snapshot `s`, Pin's logical requirement is:

```text
count(q, s) = number of distinct heap rows that:
              satisfy the exact query semantics,
              are visible under s,
              satisfy every remaining SQL/security condition.
```

The proposed physical optimization partitions work:

```text
count = sum(popcount(exact, live, deduplicated offsets on certified pages))
      + sum(heap-verified matches on every uncertified page/source)
```

“Certified” includes query exactness, source/incarnation safety, visibility ordering, and executor eligibility. It does not mean merely “the VM bit happened to be one.” Stored document frequencies and live-document totals do not satisfy this equation.

### 13.4 Proposed bounded ownership protocol

A candidate protocol for sealed sources is:

1. Capture and register a consistent manifest generation.
2. For a bounded heap-page batch, acquire the authoritative document-owner pins and validate/copy publication and liveness under the required content locks.
3. Retain the cleanup-blocking ownership protection through VM acquisition and count decision.
4. Obtain VM status through PostgreSQL's supported function and apply the source-specific publication-ordering proof.
5. Count exact offset membership only on certified pages. Heap-check the rest using root-TID-aware fetch.
6. Release pins and batch resources promptly, before yielding unnecessary work to the client.

VACUUM must honor this ownership protection in every reader-reachable generation before permitting heap-slot reuse. The implementation must prove lock ordering, cancellation cleanup, and absence of self-deadlock when several source owners cover the same heap page.

**Status:** this is the proposed proof obligation, not a proven implementation. Initially enable V2 only for cases that have passed the full protocol model and isolation/crash suite. Mutable-source matches remain heap-checked until their publication interlock is separately proven. Retaining a manifest reference alone never grants V2 eligibility.

### 13.5 Races that must be modelled explicitly

| Interleaving | Property that must survive |
|---|---|
| Reader sees candidate; writer clears VM and publishes another document | No stale all-visible observation can certify an ineligible candidate |
| Reader copies liveness; VACUUM removes root; heap slot reused | Old posting cannot count the replacement document |
| VACUUM removes from active source while reader holds retired manifest | Retired source remains safely updated or reuse is blocked |
| Merge copies live bits; VACUUM deletes; merge publishes | Deletion cannot be lost |
| Reader is cancelled while holding owner pins | VACUUM eventually proceeds; no leaked registration |
| VM page missing or changed during traversal | Fall back, never infer invisibility or exact count |
| Phrase candidates are approximate | Recheck positions/heap predicate before counting |
| Two sources temporarily overlap | Count logical rows once without mixing incarnations |

Use deterministic injection points to stop execution between these steps. Random stress alone is unlikely to exercise all critical windows.

### 13.6 Upper-plan eligibility

The initial direct count path should accept only a narrow `COUNT(*)` shape over one eligible heap relation with the exact Pin predicate and no remaining conditions requiring tuple evaluation. Exclude RLS, security-barrier qualifications, joins, grouping, DISTINCT, aggregate FILTER, row marks, volatile expressions, and unsupported snapshot contexts.

A base-relation scan cannot simply return one row containing the count while the original aggregate still expects document rows. Offer a legal upper-plan replacement using the upper-path hook, or leave the core aggregate in place over a custom row-producing scan. PostgreSQL exposes upper-path integration separately from base relation paths. [PG-Planner-Source] [PG-Custom-Path]

### 13.7 Operational consequence

VM-assisted performance depends on the heap's actual modification and vacuum state. Pin must report the number of VM-certified pages, heap-checked pages, liveness rejects, and fallback reasons. Read-mostly and update-heavy tables may have very different speedups. Benchmark both instead of presenting a vacuumed static corpus as the only production case.

## 14. BM25, pruning, and exact visible top-k

### 14.1 Freeze the scoring definition

Use a versioned BM25 definition. One suitable Pin v1 proposal is:

```text
idf(t) = ln(1 + (N - df(t) + 0.5) / (df(t) + 0.5))

score(d, q) = sum over positive scoring terms t:
    boost(t) * idf(t) *
    [tf(t,d) * (k1 + 1)] /
    [tf(t,d) + k1 * (1 - b + b * length(d) / avg_length)]
```

Proposed defaults are `k1 = 1.2` and `b = 0.75`. Require finite `k1 > 0`, finite `0 <= b <= 1`, and finite nonnegative boosts. A term with `tf = 0` contributes zero without evaluating a potentially zero denominator. Define how repeated query terms contribute and which Boolean clauses score. Pure negative queries can have score zero. Phrase eligibility and phrase-specific bonuses are separate; omit bonuses initially.

Lucene's official BM25 documentation/source is a useful primary reference for the model and its implementation choices. **Do not promise identical numeric scores:** Pin's displayed formula, exact lengths, floating-point policy, and corpus statistics are its own explicit contract; different implementations can use different normalization constants or compressed norms. [Lucene-BM25] [Lucene-BM25-Source]

### 14.2 Statistics policy: exact membership, approximate corpus statistics

Low-cost search statistics are not exact snapshot-visible SQL counts. The proposed first policy is an immutable **physical-corpus statistics epoch**, derived from a consistent set of completed index-document summaries. It may include versions not visible to a particular snapshot and may lag recent insertions or VACUUM. That approximation affects relevance calibration, not whether a row matches or is visible.

Each epoch must keep `N`, term document frequencies, and length totals mutually coherent; validate `0 <= df(t) <= N`. A term absent from an older epoch has a defined `df = 0`. Use `avg_length = total_length / N` when both `N` and total length are positive; define `avg_length = 1` when `N = 0` or total length is zero, including a corpus containing only empty documents. Validate `N - df` in integer arithmetic before floating-point conversion, and reject non-finite computed scores. Do not patch inconsistent statistics by silently clamping random fields and continuing as though nothing happened.

Capture one epoch per corpus for a statement and share it across that statement's score expressions and accelerated scans. Use a statement-lifetime registry, with scan-specific bindings; do not retain it across prepared-statement executions. Nested executors and repeated scans require explicit lifetime tests.

Expose the epoch ID and freshness. An optional expensive snapshot-exact statistics mode can be future work; do not imply it is free. Restrict statistics/score access where corpus-wide information could leak data hidden by row-level policies.

### 14.3 Scoring kernel

Start with `f64`, deterministic term accumulation order, explicit finite-value checks, and exact TF/length inputs. Keep floating-point behavior identical between reference and accelerated paths. No fast-math compiler options or unreviewed reassociation that changes threshold comparisons.

Precompute query-constant weights and normalization factors once. Decode TF/length only for blocks that survive cheaper Boolean and score-bound tests. A small bounded top-k heap should store root/incarnation, score, and only the minimal projection identity needed later.

### 14.4 Block-max pruning

Use block-level maxima or conservative TF/length bounds to avoid visiting noncompetitive documents. Lucene's WAND implementation is a primary-source example of dynamic pruning and conservative score-bound rounding. It is an implementation reference, not code to copy without license compliance. [Lucene-WAND]

Pin's first pruning proof should be simple: nonnegative additive scores; a fixed query statistics epoch; an upper bound for every candidate block; and a threshold generated only by eligible rows. More sophisticated WAND/MaxScore variants come after equivalence with exhaustive scoring.

Bounds can be generated from maximum TF and minimum length for a block, then evaluated with the current query's fixed weights. Stored precomputed score maxima must be tagged with every dependency or conservatively recomputed. Changing IDF, average length, analyzer, or scoring parameters can invalidate an old numeric maximum.

Use outward/conservative rounding for bounds, not a guessed epsilon. Rounding a bound downward can silently omit a winning row. Compare boundary cases against an exhaustive high-precision or carefully specified reference; include almost-equal scores and very large TF/length.

### 14.5 Visibility and filters precede threshold admission

Consider an invisible document with score 100 and a visible document with score 90. In a top-1 query, admitting the invisible document to the competitive heap could prune the visible result. The same bug occurs when a high-scoring document fails `published = true` or an authorization condition.

**Rule:** only a `VisibleMatch` that satisfies every condition required by the local top-k may become an `EligibleMatch` and raise the threshold. A candidate may be scored before heap visibility for scheduling purposes, but cannot constrain the final result until eligible.

A fixed oversampling multiplier is not an exact solution. Neither “fetch 10k candidates for LIMIT k” nor “continue until k physical matches” guarantees k eligible results. Continue until the remaining safe upper bounds prove no eligible unseen row can win.

### 14.6 SQL ordering details

The optimized node must implement the requested pathkeys and tie behavior. For score-only ordering, an internal physical tie-breaker may make output reproducible within one physical generation, but it is not a durable pagination key. An additional `ORDER BY id` cannot be ignored; either handle it exactly or decline the hard top-k optimization.

Account for OFFSET as part of the required competitive set, with overflow/budget checks. `WITH TIES`, window functions, aggregates, joins, and row locking need dedicated support or fallback. A local ranked scan below a join may return an ordered complete stream, but cannot assume that a LIMIT above the join permits truncating the base relation.

### 14.7 Late materialization without hidden unsafe lifetimes

Batch candidates by heap page where that reduces visibility fetches, then restore score order for output. Retain only necessary row data or materialize into PostgreSQL-managed slots/tuplestores as required. Never keep a pointer into a heap buffer after releasing its validity protection.

Do not advertise elimination of all heap I/O: returning `body`, evaluating residual filters, or locking rows may still require heap access even when visibility is certified. Measure visibility savings and projection I/O separately.

## 15. Index AM callback and capability contract

The official AM API, callback definitions, scan/locking chapters, and target-release `amapi.h` are the implementation authority. The following tables specify **Pin's proposed choices**, not replacements for those documents. Bind through the selected release's generated definitions rather than copying an old struct layout. [PG-AM-API] [PG-AM-Functions] [PG-AMAPI-Source]

### 15.1 Capability settings

| Field | Initial Pin choice | Reason / later gate |
|---|---|---|
| `amstrategies` | 1 for the initial match operator | Boolean structure lives inside the typed query |
| `amsupport` | 0 for the initial closed built-in opclass | Add support procedures only with a real public contract |
| `amoptsprocnum` | 0 | No opclass-options procedure initially |
| `amcanorder` | false | Physical TID traversal is not text SQL ordering |
| `amcanorderbyop` | false | Custom score ordering is a separate path initially |
| `amcanhash` | false | This is not the hash AM contract |
| `amconsistentequality` | false | Match is not equality |
| `amconsistentordering` | false | No generic ordered-comparison promise |
| `amcanbackward` | false | Forward scan only |
| `amcanunique` | false | No unique/exclusion-index claim |
| `amcanmulticol` | false | One key column initially |
| `amoptionalkey` | false | A match key is required; nulls may be omitted |
| `amsearcharray` | false | SQL scalar-array handling is not the internal query OR |
| `amsearchnulls` | false | No null-search strategy initially |
| `amstorage` | false | No alternative SQL opclass storage type initially |
| `amclusterable` | false | No clustering-order contract |
| `ampredlocks` | false | Use core coarse predicate locking on ordinary paths |
| `amcanparallel` | false | Enable only with full parallel-scan lifecycle |
| `amcanbuildparallel` | false | Separate parallel-build gate |
| `amcaninclude` | false | No covering original-value storage initially |
| `amusemaintenanceworkmem` | true when build honors it | Shared budget, not per-worker multiplication |
| `amsummarizing` | false | Individual root identities remain significant |
| `amparallelvacuumoptions` | 0 | No unproved parallel VACUUM support |
| `amkeytype` | `InvalidOid` for normal key typing | Private byte layout is not this SQL-level field |

Initialize all fields explicitly or through a checked constructor, with the proper node tag and PostgreSQL-owned allocation. Compile-time ABI tests and a runtime capability inspection test should detect accidental drift. The first opclass validator accepts only the supported signature/profile; it must not bless arbitrary user-supplied operators that happen to share a strategy number.

### 15.2 Callback responsibilities

| Callback | Pin component and expected behavior |
|---|---|
| Handler | Allocate and populate a correct `IndexAmRoutine`; no persistent Rust-owned pointer masquerading as a PostgreSQL node |
| `ambuild` | Use core table index-build scan; collect bounded runs; preserve HOT roots and partial/expression semantics; return accurate document-level statistics |
| `ambuildempty` | Implement supported empty/init-fork behavior, or reject unsupported persistence before creating an unusable index |
| `aminsert` | Analyze once; publish complete searchable data; preserve heap-TID identity and error cleanup |
| `aminsertcleanup` | Release statement insertion resources; no delayed visibility dependency |
| `ambulkdelete` | Remove callback-approved incarnations across all reachable sources; accumulate multi-round statistics |
| `amvacuumcleanup` | Handle zero/multiple deletion passes and analysis-only calls; bounded safe cleanup |
| `amcanreturn` | NULL/false initially; inverted tokens do not reconstruct original text |
| `amcostestimate` | Predict index work from measured metadata; avoid counting core heap cost twice |
| `amgettreeheight` | NULL unless a meaningful cheap value is useful; no fabricated tree height |
| `amoptions` | Validate documented physical tuning options; preserve semantic-profile immutability |
| `amproperty` | Optional accurate properties/diagnostics, never unsupported capability advertising |
| `ambuildphasename` | Expose actual build phases for progress monitoring |
| `amvalidate` | Validate the known opclass/operator signatures, strategy numbers, and supported profile |
| `amadjustmembers` | Optional initially; required before supporting arbitrary extensible opfamilies |
| `ambeginscan` | Use `RelationGetIndexScan` and allocate private scan-lifetime state without assuming keys are initialized |
| `amrescan` | Reset parameters, iterators, resources, thresholds, and per-scan bindings correctly |
| `amgetbitmap` | Stream candidates into PostgreSQL's bitmap; declare rechecks conservatively |
| `amgettuple` | Added only after owner-pin/VACUUM protocol; preserve root-TID and requested direction contract |
| `amendscan` | Release registrations, pins, slots, and private allocations; do not free the core-owned scan descriptor |
| `ammarkpos` / `amrestrpos` | NULL until a full restart/position contract is implemented |
| `amestimateparallelscan` / `aminitparallelscan` / `amparallelrescan` | NULL until DSM ownership and rescanning are tested |
| `amtranslatestrategy` / `amtranslatecmptype` | NULL initially; match predicates are not ordinary ordered comparisons |

`amoptions` also has PostgreSQL-specific validation-versus-loading behavior; follow that distinction rather than raising incompatible errors while reading existing reloptions. For catalog/disk semantic incompatibility, use the appropriate explicit version checks instead. [PG-AM-Functions]

### 15.3 Recheck and scan state

For `amgettuple`, set `xs_recheck` according to actual exactness. For `amgetbitmap`, supply the corresponding recheck requirement through PostgreSQL's bitmap API, such as the `recheck` argument of `tbm_add_tuples`; setting a scan-descriptor flag alone is not the bitmap contract. Lossy pages require rechecks. Do not hide an approximate phrase/prefix path behind an exactness flag. [PG-TIDBitmap-Source]

Add results to the caller's existing bitmap rather than replacing its contents. The `amgetbitmap` return value is AM scan accounting, not a snapshot-exact SQL count. Use `RelationGetIndexScan` for the core scan descriptor, preserve previous keys when a rescan supplies NULL keys, and let core own the descriptor's final freeing. Pin releases its own scan resources. [PG-AM-Functions]

Scans must reset correctly when parameters change, including prepared statements and nested-loop rescans. Cached term handles, scoring epochs, and generation registrations have different lifetimes. Record which are statement-scoped and which must be rebuilt per rescan. [PG-AM-Scan]

### 15.4 Cost model

A proposed ordinary-index cost model estimates dictionary probes, source fanout, group/page metadata reads, compressed bytes decoded, positional rechecks, and TID production. Return credible selectivity, correlation, and index-page estimates. PostgreSQL adds heap-related costs for an ordinary index path; do not double count them in `amcostestimate`. [PG-AM-Cost]

A CustomScan cost can model its full work, including heap fallback, residual filters, scoring, materialization, and sorting avoided. Estimate startup versus total cost separately so LIMIT-sensitive plans remain sensible. Never set cost to zero to force the planner to choose Pin.

### 15.5 Concurrent builds are not an advertising flag

There is no single `amcanconcurrentbuild` boolean that proves correctness. The AM's build, insert, scan, validation interaction, and HOT handling must cooperate with core `CREATE INDEX CONCURRENTLY` behavior. Failed or invalid indexes, waiting phases, old snapshots, and concurrent writers need dedicated testing before this is supported. [PG-CreateIndex] [PG-HOT-Source]

## 16. Planner and CustomScan integration

### 16.1 Add alternatives; do not hijack SQL

Use the supported hooks to offer paths, chain any existing hooks, and let the planner compare alternatives. Avoid text matching against SQL strings, unconditional query rewrites, or mutating global planner state. Match real operator/function OIDs, expression trees, index predicates, collation/profile identity, and parameterization.

A base-relation hook can offer a row-producing search path. An upper-path hook can offer eligible ranking/aggregate alternatives. The CustomPath, CustomScan plan, and CustomScanState have distinct responsibilities. [PG-Custom-Path] [PG-Custom-Plan] [PG-Planner-Source]

### 16.2 Plan data versus execution data

Store only copyable/serializable PostgreSQL Nodes and stable identifiers in plan-private fields. Do not put a Rust pointer, buffer handle, open relation, or backend-specific query object into a reusable plan.

At execution start, open validated relations, capture the snapshot and scoring epoch, compile runtime parameters, and create resource-owned state. At rescan, refresh what the parameter and snapshot contracts require. At end/shutdown/ERROR, release everything. Custom execution callbacks include more than a “next row” method; implement only advertised capabilities and test the entire lifecycle. [PG-Custom-Execution]

### 16.3 Score binding

Bind score expressions to the exact scan alias, indexed expression, query, and corpus. Self-joins, two searches of one table, subqueries, and prepared statements must not share the wrong score cache. A statement-level statistics registry may be shared, but per-row/per-scan score state may not.

Do not rely on an order in which PostgreSQL happens to evaluate target-list expressions. The ordinary `pin.score` function must remain meaningful when no accelerated scan ran, and the planner must not replace a semantically different expression just because it resembles the supported form.

### 16.4 Security and serializability

RLS policies and security barriers constrain when predicates may run. Initially decline custom paths when those conditions would require a new proof; ordinary PostgreSQL execution remains available. Do not mark search functions leakproof merely to make a path eligible. Corpus-statistics inspection and ranking can themselves disclose information and need a permission policy. [PG-RLS]

When `ampredlocks` is false, ordinary index scans use PostgreSQL's coarse index predicate locking. A custom node that reads storage directly may bypass the wrapper that establishes that behavior. Initially disable custom paths in SERIALIZABLE transactions; later audit equivalent read/write conflict tracking, including empty-result queries. [PG-AM-Lock] [PG-SSI-Source]

### 16.5 Modification and row locks

`FOR UPDATE`, `FOR SHARE`, UPDATE/DELETE targeting, EvalPlanQual, and volatile qualifications can require tuple behavior beyond a read-only search node. Fall back until explicitly implemented and tested. A custom row-producing node must provide correct tuple identity and slot semantics; returning a projected record is not automatically interchangeable with a base-relation tuple.

### 16.6 Explainability

`EXPLAIN` should identify the actual path and show analyzer/format/scoring versions, source fanout, estimated and actual candidate counts, groups skipped, positions decoded, heap fetches, VM-certified pages, top-k bound skips, memory peaks, and fallback reasons. Keep instrumentation cheap when disabled.

A diagnostic “fast paths off” mode should permit differential comparison in the same database. It must change execution strategy, not query semantics or visibility.

## 17. Rust, FFI, memory, and unsafe standards

### 17.1 Rust is the implementation language; PostgreSQL is the host runtime

Use stable Rust and the standard library freely where they fit the ownership model. There is no performance reason to impose `no_std` on a normal Linux PostgreSQL extension. The important distinction is between pure owned computation and host-owned resources, not between `std` and “low-level” code.

Make `pin-core`, test/reference code, and orchestration code forbid unsafe code. Put CPU intrinsics in `pin-kernels`, validated optional unchecked decoding in a tightly controlled codec module, and PostgreSQL FFI in `pin-pg`. A small C shim may cover macros/inlines and critical host-runtime boundaries. Do not duplicate PostgreSQL C bitfields or inline protocols in Rust merely to keep the repository visually all-Rust.

### 17.2 Allocation ownership table

| Object | Owner and lifetime | Required behavior |
|---|---|---|
| SQL return Datum / PostgreSQL Node | Appropriate PostgreSQL memory context | Correct alignment/representation; survives expected caller use |
| AM scan or CustomScan state | Scan/query context plus explicit resource cleanup | No state leaked into later prepared executions |
| Pure query AST / scratch arrays | Rust-owned within a guarded lifetime, or an audited query arena | Bounded; destructor/abort policy specified |
| Shared index page | PostgreSQL buffer manager | Pin and content validity tracked separately |
| WAL page image | Generic-WAL operation | Borrow cannot escape the operation |
| Segment registration | PostgreSQL resource/lifecycle integration | Removed on normal end, ERROR, abort, and backend death |
| Shared worker control | PostgreSQL shared memory / DSM | Offset-based structures and interprocess synchronization |
| Spill file | PostgreSQL-managed temporary resource | Cleanup on cancellation and transaction/process exit |

Do not use `Vec::from_raw_parts` or `Box::from_raw` to take ownership of a `palloc` allocation. Do not `pfree` memory owned by the Rust allocator. Do not globally replace Rust's allocator with an unreviewed PostgreSQL allocator shim. Rust collection ownership and PostgreSQL memory-context ownership are different systems. [Rust-Vec] [PG-Memory-Source]

### 17.3 Memory contexts do not run arbitrary Rust destructors

A PostgreSQL context reset can free memory without executing Rust `Drop`. A Rust stack unwind can execute `Drop`, while a PostgreSQL error jump needs a compatible boundary strategy. Therefore each wrapper must state both its normal and error cleanup path.

Use resource ownership for buffer pins, open resources, and generation registrations, not only memory contexts. PostgreSQL's ResourceOwner interface supplies explicit remember/forget and release machinery; release callbacks must obey its nonfailure and ordering rules. [PG-ResourceOwner-Source]

Design cleanup as idempotent: normal Rust cleanup marks an entry released, and resource cleanup must not release it twice. Resource registration itself must be arranged so failure between acquisition and registration cannot leak the resource. Pre-enlarge/prepare resource-owner capacity when required before a nonfailure acquisition region.

### 17.4 Errors, panic, and unwinding

Use the selected pgrx release's supported guards for callbacks entering Rust and for PostgreSQL calls that may ERROR. The pgrx guard documentation describes an `extern "C-unwind"` boundary; actual callback signatures must match the generated binding for the selected version. [Pgrx-Guard]

Rust's FFI/unwinding rules must be reviewed for every nontrivial boundary. `catch_unwind` is not a general handler for arbitrary C `longjmp`, foreign exceptions, or aborting failures. Do not let PostgreSQL jump through unprotected Rust frames holding resources, and do not let a Rust panic escape through an ABI that forbids it. [Rust-FFI] [Rust-CatchUnwind]

The initial release profile uses `panic = "unwind"`. Do not select `panic = "abort"` as a casual performance optimization in a server extension. `Drop` implementations must not panic, invoke fallible logging, or call PostgreSQL routines that can recursively ERROR during cleanup.

Keep PostgreSQL critical sections tiny and prevalidated. Prefer a narrowly audited C/pgrx-compatible wrapper when the critical-section/error behavior cannot be expressed safely across the Rust boundary. Never allocate, format messages, run user callbacks, or perform arbitrary compression inside a region whose failure would compromise shared state.

### 17.5 Unsafe acceptance checklist

Every unsafe block requires a nearby `SAFETY:` explanation containing the relevant proof, not “safe because PostgreSQL.” Its review must cover:

| Obligation | What must be established |
|---|---|
| Bounds | Full byte range, overflow checks, one-past-end handling, vector tails |
| Alignment | Actual alignment or a correct unaligned operation |
| Initialization | Every element read or exposed has been initialized |
| Provenance and lifetime | Pointer comes from a valid allocation/resource and cannot outlive it |
| Aliasing | Shared/mutable references obey Rust rules despite C and shared-memory mutation |
| Concurrency | Required PostgreSQL lock/barrier/atomic protocol, including readers and writers |
| ISA | Runtime CPU features and compile-time target requirements match |
| Errors | No forbidden unwind/jump or leaked resource across failure |
| Format | Untrusted disk/query bytes were validated before unchecked interpretation |
| Evidence | Safe reference, targeted tests, sanitizer/Miri applicability, and benchmark justification |

Rust's undefined-behavior reference is the baseline, not a list of optional cautions. A benchmark improvement never makes undefined behavior acceptable. [Rust-UB]

### 17.6 Raw slices and pointer APIs

`slice::from_raw_parts` requires more than a non-null pointer: valid initialized memory of the required extent, correct alignment, a single allocation, and the required alias/lifetime conditions. Do not create a shared Rust slice over memory concurrently mutated by another backend under an incompatible protocol. [Rust-Slice]

`ptr::read_unaligned` removes an alignment requirement for the operation, not bounds or initialization requirements. `copy_nonoverlapping` requires valid nonoverlapping regions and checked sizes. `MaybeUninit` only helps when initialization tracking is correct; exposing uninitialized elements through `set_len` is still wrong. [Rust-ReadUnaligned] [Rust-Copy] [Rust-MaybeUninit]

Prefer checked byte readers using `from_le_bytes` and validated slices in the first codec. Move a proven bounded inner loop to unsafe only when profiling shows the safe implementation is materially worse. Safe code can already compile to very efficient instructions.

### 17.7 Shared memory is not ordinary Rust shared ownership

`Arc`, `Mutex`, and a heap pointer do not form a PostgreSQL interprocess data structure. Use PostgreSQL LWLocks, latches, resource/process identifiers, and DSM conventions. Keep shared structures fixed-layout with offsets or validated handles. Avoid Rust-owned objects whose destructor or allocator assumes the creating process. [PG-LWLock-Source] [PG-DSM-Source]

`UnsafeCell` permits interior mutability under Rust's rules; it does not synchronize accesses. Atomic memory orderings need a written protocol that also covers PostgreSQL's C-side readers/writers. Do not mix unrelated Rust atomics with uncoordinated C accesses to the same bytes. [Rust-UnsafeCell] [Rust-Ordering]

### 17.8 Allocation and error budgets

Reserve bounded vectors fallibly before hot loops, using APIs such as `try_reserve`; ensure pushes cannot unexpectedly exceed capacity in a supposedly nonallocating region. Track query scratch, parsed term bytes, position windows, and top-k entries against the same query budget.

Not every allocator or operating-system OOM is recoverable. The design must minimize infallible allocations on user-controlled sizes, avoid unbounded growth, and test backend/process failure recovery. Do not claim that a Rust collection automatically converts every OOM into a clean SQL error. [Rust-Vec]

### 17.9 Build profile and code quality

Use an explicit release profile as a starting experiment:

```toml
# Proposed initial policy; validate with the selected toolchain and pgrx release.
[profile.release]
opt-level = 3
lto = "thin"
panic = "unwind"
overflow-checks = true
debug = "line-tables-only"
```

Measure codegen-unit choices, thin versus full LTO, and profile-guided optimization on end-to-end workloads. Do not disable overflow checks globally; use checked arithmetic for persistent lengths and resource accounting regardless of profile. Keep useful symbols for production diagnostics. Cargo and rustc's official profile/PGO documentation are the authority for these switches. [Cargo-Profiles] [Rust-PGO]

Run formatting, Clippy, rustdoc checks, and explicit unsafe lints. Ban unexplained `unwrap`, `expect`, and `unreachable_unchecked` in server-facing paths. Test-only assertions can be stronger; production corruption handling must remain controlled.

### 17.10 Naming clarity

Rust's `std::pin::Pin` is unrelated to a PostgreSQL buffer pin or the Pin extension. Use `RustPin` as an import alias when that type is needed, and use concrete names such as `BufferPinGuard` for database resources. This avoids a subtle but recurrent terminology confusion. [Rust-Pin]

## 18. SIMD, I/O, parallelism, and throughput

### 18.1 Optimization order

The preferred order is: algorithmic pruning; representation size; fewer allocations; better locality; fewer lock acquisitions; useful I/O overlap; then specialized instructions. Measure the full query after each change. A faster bitmap kernel can have no effect when heap fetches dominate.

Hot-path targets include group/page bitmap operations, offset intersection/union, popcount, delta decoding, ASCII analysis, and score batches. Cold administrative paths should stay simple and robust.

### 18.2 CPU dispatch

Implement scalar `u64` operations first, including `count_ones` and `trailing_zeros`. Add architecture-specific kernels behind a safe dispatch layer. Detect CPU features once per backend or immutable initialization context, not once per posting. `OnceLock` can hold process-local immutable dispatch selection; it is not a shared-memory publication primitive. [Rust-U64] [Rust-OnceLock]

Use stable `std::arch`/`core::arch` and target-feature annotations correctly. The retrieved `std::simd` documentation still marks portable SIMD nightly-only, so do not make that a stable-release dependency. [Rust-Arch] [Rust-TargetFeature] [Rust-SIMD]

| Variant | Intended use | Required discipline |
|---|---|---|
| Scalar | Universal baseline and oracle | Always available and forceable in tests |
| AVX2 | Wide bitmap operations and selected decode kernels | Runtime feature check; safe tails and valid load ranges |
| AVX-512 | Selected wide operations/popcount where beneficial | Check exact required features, including `avx512vpopcntdq` for that popcount intrinsic |
| AArch64 NEON | Equivalent vector operations | Independent cross-architecture differential suite |

AVX2 does not automatically supply AVX-512 vector popcount. Unaligned vector loads still require the entire loaded range to be valid. A masked or padded tail must be justified by the specific intrinsic's contract, not by “the next page is probably mapped.” [Rust-AVX2-And] [Rust-AVX2-Load] [Rust-AVX512-Popcount] [Rust-NEON-Popcount]

Distributable binaries must not be compiled with an uncontrolled `target-cpu=native`. Let an administrator force scalar/AVX2/AVX-512 for diagnosis and comparison. Benchmark any frequency/power or contention effects on the actual deployment CPUs rather than assuming the widest ISA always wins.

### 18.3 I/O follows PostgreSQL ownership

Start with supported buffer reads and appropriate access strategies. Do not mmap the index as an independent authoritative cache or issue raw writes around the buffer/WAL manager. Group candidate heap fetches where SQL ordering permits it, and prefetch only data likely to be used.

PG18's internal read-stream interface is a candidate for batching immutable index reads and maintenance. Its callbacks and batching flags have specific restrictions, including when blocking work may occur. Wrap the target version's API carefully; do not assume that enabling a flag makes arbitrary nested I/O safe. [PG-ReadStream-Source] [PG-Buffer-API]

The experiment must measure cold and warm workloads, read amplification, outstanding pins, I/O depth, and memory. Aggressive prefetch that evicts useful heap pages can reduce overall throughput.

### 18.4 Parallel query execution

Parallelize only after serial execution is stable and worker startup is justified by query size. A useful work unit is a disjoint heap-group range, with a worker responsible for all sources relevant to that range. This avoids accidental double counting when segments overlap physically.

Share only immutable query parameters and validated offset-based coordination state. Transfer/reconstruct snapshots and score epochs through PostgreSQL's supported parallel-execution machinery. Implement DSM estimation, initialization, worker attachment, rescan, shutdown, and failure cleanup before advertising parallel safety. [PG-Custom-Execution] [PG-Parallel-Safety]

For exact parallel top-k, disjoint partitions with the same total ordering can each produce a sufficient local competitive set, followed by an exact merge. Visibility and all necessary filters must be applied before local thresholds rise. OFFSET, ties, and unsupported expressions can invalidate a simplistic “k per worker” rule and need explicit handling or fallback.

### 18.5 Parallel build and maintenance

Parallel builds should partition analysis/sorting work under one total `maintenance_work_mem`-derived budget, not allocate the full budget independently in every worker. Use PostgreSQL's build snapshot and heap-root behavior. Write bounded runs to PostgreSQL-managed temporary storage and merge them into recoverable index pages.

Compaction should start with one maintenance worker per eligible index operation and expand only when I/O, lock, and memory measurements justify it. Read throughput often improves more from eliminating excessive source fanout than from adding workers to every query.

### 18.6 Throughput and fairness

Maintain separate limits for foreground-query work, writer buffering, and background compaction. A background process should yield or back off under foreground pressure. An overloaded writer path must expose backpressure instead of creating unbounded mutable segments.

Instrument time waiting for locks, pins, WAL, heap reads, and compaction. CPU flame graphs alone cannot explain throughput limited by a cleanup pin or WAL flush. A sustained run must show a stable maintenance backlog and acceptable write p99, not just a peak read-QPS number.

## 19. Memory budgets and operational controls

### 19.1 Budget by concurrency

A proposed host-memory model is:

```text
host demand = shared_buffers (counted once)
            + other PostgreSQL shared memory
            + active backends * private baseline
            + sum(active Pin query budgets)
            + build and maintenance worker budgets
            + other executor-node memory
            + OS cache and system headroom
```

For illustration, 64 concurrent Pin queries each using 8 MiB consume 512 MiB of private query memory before the rest of the server is counted. That is arithmetic, not a measured Pin footprint. Do not sum process RSS blindly and count shared buffers once per backend; use appropriate process/shared/cgroup measurements.

“Full utilization of memory” should mean useful bounded work and a high cache hit rate, not allocating every available byte. Leave headroom for PostgreSQL, the operating system, replication, connections, and workload spikes.

### 19.2 Proposed memory controls

| Control | Intended semantics |
|---|---|
| `pin.query_memory_limit` | Hard budget for Pin-private work in one query context; no silent truncation |
| `pin.max_query_terms` / `pin.max_query_depth` | Parser/planner complexity limits |
| `pin.max_expanded_terms` | Bound prefix and future wildcard expansion |
| `pin.max_document_bytes` / `pin.max_positions` | Explicit ingest limits with transaction-safe failure |
| `pin.mutable_target_bytes` | Target source size before sealing; not permission to exceed all memory limits |
| `pin.max_searchable_sources` | Trigger compaction/backpressure before read amplification becomes unbounded |
| `pin.maintenance_memory_limit` | Cap below/within the relevant PostgreSQL maintenance budget |
| `pin.maintenance_workers` | Cluster-level resource limit, not unlimited per-index spawning |
| `pin.enable_count_fastpath` / `pin.enable_topk` | Diagnostic/performance switches with equivalent semantics |
| `pin.cpu_mode` | Automatic dispatch or supported forced implementation for diagnosis |

These names are proposed, not registered GUCs. Distinguish server-start controls, session controls, and index reloptions; validate ranges and permissions. Do not allow ordinary users to request arbitrary cluster-wide memory or workers.

### 19.3 Query complexity budget

Target approximately `O(query_terms * source_fanout + batch_scratch + k)` private search state, plus bounded dictionary/query bytes. Positions are streamed; exact counts do not store all matching TIDs in the custom path. The ordinary `TIDBitmap` path uses PostgreSQL's memory/lossification behavior and may incur extra rechecks.

A large k may need a core sort/spill plan rather than Pin's small-heap specialization. Budget integer arithmetic is checked. Track both logical allocated bytes and allocator capacity, and retain only intentionally bounded reusable arenas between operations.

### 19.4 Operational metrics

Expose counters/histograms or sampled measurements for queries, candidates, matches, heap fetches, VM eligibility, decoded bytes, positional work, source count, merge backlog, liveness debt, owner-pin duration, lock wait, WAL bytes, and private-memory peak.

Integrate with PostgreSQL's existing statistics and progress facilities rather than replacing them with an invisible logging subsystem. Exact available fields must be checked for the supported release. [PG-Stats]

Metric collection must not require an atomic increment for every individual posting. Accumulate per-query/per-batch values and publish periodically. Avoid putting full user queries or document terms into logs by default.

## 20. Repository structure and file responsibilities

### 20.1 Dependency direction

Use a Cargo workspace with six small-purpose packages rather than one monolith:

```text
pin-pg -> pin-core -> pin-codec -> pin-kernels
              \-----------------> pin-kernels

pin-testkit -> pure crates / independent oracles
xtask       -> build/test/release orchestration only
```

Each arrow points from a dependent package to the package it uses; not every module needs every allowed dependency. `pin-kernels` must not depend on PostgreSQL or the high-level query engine. `pin-core` must not depend on pgrx. `pin-pg` owns all host-runtime integration. Avoid a cycle created by putting shared domain types in the wrong crate.

Cargo workspace metadata, lints, profiles, and the lockfile live at the root. Feature selection must not accidentally enable multiple PostgreSQL-major bindings in one binary. Validate the resolved feature graph, not just individual manifests. [Cargo-Workspace] [Cargo-Features]

### 20.2 Proposed tree

This is the **target organization**, not a mandate to create empty files before they have real responsibilities. The comments define each file's purpose. Split a module further only when separate ownership, invariants, or testability justify it.

```text
pin/
├── Cargo.toml                     # Workspace, shared dependencies/lints, release profiles
├── Cargo.lock                     # Reproducible reviewed dependency resolution
├── rust-toolchain.toml             # Exact compiler and required tools
├── rustfmt.toml                    # Small stable formatting policy
├── clippy.toml                     # Documented project-specific lint configuration
├── deny.toml                       # Dependency/license/advisory policy
├── .editorconfig                  # Consistent whitespace and file endings
├── .gitignore                     # Generated SQL, build output, local PG data, benchmark data
├── Makefile                       # Thin shortcuts to xtask; no second build system
├── README.md                      # Honest features, install, examples, support matrix, limits
├── ARCHITECTURE.md                 # Maintained architectural overview linked to detailed docs
├── CONTRIBUTING.md                 # Reproducible workflow, source-first rule, reviews, tests
├── AGENTS.md                       # Same implementation rules for automated contributors
├── SECURITY.md                     # Private reporting, severity, support, disclosure process
├── CODE_OF_CONDUCT.md              # Community behavior and real reporting route
├── GOVERNANCE.md                   # Maintainer authority, decisions, conflict resolution
├── MAINTAINERS.md                  # Actual maintainers and subsystem responsibilities
├── SUPPORT.md                      # Supported versions/platforms and bug-report requirements
├── CHANGELOG.md                    # User-visible behavior, compatibility, performance changes
├── LICENSE-APACHE                  # Apache 2.0 text for original dual-licensed contributions
├── LICENSE-MIT                     # MIT text for original dual-licensed contributions
├── NOTICE                         # Required and appropriate upstream attribution
├── THIRD_PARTY.md                  # Dependency/source/data provenance and license inventory
├── DCO                            # Contribution sign-off policy/reference
│
├── crates/
│   ├── pin-core/
│   │   ├── Cargo.toml             # Pure safe engine; no pgrx dependency
│   │   └── src/
│   │       ├── lib.rs             # Public engine API; forbid unsafe
│   │       ├── types.rs           # RootTid, document/segment identities, bounded domain types
│   │       ├── error.rs           # Structured non-PG errors without hot-path formatting
│   │       ├── budget.rs          # Checked query/document work and memory limits
│   │       ├── analysis/
│   │       │   ├── mod.rs         # Analyzer trait/contracts and token stream
│   │       │   ├── profile.rs     # Immutable semantic profile/version definitions
│   │       │   ├── unicode.rs     # Reviewed segmentation/normalization adapter
│   │       │   └── ascii.rs       # Equivalent fast path, tested against unicode.rs
│   │       ├── query/
│   │       │   ├── mod.rs         # Validated query entry points
│   │       │   ├── ast.rs         # Bounded typed Boolean/phrase/prefix representation
│   │       │   ├── parser.rs      # Syntax, escaping, precedence, complexity limits
│   │       │   ├── normalize.rs   # Semantics-preserving rewrites without clause explosion
│   │       │   └── reference.rs   # Straightforward document-at-a-time semantic oracle
│   │       ├── search/
│   │       │   ├── mod.rs         # Search inputs, iterator and batch contracts
│   │       │   ├── cursor.rs      # Forward group/page/offset advancement and skips
│   │       │   ├── boolean.rs     # Mixed-container AND/OR/NOT and exact deduplication
│   │       │   ├── phrase.rs      # Lazy exact positional matching
│   │       │   ├── universe.rs    # Non-null complete-document universe semantics
│   │       │   └── batch.rs       # Bounded candidates, never assumed snapshot-visible
│   │       └── rank/
│   │           ├── mod.rs        # Versioned scoring interfaces
│   │           ├── bm25.rs       # Exact specified formula and finite-input validation
│   │           ├── statistics.rs # Immutable StatsEpoch semantics and validation
│   │           ├── bounds.rs     # Conservative block/query bounds and rounding proof
│   │           ├── wand.rs       # Gated pruning scheduler; exhaustive equivalence tests
│   │           └── topk.rs       # Eligible-only threshold heap and total-order policy
│   │
│   ├── pin-codec/
│   │   ├── Cargo.toml             # Standalone format and checked byte-view library
│   │   └── src/
│   │       ├── lib.rs             # Small validated public encoding/decoding surface
│   │       ├── header.rs          # Magic/version/feature/page-payload headers
│   │       ├── bytes.rs           # Checked endian readers/writers and extent arithmetic
│   │       ├── varint.rs          # Bounded integer codec with malformed-input tests
│   │       ├── container.rs       # Singleton/list/run/dense offset and page encodings
│   │       ├── group.rs           # Heap-group directories and skip metadata
│   │       ├── dictionary.rs      # Exact term blocks and prefix directory format
│   │       ├── positions.rs       # Positional checkpoints and streaming decoder
│   │       ├── document.rs        # Owner/incarnation/publication/length payload format
│   │       ├── manifest.rs        # Durable source ownership/state encoding
│   │       ├── statistics.rs      # Versioned statistics/impact metadata encoding
│   │       ├── validate.rs        # Structural cross-reference and range validation
│   │       └── unchecked.rs       # Optional measured inner loops behind validated inputs
│   │
│   ├── pin-kernels/
│   │   ├── Cargo.toml             # Stable target-specific kernels only
│   │   └── src/
│   │       ├── lib.rs             # Safe slice-based kernel interface
│   │       ├── scalar.rs          # Portable implementations and differential baseline
│   │       ├── dispatch.rs        # One-time feature detection; forced-mode diagnostics
│   │       ├── x86_avx2.rs        # Audited AVX2 routines and tail handling
│   │       ├── x86_avx512.rs      # Exact-feature-gated wide/popcount routines
│   │       └── aarch64_neon.rs    # Audited AArch64 equivalents
│   │
│   ├── pin-pg/
│   │   ├── Cargo.toml             # cdylib, one PG-major feature, exact pgrx selection
│   │   ├── pin.control            # Extension version, library, schema, requirements
│   │   ├── build.rs               # Reproducible minimal C-shim compilation/ABI checks
│   │   ├── cshim/
│   │   │   ├── pin_shim.h         # Narrow versioned C/Rust boundary declarations
│   │   │   ├── pin_shim.c         # Macros/inlines and audited host-only operations
│   │   │   └── abi_asserts.c      # Sizes, alignment, constants, supported PG assumptions
│   │   ├── sql/
│   │   │   ├── bootstrap.sql      # Ordered hand-maintained AM/opclass bootstrap pieces
│   │   │   └── upgrades/          # Explicit SQL upgrades; never silently rewrite disk format
│   │   └── src/
│   │       ├── lib.rs             # Module magic, _PG_init, hook/preload registration
│   │       ├── config.rs          # GUC and physical-reloption parsing/permissions
│   │       ├── compatibility.rs   # PG version, encoding, BLCKSZ, table-AM support checks
│   │       ├── error.rs           # Structured error-to-SQLSTATE mapping at guarded edges
│   │       ├── ffi/
│   │       │   ├── mod.rs         # Allowlisted low-level imports and unsafe rules
│   │       │   ├── memory.rs      # Context-owned values; no allocator ownership confusion
│   │       │   ├── resources.rs   # ResourceOwner registration and idempotent release
│   │       │   ├── buffers.rs     # Pin/content-lock/page-view guards
│   │       │   ├── snapshots.rs   # Snapshot lifetime and supported-context validation
│   │       │   └── interrupts.rs  # Safe cancellation/yield boundaries
│   │       ├── am/
│   │       │   ├── mod.rs         # AM surface and callback registration
│   │       │   ├── handler.rs     # Exact IndexAmRoutine capabilities
│   │       │   ├── build.rs       # Core build scan, bounded runs, progress
│   │       │   ├── insert.rs      # Insertion/cleanup callback adapter
│   │       │   ├── scan.rs        # Begin/rescan/end and gated synchronous tuple scan
│   │       │   ├── bitmap.rs      # Streaming TIDBitmap adapter and recheck flags
│   │       │   ├── vacuum.rs      # Callback authority, multi-round stats, cleanup
│   │       │   └── opclass.rs     # Opclass signature/profile validation
│   │       ├── storage/
│   │       │   ├── mod.rs         # Storage protocol interfaces, no SQL planner logic
│   │       │   ├── page.rs        # PostgreSQL page wrapper and typed validated payloads
│   │       │   ├── allocator.rs   # Extent allocation/free-list and generation checks
│   │       │   ├── wal.rs         # Generic-WAL wrapper; nonescaping page images
│   │       │   ├── journal.rs     # Durable operation/allocation state and orphan discovery
│   │       │   ├── manifest.rs    # Atomic source-set publication and registration
│   │       │   ├── mutable.rs     # Searchable mutable dictionaries/postings
│   │       │   ├── publish.rs     # Complete-document state machine
│   │       │   ├── document.rs    # Authoritative owner/liveness directory access
│   │       │   ├── seal.rs        # Writer handoff and freeze completion
│   │       │   ├── merge.rs       # Correct copying merger and removal reconciliation
│   │       │   ├── reclaim.rs     # Reachability-aware reclaim; no premature reuse
│   │       │   ├── read_stream.rs # Optional PG18 I/O batching adapter
│   │       │   └── verify.rs      # Online/offline structural verification primitives
│   │       ├── planner/
│   │       │   ├── mod.rs         # Hook chaining and legal path insertion
│   │       │   ├── eligibility.rs # SQL/security/isolation/limit proof predicates
│   │       │   ├── cost.rs        # Calibrated ordinary and custom cost models
│   │       │   ├── binding.rs     # Expression/OID/profile/score-to-scan identity
│   │       │   └── upper.rs       # Narrow ranked/count upper-plan alternatives
│   │       ├── executor/
│   │       │   ├── mod.rs         # CustomScan methods and lifecycle
│   │       │   ├── state.rs       # EState/statement/scan ownership separation
│   │       │   ├── fetch.rs       # HOT-aware root-to-visible tuple fetch
│   │       │   ├── visibility.rs  # VM protocol; certification is explicit, not a bool guess
│   │       │   ├── count.rs       # Exact mixed certified/heap-checked count path
│   │       │   ├── ranked.rs      # Visible-and-eligible top-k adapter
│   │       │   ├── statistics.rs  # Statement-level epoch registry and access control
│   │       │   └── parallel.rs    # Gated DSM workers, disjoint work, rescan/shutdown
│   │       ├── sql/
│   │       │   ├── mod.rs         # SQL surface and explicit volatility/strictness
│   │       │   ├── query_type.rs  # Versioned query input/output/send/receive
│   │       │   ├── matching.rs    # Correct sequential predicate fallback
│   │       │   ├── scoring.rs     # Explicit-context scalar scoring fallback
│   │       │   └── admin.rs       # Permission-checked information/check/maintenance functions
│   │       ├── workers/
│   │       │   ├── mod.rs         # Worker registration and lifecycle
│   │       │   ├── scheduler.rs   # Bounded fair work queue and backpressure
│   │       │   └── compactor.rs   # Transactional maintenance execution/retry/cancel
│   │       └── observability/
│   │           ├── mod.rs        # Low-overhead metric aggregation
│   │           ├── explain.rs    # Honest estimated/actual execution details
│   │           ├── progress.rs   # Build/maintenance phase reporting
│   │           └── wait_events.rs# Named extension waits where supported
│   │
│   └── pin-testkit/
│       ├── Cargo.toml             # Test-only reusable generators and oracles
│       └── src/
│           ├── lib.rs             # Deterministic test utilities
│           ├── documents.rs       # Seeded corpora, lengths, repetition, Unicode cases
│           ├── sets.rs            # Independent simple set/phrase reference algorithms
│           ├── ranking.rs         # Exhaustive scoring and top-k oracle
│           └── schedules.rs       # Named concurrency schedules and invariant assertions
│
├── xtask/
│   ├── Cargo.toml                 # Development CLI; no runtime extension dependency
│   └── src/
│       ├── main.rs                # Explicit reproducible subcommands
│       ├── environment.rs         # Validate toolchain, PG config, dependency lock
│       ├── test.rs                # Pure/regression/isolation/crash suite orchestration
│       ├── bench.rs               # Reproducible benchmark manifest and result collection
│       └── release.rs             # Package, provenance, checksums, compatibility checks
│
├── docs/
│   ├── blueprint.md               # This research charter as the initial design baseline
│   ├── format-v1.md               # Normative bytes, feature bits, invariants, upgrade rules
│   ├── concurrency.md             # Lock order, publication, VM, VACUUM and reclamation proofs
│   ├── sql-semantics.md            # Exact operator/analyzer/scoring behavior and exclusions
│   ├── unsafe-audit.md             # Unsafe inventory, proof owner, tests and benchmark evidence
│   ├── api-evidence.md             # Official API/source contract ledger pinned to commits
│   ├── performance.md             # Workload definitions, metrics, current verified results
│   ├── operations.md              # Install, backup, restore, replica, vacuum, incident runbooks
│   ├── compatibility.md           # SQL/disk/PG/ISA support and migration matrix
│   └── adr/
│       ├── 0001-native-am.md       # Why own AM rather than a GIN opclass/embedded engine
│       ├── 0002-document-id.md     # Root/incarnation model and reuse handling
│       ├── 0003-publication.md     # Multi-record atomic document coverage
│       ├── 0004-visibility.md      # Count/VM proof and restricted eligibility
│       ├── 0005-scoring.md         # Formula, epochs, finite ordering and pruning bounds
│       ├── 0006-reclamation.md     # Generation lifetime, VACUUM reachability, merge rules
│       └── 0007-standby-replay.md  # Recovery-time reader conflicts, retention, and support gate
│
├── tests/
│   ├── regression/sql/            # SQL tests grouped by semantics/lifecycle/security
│   ├── regression/expected/       # Reviewed expected output for corresponding SQL files
│   ├── isolation/specs/           # Deterministic multi-session schedules
│   ├── isolation/expected/        # Expected outcomes including allowed serialization errors
│   ├── recovery/                  # TAP/process tests: crash, checkpoint, replica, PITR
│   ├── upgrade/                   # Install/upgrade/reindex/dump/restore compatibility tests
│   ├── corpus/                    # Small licensed deterministic semantic fixtures
│   └── format-golden/             # Versioned binary payload fixtures with provenance
│
├── fuzz/
│   ├── Cargo.toml                 # Isolated fuzz tooling, not a production dependency
│   └── fuzz_targets/
│       ├── query.rs               # Parser/typed-query bounds and round-trip
│       ├── containers.rs          # Decoder and mixed set-operation equivalence
│       ├── positions.rs           # Position decoding and phrase equivalence
│       ├── page.rs                # Malformed page payloads; no unchecked escape
│       └── manifest.rs            # Reachability/state-machine format validation
│
├── benches/
│   ├── kernels.rs                 # Scalar/SIMD throughput with varied density/tails
│   ├── codecs.rs                  # Bytes and cycles for real posting distributions
│   ├── analysis.rs                # UTF-8/ASCII/long-document analysis cost
│   ├── search.rs                  # Pure Boolean/phrase/ranking benchmark matrix
│   ├── workloads/                # Versioned SQL/client workload definitions
│   └── manifests/                # Hardware/settings/dataset/toolchain result metadata
│
├── packaging/
│   ├── README.md                  # Supported install layout and package policy
│   ├── debian/                    # Reproducible Debian-family packaging when supported
│   ├── rpm/                       # Reproducible RPM-family packaging when supported
│   └── container/                 # Pinned development/test images, not a hidden database fork
│
└── .github/
    ├── CODEOWNERS                 # Required subsystem/security/unsafe reviewers
    ├── PULL_REQUEST_TEMPLATE.md   # Contract citations, tests, safety, performance, migration
    ├── ISSUE_TEMPLATE/            # Bug, corruption, performance, design-request templates
    └── workflows/
        ├── checks.yml             # Format, lint, rustdoc, dependency and source-policy checks
        ├── postgres.yml           # Supported PG integration/regression/isolation matrix
        ├── recovery.yml           # Crash/replication/upgrade gates
        ├── fuzz.yml               # Bounded CI fuzz plus preserved regression cases
        ├── miri-sanitizers.yml    # Pure Rust Miri and applicable instrumented integration
        ├── performance.yml       # Controlled regression runs; not noisy shared-runner claims
        └── release.yml           # Signed artifacts, provenance, SBOM, install verification
```

Generated extension SQL such as `pin--0.1.0.sql` is a release/build artifact. Choose one authoritative generation path using the selected pgrx tooling plus explicit bootstrap/upgrade fragments. Do not maintain a second hand-written copy that drifts from Rust-exported function declarations. PostgreSQL's extension packaging and PGXS documentation remain required references even when Cargo/pgrx orchestrates the build. [PG-Extensions] [PG-PGXS]

### 20.3 Component expectations

| Component | It must optimize for | It must not do |
|---|---|---|
| Analysis/query | Stable semantics, bounded work, one-pass reuse | Change results based on chosen index |
| Codec | Small checked format, selective decode, compatibility | Trust disk lengths or Rust native layout |
| Kernels | Fast pure operations with safe dispatch | Know snapshots, files, locks, or SQL |
| Storage | Durable publication and safe reuse | Reimplement transaction visibility |
| AM | Correct PostgreSQL lifecycle | Claim capabilities it cannot honor |
| Planner | Proven equivalent alternatives and honest cost | Force plans or pattern-match SQL strings |
| Executor | Snapshot/security/qualification correctness | Let candidates masquerade as final rows |
| Maintenance | Bounded debt and foreground fairness | Make accepted writes depend on worker availability |
| Observability | Explain cost and correctness decisions | Leak text or add per-posting contention |

### 20.4 File-level standards

Every public module begins with its responsibility, invariants, ownership/lifetime, and relevant official-source references. Every public fallible API documents errors and resource effects. Unsafe APIs additionally document their safety contract and callers' obligations.

A file is not complete because it compiles. Its definition of done includes a reference/oracle where meaningful, unit/integration tests for its failure modes, diagnostics, and a benchmark for claimed hot-path improvements. Do not write a generic framework unless a concrete second use justifies its complexity.

## 21. Implementation phases and acceptance gates

### 21.1 Gate map

```text
G0 ABI / scope / proof prototypes
            |
G1 semantics + pure codecs + reference engine
            |
G2 durable mutable index + ordinary bitmap/heap path
            |
G3 sealing + copying compaction + VACUUM/reclamation
            |
G4 synchronous scan protocol + exact heap-checked ranked execution
          /   
G5 VM/count proof      G6 measured SIMD + I/O + allocation optimization
          \   /
G7 selected parallelism / optional copy-avoiding merges
            |
G8 operational qualification and release
```

This is a dependency map, not a calendar estimate. Features that do not pass their gate remain disabled or unsupported with a correct fallback. The first useful alpha arrives before specialized count and SIMD paths; a performance-parity claim requires the later evidence, not just an alpha label.

### G0 — Establish a buildable, auditable host boundary

**Deliver:** a reproducible workspace, exact compatible toolchain/pgrx set, extension load/unload/error-path smoke tests, a minimal AM registration spike, ABI assertions, support matrix, and initial ADRs for physical identity, publication, liveness ownership, and statistics semantics.

**Investigate before format lock:** how the document-owner leaf satisfies the synchronous pin rule; how VACUUM reaches retired sources; how statement statistics epochs bind across score expressions; how required preload/shared state is initialized; how generic WAL handles the selected page layout; and whether standby index reads are excluded or have a replay-safe reclamation design.

**Exit:** tests run on a clean supported PostgreSQL installation; no reliance on a private server fork; every unsafe boundary has a named reviewer and an official contract link. Unresolved correctness questions are written as blocking proof obligations, not buried in TODOs.

### G1 — Pure semantics and checked representation

**Deliver:** typed query parser, frozen analysis profile, sequential document oracle, exact Boolean/phrase engine, checked posting/position codecs, document/manifest encodings, and exhaustive reference BM25/top-k.

**Exit:** round-trip and malformed-input fuzzing; deterministic Unicode fixtures; mixed-container equivalence across sparse/dense/tail cases; explicit resource limits; exact scoring fixtures. No PostgreSQL pointers enter these crates. Disk format is still explicitly experimental.

### G2 — Durable searchable index baseline

**Deliver:** logged permanent-table `ambuild`, `aminsert`, `aminsertcleanup`, core-backed heap build scan, multi-record complete-document publication, ordinary `amgetbitmap`, correct rechecks, VACUUM callback integration, and recovery-safe orphan handling.

**Exit:** indexed results equal sequential predicate results under concurrent writes, abort/savepoint/speculative insertion, HOT/non-HOT changes, and restart. Own writes are discoverable under correct PostgreSQL command/snapshot semantics. Every publication transition has a crash injection test. No background worker is required to see accepted inserts.

### G3 — Sustainable segments and reclamation

**Deliver:** bounded mutable sources, safe freeze/handoff, compressed immutable segments, generation manifests, a copying merger, deletion reconciliation, reachable-generation liveness updates, resource-owned readers, free-space reclamation, and backpressure metrics.

**Exit:** no missing/duplicate matches during concurrent seal/merge/VACUUM; repeated slot reuse cannot resurrect terms; worker/backend death leaves reclaimable state; sustained ingestion reaches stable source fanout and maintenance debt. Long snapshots/queries delay reclamation safely and visibly.

### G4 — Exact ranked execution and plain-scan protocol

**Deliver:** proven document-owner pin/cleanup protocol, `amgettuple` where supported, HOT-aware fetch adapter, explicit-context scalar scoring API, statement epoch registry, conservative CustomScan eligibility, and exact visible top-k with initially simple bounds.

**Exit:** exhaustive ranking equivalence with the same scoring epoch; invisible/filtered high-score adversarial tests; rescan/prepared/self-join correctness; no RLS/SSI/row-lock bypass; cancellation releases pins. Ordinary plans remain available and equivalent.

### G5 — Visibility-map acceleration and exact direct counts

**Deliver:** written VM/publication/liveness proof, deterministic interleaving model, sealed-source certification, mixed certified/heap-checked count execution, narrow upper-plan integration, and detailed fallback instrumentation.

**Exit:** counts match core aggregation under old snapshots, deletion/reuse, HOT, merge, failed insert, VM transitions, and cancellation. No source is counted merely because a global live bit or stored DF says so. Fast-path disablement changes only performance. Mutable certification, RLS, or serializable custom behavior remains excluded until individually proven.

### G6 — Measured low-level optimization

**Deliver:** CPU-dispatched kernels, allocation reductions, calibrated containers/group sizes, optional PG18 read-stream batching, and cost-model tuning. Keep a force-scalar/reference switch.

**Exit:** scalar/SIMD equivalence, tail/alignment tests, sanitizer/Miri coverage where applicable, and a reproducible end-to-end benefit at equal memory/durability. Reject microbenchmark-only wins that worsen write p99, memory, or maintenance stability.

### G7 — Selective parallelism and advanced merging

**Deliver only what earns acceptance:** parallel query/build/maintenance lifecycles; exact disjoint-work aggregation; optional safe payload sharing/ownership-transfer merge.

**Exit:** worker failure/rescan/shutdown cleanup; total memory accounting; no double counting; same ranked results; no reclaimed shared extents. Copy-avoiding merges additionally pass all publication/recovery/deletion tests. These features may remain off without blocking the correctness of the core engine.

### G8 — Operational qualification

**Deliver:** install/upgrade packages, complete support matrix, security review, backup/restore/PITR and physical-replica/promotion validation with an explicit hot-standby scan support policy, logical-replication expectations, DDL lifecycle tests, benchmark dossier, runbooks, release notes, and an open-source contribution process.

**Exit:** no known release-blocking wrong-result/data-loss/memory-safety/security bug; no unexplained sustained resource growth; reproducible build/install/restore; all advertised features tested on the published matrix. A benchmark-parity statement is permitted only for measured workloads and configurations.

### 21.2 Workstream ownership

Assign explicit responsibility for PostgreSQL integration and MVCC, durable storage/recovery, pure search/ranking, Rust/unsafe, and testing/performance. This does not require five separate teams, but it requires independent review across the highest-risk boundaries.

No one should approve their own new visibility shortcut, WAL publication protocol, or unsafe decoder solely on a local benchmark. Changes spanning storage and visibility require both subsystem perspectives. Security and release compatibility have named veto authority for unresolved data-loss or policy-bypass risk.

### 21.3 A practical first implementation slice

The first meaningful end-to-end slice is not a thousand-file scaffold. It is a small `text` match operator, a checked minimal posting representation, a WAL-backed complete-document record, a core-assisted build, and an ordinary bitmap/heap scan whose output equals the sequential operator before and after VACUUM and restart.

That slice tests the architectural assumptions at the point where mistakes are cheapest to fix. Once it is correct, add segments and ranking without changing the underlying identity/publication invariants.

## 22. Correctness, crash, security, and concurrency testing

### 22.1 Independent oracles, not two copies of the same bug

Maintain a deliberately simple document-at-a-time reference implementation. It analyzes one input, evaluates the query without compressed postings, and computes the documented score without pruning. The optimized executor must agree with it. Codec round trips alone are insufficient: an encoder and decoder can agree while both violate the format or query semantics.

At the PostgreSQL layer, compare sequential predicate evaluation, ordinary bitmap/heap execution, synchronous index scans when enabled, exhaustive ranking, and custom accelerated plans. Verify the selected plans with `EXPLAIN`; changing planner switches is not proof that the intended implementation ran. Compare rows as multisets where SQL has no ordering guarantee, and compare ordered results using the same explicit tie-breaker and statistics epoch where ordering is part of the contract.

Use PostgreSQL's regression infrastructure for SQL expectations and process-oriented tests for lifecycle/recovery. Keep the pure library tests runnable without starting PostgreSQL. These test layers address different contracts and should not replace each other. [PG-Tests]

### 22.2 Required test matrix

| Area | Adversarial cases | Required result |
|---|---|---|
| Analysis and parsing | Empty/null inputs, combining characters, normalization equivalents, malformed query syntax, deep nesting, long tokens, repeated terms | Defined semantics and bounded resources; no silent truncation |
| Boolean execution | Rare/common terms, overlap-heavy OR, nested NOT, empty universes, duplicated clauses, missing terms | Exact document membership; no duplicate counting |
| Phrase execution | Repeated words, overlapping phrases, positional gaps, large positions, many repeated occurrences | Same matches as the positional reference; checked bounds |
| Codec/container dispatch | Every sparse/dense/run pairing, empty/full containers, tails, boundary group numbers, corrupted lengths | Same scalar results; malformed bytes fail safely |
| Own writes | INSERT/SELECT, COPY, command-counter changes, triggers, savepoints, subtransactions | Visibility agrees with PostgreSQL's snapshot/command semantics |
| Abort/speculation | Aborted inserts, aborted updates, `ON CONFLICT`, failed statements, prepared transactions | No visible half-document and no permanent orphan leak |
| HOT | Multiple HOT versions, root redirects, pruning, unindexed-column updates, visible TID differing from root | Correct visible tuple and preserved index identity |
| Non-HOT updates | Indexed text changes, index-unchanged hints, moved tuple versions | Every required version indexed; old versions removed only when allowed |
| VACUUM | Repeated passes, no dead tuples, large dead sets, skipped cleanup, cancellation, concurrent inserts | Callback-approved cleanup without lost live references |
| Slot reuse | Repeated deletion and reuse of the same `(block, offset)` | No term resurrection or score metadata from the previous incarnation |
| Manifest lifecycle | Concurrent freeze, merge, reader registration, retirement, worker death | A complete source set; no missing, duplicate, or freed payload |
| Ranking | Invisible highest scores, rejecting residual filters, ties, large `k`, epoch replacement | Exact eligible top-k under the documented score contract |
| Direct counts | VM transitions, old snapshots, mutable/sealed overlap, deletion and slot reuse | Same count as core aggregation, or explicit fallback |
| Executor lifecycle | Rescan, parameter changes, prepared plans, self-joins, cursors, cancellation | No stale score/query state and no leaked resources |
| SQL integration | RLS, security barriers, joins, LIMIT/OFFSET, row locking, EvalPlanQual | Correct ordinary fallback where custom execution is ineligible |
| Isolation | Read Committed, Repeatable Read, Serializable including empty-result predicates | No isolation weakening or missed required conflict behavior |
| DDL | Concurrent build, failed build, REINDEX, TRUNCATE, DROP, table rewrite, partition lifecycle | Correct physical generation/invalidation and safe resource release |
| Durability/operations | Restart, crash, backup restore, PITR, promotion, version mismatch | Recoverable data or an explicit supported diagnostic; never silent corruption |

Tests for a proposed feature do not make that feature supported before its implementation gate passes. In particular, hot-standby index reads require the additional replay/lifetime work in section 24.3.

### 22.3 Deterministic schedules for the dangerous races

Introduce test-only synchronization points at publication, manifest acquisition, liveness lookup, VM lookup, candidate return, heap fetch, deletion reconciliation, and extent reuse. A test should stop each backend at the relevant boundary and drive the competing operation deliberately. Sleep-based stress alone is too weak and too flaky.

One mandatory scenario is **old term resurrection**. Index a row containing `alpha`; let a reader retain an old segment reference; delete the row; run the permitted VACUUM sequence; reuse the physical slot for a row containing only `beta`; then resume the reader and compactor. The legal outcome depends on the held snapshot and whether cleanup was allowed to proceed, but it may never be a visible `beta` row returned as an `alpha` match because an old posting survived. Assert both the expected wait/cancellation behavior and the result.

A second scenario places an invisible or filter-rejected row above every visible row in raw score. Run every top-k variant and show that this row cannot raise the competitive threshold. A third interleaves publication, VM certification, VACUUM, and a direct count, checking every proposed fast-path precondition rather than merely comparing an uncontended final count.

Build a small state-machine model of publication, owner registration, retirement, and reuse. Exhaustively explore bounded schedules. A model is useful only when the implemented transitions, locks, and recovery records are traceable to it; it is not a substitute for exercising real PostgreSQL backends.

### 22.4 Crash and failure injection

Inject failure before and after each durable transition: allocation reservation, fragment write, dictionary/posting reference, complete-document publication, source handoff, merge-output completion, manifest swap, liveness update, and free-list publication. Exercise both ordinary WAL deltas and full-page-image/checkpoint conditions.

Run controlled-cluster tests for backend termination, maintenance-worker termination, postmaster crash, disk exhaustion, WAL-volume exhaustion, allocation failure where injectable, statement timeout, cancellation, and interrupted index creation. After restart, check SQL equivalence, page integrity, reachable extent ownership, orphan reclamation, and the absence of unbounded debt. A crash-safe result includes the correct handling of transactions that did not commit; it does not mean preserving their rows.

Do not implement failure injection as an unrestricted production SQL facility. Compile dangerous hooks only into dedicated test builds and run them in disposable clusters. Persist the seed, workload, scheduling trace, server configuration, and last successful transition for every failure.

### 22.5 Rust and mixed-language validation

Fuzz query parsing, byte decoding, container operations, position streams, dictionary traversal, and manifest validation with size limits and a reference oracle. The Rust Fuzz project's `cargo-fuzz` documentation is the tool reference. Minimized crashing inputs become permanent regression fixtures. [Rust-Fuzz]

Use Miri for the pure Rust unsafe surface and compatible isolated harnesses. Its scope and supported operations must be understood; a Miri run of a codec does not validate PostgreSQL's C code, process concurrency, or an arbitrary FFI call. Run address/undefined-behavior instrumentation against a compatibly built PostgreSQL/extension stack where supported, and retain scalar/non-intrinsic test variants for tools that cannot execute a selected CPU kernel. [Rust-Miri]

A clean sanitizer run, high coverage, or millions of fuzz cases is evidence, not a proof of absence of bugs. Every unsafe operation still needs a local safety argument and every visibility shortcut still needs a protocol argument.

### 22.6 Security and hostile-input boundaries

Treat SQL text, query syntax, document size, index metadata, and on-disk lengths as untrusted at their respective boundaries. Corruption should produce a controlled PostgreSQL error with enough context to diagnose the affected index, not a process crash, out-of-bounds access, or a silent omission of matches.

Test role changes, revoked privileges, malicious `search_path` settings, RLS policies, security-barrier views, and multiple aliases of the same relation. Administrative inspection functions must check ownership/privileges and avoid exposing hidden terms or corpus statistics. Scoring functions that consult an explicitly named corpus need their own authorization policy; access to a text value alone is not authorization to inspect every row in that corpus. PostgreSQL's extension-installation and RLS rules are the baseline. [PG-CreateExtension] [PG-RLS]

Bound query complexity, prefix expansion, tokenization work, temporary files, retained pins, and lock wait behavior. Do not label functions `LEAKPROOF` to obtain a faster plan without a separate security proof. Do not expose arbitrary filesystem paths or raw memory through diagnostics. Dependency and release-pipeline security are part of the product, not optional documentation work.

### 22.7 Failure triage standard

A wrong-result, data-loss, memory-safety, or policy-bypass defect blocks release. Fixes must include a minimized reproducer, root cause, affected versions/features, invariant violated, and regression coverage. Repeated failures with different symptoms should trigger a protocol review, not a growing collection of special cases.

## 23. Benchmarking and evidence standards

### 23.1 Match the problem before comparing the speed

Publish a semantic matrix before a performance chart. Exact count is not approximate cardinality; Boolean matching is not ranked retrieval; phrase matching is not a conjunction; warmed read-only search is not sustained mixed traffic. A GIN baseline must be configured for comparable text-analysis and match semantics, while ranking comparisons must disclose differences in score formulas and corpus statistics. PostgreSQL documents the capabilities and recheck characteristics of its built-in full-text index choices. [PG-FTS-Indexes]

Use Tin's public benchmark materials as one reproducible workload source, not as the only definition of success. Comparisons to a hosted service must disclose differing hardware, storage, network, concurrency limits, cache state, and maintenance behavior. Without sufficiently comparable environments, report separate measurements rather than a universal speedup claim. [Tin-Bench]

### 23.2 Dataset and workload manifest

Every dataset needs a source/license, immutable identifier or checksum, ingestion procedure, schema, row count, raw text bytes, token/length distributions, distinct-term distribution, and query-generation seed. Include reproducible synthetic corpora for adversarial density and update patterns alongside representative real text. Do not distribute a corpus whose redistribution rights are unclear.

The read suite must cover rare-term lookup, common-term counts, asymmetric conjunctions, overlap-heavy disjunctions, exact phrases, negation, prefix expansion within limits, and ranked top-10/top-100/larger-k retrieval. Include both small returned projections and large TOASTed text, because materialization can change the bottleneck. Include empty results, highly selective residual filters, and many visible matches.

The mixed suite varies read/write ratios, writer count, insert/update/delete mix, document size, HOT eligibility, and maintenance capacity. Example ratios such as 95/5 or 80/20 are experiment configurations, not product promises. Run long enough to observe seal/merge cycles, checkpoint effects, VACUUM, and whether debt reaches a steady state. A benchmark that ends before deferred work becomes visible is incomplete.

### 23.3 Environment disclosure

Record CPU model and enabled instruction sets, cores/SMT, memory, NUMA topology, storage device/filesystem, kernel, PostgreSQL build and settings, exact extension commit, Rust toolchain, dependency lockfile, and build profile. Record connection counts and whether the load generator shares resources with the server.

Publish durability settings and keep them comparable. Do not silently turn off `fsync`, WAL durability, or maintenance on one candidate. Record `shared_buffers`, work-memory settings, checkpoint configuration, autovacuum configuration, Pin budgets, preload configuration, and any planner overrides. Explicitly distinguish cold-cache, warm-cache, and sustained steady-state runs; do not call a cache-dropping procedure portable or harmless without validating the platform-specific method.

### 23.4 Latency and throughput methodology

Measure both closed-loop saturation and rate-controlled offered load. In a closed-loop test, slow responses reduce the rate at which new requests arrive; do not let that hide queueing behavior. Record scheduled arrival time as well as actual start/finish time in a rate-controlled harness, and report overload, queue limits, timeouts, and rejected requests instead of dropping them from the distribution.

PostgreSQL's `pgbench` supports custom scripts and provides a useful official baseline harness. Verify its selected mode and reporting semantics for the target release; a custom search workload may additionally need a purpose-built driver. The load generator must have demonstrated spare capacity or run separately. [PG-Bench]

Repeat runs, randomize candidate order where appropriate, preserve raw samples, and report variation or uncertainty. Investigate regressions on controlled hardware rather than treating noise on a shared CI runner as a precise performance verdict. Report p50, p95, p99, maximum observed latency, achieved throughput, errors, cancellations, and run duration together.

### 23.5 The full resource scorecard

| Dimension | Required measurements |
|---|---|
| Query work | Candidates, groups/pages pruned, containers decoded, positions decoded, heap fetches, VM-certified pages |
| Memory | Peak private memory per backend, shared Pin allocation once, build/maintenance memory, temporary-file bytes, process/cgroup measurements |
| Storage | Complete index bytes including dictionaries, owner records, liveness, statistics, free/retired extents; bytes per indexed document/token |
| Writes | Insert/update/delete latency, WAL bytes, index bytes written, checkpoint/FPI effects, source fanout |
| Maintenance | Merge/VACUUM throughput, debt growth, oldest retained generation, stall duration, reclamation lag |
| Operations | Build and REINDEX time, restore/recovery time, replica replay rate, cancellation responsiveness |

Do not sum shared mappings as though each backend privately owns them, and do not report only Rust allocator bytes while ignoring PostgreSQL memory contexts and shared buffers. Explain the measurement method rather than presenting incompatible RSS totals as comparable memory footprints.

### 23.6 Ablation before optimization claims

Run controlled comparisons for group size, sparse/dense thresholds, position layout, copying versus shared-payload merging, scalar versus SIMD, exhaustive versus block-pruned ranking, and heap-checked versus VM-assisted/direct counts. Change one meaningful factor at a time before combining wins. Include small workloads where initialization overhead can make a sophisticated path slower.

A new unsafe kernel is accepted only when an end-to-end bottleneck is demonstrated, the safe/scalar path remains correct, and the gain survives realistic concurrency and memory constraints. A throughput gain accompanied by rising maintenance debt or unacceptable write p99 is not an unconditional improvement.

### 23.7 What may be claimed publicly

Publish the workload, versions, settings, correctness checks, raw results, and scripts with every headline. Use wording such as “on workload X, at concurrency Y, under configuration Z,” not “the fastest PostgreSQL search engine” from a narrow benchmark.

This blueprint contains **no measured Pin results**. Tin parity, memory parity, build speed, and throughput targets remain unmeasured until the extension exists and the benchmark dossier passes review.

## 24. Packaging, upgrades, replication, and operations

### 24.1 Installation and compatibility axes

Pin is a native server extension, not a SQL-only package. The proposed supported configuration requires installing the matching server binary and SQL/control files, configuring preload, restarting as required, and then creating the extension with appropriate privileges. A managed PostgreSQL provider must explicitly permit this native extension; do not promise installation on arbitrary hosted services through `CREATE EXTENSION` alone. PostgreSQL's extension and PGXS documentation define the packaging baseline. [PG-Extensions] [PG-PGXS] [PG-Preload]

Maintain separate compatibility dimensions: PostgreSQL major/minor and build assumptions; extension SQL version; shared-library ABI; on-disk format; analyzer/profile version; scoring/statistics version; and CPU architecture. One semantic version number cannot replace that matrix.

An unsupported disk/analyzer version must be rejected before unsafe decoding. `ALTER EXTENSION UPDATE` changes registered extension objects according to an upgrade script; it must not silently reinterpret existing index pages. State whether an upgrade is read-compatible, needs an explicit index rewrite, or requires REINDEX. A changed analyzer normally requires a deliberate rebuild or a separately designed multi-version query contract.

### 24.2 Backup, PITR, and promotion

Keep authoritative contents in PostgreSQL-managed relation storage and use the supported WAL protocol. That aligns Pin with physical backup/recovery infrastructure, but every advertised backup, restore, PITR, and promotion path still needs integration tests with incomplete publications, in-progress merges, and reclaimed extents. Recovery must rebuild coordination state without depending on lost process-local caches. [PG-Backup] [PG-PITR] [PG-Standby]

A generic-WAL-only implementation does not need a Pin-specific redo function to apply its generic page records. That does not remove the requirement to install compatible Pin code before using its SQL/index functionality. A future custom resource manager creates a stronger recovery-time library/ID compatibility obligation and must be documented as such. [PG-Generic-WAL] [PG-Custom-Rmgr]

After promotion, initialize primary-only maintenance safely, invalidate stale local coordination, and verify the active manifest against durable ownership metadata. Workers may not assume that a relation was quiescent merely because the process just started.

### 24.3 Separate physical replay from safe hot-standby index reads

**Important additional proof gate:** a primary's in-memory generation-reader registry does not know which old Pin sources a standby query is still reading. Primary-side compaction and extent reuse can therefore require a replay-time reader-conflict or retention protocol beyond ordinary primary reader registration.

This is not an invented PostgreSQL concern: upstream B-tree page-reuse code emits a standby conflict horizon, and hot-standby documentation describes cleanup/replay conflicts. The inspected generic redo routine applies registered page changes; it does not implement Pin's proposed generation ownership semantics. **Inference for Pin:** successful generic-WAL replay alone is insufficient evidence that concurrent standby Pin readers are safe. [PG-Btree-Page-Source] [PG-Btree-WAL-Source] [PG-Hot-Standby] [PG-Generic-Source]

The baseline release contract should support physical recovery and post-promotion use after qualification, but keep **Pin index/CustomScan reads during recovery disabled** until a replay-safe protocol passes review. Provide planner eligibility checks where supported and an executor guard before opening unsafe scan state; a missed planner exclusion must produce an explicit unsupported-operation error, not a risky scan. A separately validated sequential text predicate can remain usable without consulting Pin index storage. Corpus-dependent scoring is not automatically part of that fallback.

Evaluate retention tied to a valid standby conflict horizon, an extension-specific redo/conflict design, or another rigorously specified approach. Do not copy B-tree WAL records under its resource-manager ID, assume `hot_standby_feedback` protects arbitrary Pin segment generations, or disable reclamation indefinitely and call the resulting unbounded growth production-ready.

This decision is a deliberate feature restriction, not a claim that PostgreSQL cannot support the desired behavior. If standby search is a release requirement, move this proof into G0/G3 and complete it before 1.0; do not leave it to an operational footnote.

### 24.4 Logical replication

Distinguish physical index-page replay from logical table changes. Logical replication does not transport Pin's physical posting identities as a portable search index. The subscriber needs its own compatible schema/extension/index setup and maintains local physical identities when applying row changes. PostgreSQL documents logical replication's architecture and restrictions, including schema-management limitations. [PG-Logical] [PG-Logical-Limits]

Test subscriber-side index maintenance and replica-identity configurations actually advertised. Do not describe generic WAL being ignored by logical decoding as “logical replication cannot use tables with Pin”; the relevant contract is local index maintenance around replicated table DML, not shipping generic index-page records as logical documents.

### 24.5 Upgrades, DDL, and incident response

Qualify `pg_upgrade` explicitly with required binaries and on-disk compatibility. PostgreSQL major-version support requires ABI/source review and a tested migration path; an unchanged Rust API does not imply server ABI compatibility. Publish REINDEX requirements and a rollback plan before shipping a format-changing release. [PG-Upgrade]

Exercise concurrent builds, invalid-index cleanup, REINDEX, table rewrites, TRUNCATE, tablespace moves where supported, extension updates, and DROP with active scans/workers. Relation OIDs and block numbers must never be used as durable identities without their physical-generation context. Release relation locks and resource registrations through PostgreSQL's lifecycle, including error paths.

Provide runbooks for rising merge debt, blocked reclamation, excessive WAL, cancelled queries, corrupt pages, unsupported formats, and restore failures. A consistency checker reports and diagnoses; it must not “repair” corruption by silently dropping documents. The default recovery action for a confirmed index inconsistency should preserve the heap as source of truth and use a documented rebuild procedure after addressing the cause.

Observe minimum headroom for WAL, temporary files, index growth, and long-lived readers. Explain which settings are reloadable, session-local, or require restart. Remove old binaries only after the documented SQL, index-format, and retained-WAL compatibility obligations are satisfied.

## 25. Open-source licensing and contributor governance

### 25.1 License and provenance decision

**Recommended license for original Pin code: `Apache-2.0 OR MIT`.** Include the complete license texts as `LICENSE-APACHE` and `LICENSE-MIT`, use the corresponding SPDX expression in workspace/package metadata, and make contributor licensing explicit. Review the actual license texts rather than treating a short identifier as the full set of obligations. [License-Apache] [License-MIT]

This choice does not relicense copied upstream material. PostgreSQL-derived code retains its applicable PostgreSQL license and notices; Apache-licensed material retains applicable license/notice obligations. Prefer independently implemented algorithms from documented contracts, and record the origin of any adapted code, tests, or binary fixtures in `THIRD_PARTY.md`. PostgreSQL publishes its own license text. [License-PG]

Tin's public article is design inspiration, not permission to obtain, copy, or present proprietary implementation code as Pin's. Keep original implementation provenance auditable. Benchmark datasets, Unicode data, dependencies, and documentation assets also need a license inventory; source-code licensing alone is not enough.

`Pin` is the chosen project name, not a verified trademark or package-name availability claim. Before publishing, check the intended package registries, repository organization, and naming/trademark concerns. Do not assume the proposed internal crate names are available public registry names.

### 25.2 Contribution contract

Use a documented Developer Certificate of Origin sign-off workflow if adopted by the maintainers. The DCO is a contribution-origin certification, not a replacement for the project's license. Contributors must have the right to submit their work and identify adapted third-party material. [DCO-Official]

`CONTRIBUTING.md` must explain the reproducible development environment, exact toolchain selection, how to run each test tier, the official-documentation rule in section 26, and the expectations for a focused pull request. A patch should state the invariant or user problem, its PostgreSQL/Rust contracts, test evidence, compatibility impact, and measured performance impact when claiming a speedup.

`AGENTS.md` applies the same rules to automated contributors. Generated code does not receive a weaker review standard. Do not accept invented benchmark results, guessed PostgreSQL APIs, hand-waved unsafe blocks, or skipped failure-path tests because a patch is large or tool-generated.

### 25.3 Review and governance

Define maintainers and subsystem owners by actual people/roles before release; do not ship fake contact addresses. Require independent review for storage format changes, publication/reclamation protocols, snapshot/VM shortcuts, unsafe memory operations, and security-sensitive planner integration. Record architectural decisions in ADRs, including rejected alternatives and migration costs.

`GOVERNANCE.md` should define who can approve releases, resolve technical disputes, appoint maintainers, and respond to emergencies. `SECURITY.md` needs a working private reporting route, supported versions, severity handling, and coordinated-disclosure process. `CODE_OF_CONDUCT.md` needs an actionable reporting route rather than copied boilerplate with placeholders.

Keep performance discussions evidence-based. A maintainer may reject an optimization that improves one benchmark but weakens safety, observability, portability, or sustainable throughput. Format and correctness decisions require review before a release makes them expensive to reverse.

### 25.4 Supply chain and release assets

Commit dependency resolution, audit dependency/feature changes, minimize runtime dependencies, and generate an inventory/SBOM for release artifacts. Pin CI actions and build inputs to reviewed immutable versions where practical. Protect release credentials, restrict artifact-publishing permissions, and verify the packaged extension in a clean supported PostgreSQL installation.

Publish source, package checksums, build provenance, compatibility matrix, known limitations, upgrade instructions, and benchmark reproduction materials. A release package is incomplete when only the shared library is reproducible but the generated SQL or required migration scripts are not.

## 26. Implementation documentation policy and API register

### 26.1 Mandatory rule for every implementation change

Place the following policy in both `CONTRIBUTING.md` and `AGENTS.md`:

> Before implementing or changing a PostgreSQL callback, storage operation, visibility decision, WAL transition, planner/executor integration, or Rust unsafe operation, read the official documentation for the exact supported version and inspect the relevant upstream source/header when the contract is not fully documented. Before using a Rust or Cargo API whose behavior affects safety, ownership, layout, concurrency, portability, or performance, read its official documentation. Record the applicable contract, version/commit, local obligations, and validating tests in the code and `docs/api-evidence.md`. Do not substitute memory, a blog, a forum answer, or another extension's implementation for the authoritative contract. When the contract remains uncertain, preserve the correct fallback and resolve the uncertainty before enabling the optimization.

This requirement applies throughout implementation, not only during the initial research pass. A new helper that wraps a documented primitive must explain its additional local contract; merely pasting a URL above an unsafe block is insufficient.

### 26.2 Evidence ledger schema

Each material boundary entry should contain:

```text
Subsystem / local module:
Upstream symbol or documented interface:
Supported version and immutable source commit:
Official manual/API URL and relevant source path:
Input validity, ownership, lifetime, alignment and aliasing obligations:
Lock, pin, snapshot and error/unwind obligations:
Local invariant preserved / unsupported cases:
Unit, regression, isolation, crash or fuzz evidence:
Performance evidence, if optimization is the purpose:
Reviewer and last verified version:
```

Keep source links close to the implementation as well as in the ledger. For PostgreSQL major-version work, revalidate every boundary entry; for minor/toolchain updates, review the relevant upstream changes and rerun the support matrix. Floating `latest`, `main`, `develop`, and `REL_18_STABLE` links in this research document must become pinned implementation evidence.

### 26.3 Rust API register: intended building blocks

This register names the principal standard-library/API families expected in Pin. It does not pretend to enumerate future private functions that have not been designed. **Any additional implementation API inherits the same official-source requirement.** Intrinsic availability and stabilization must be verified against the chosen toolchain, not inferred from a current `latest` page.

| API / function family | Intended use | Required local rule | Official reference |
|---|---|---|---|
| `u32::from_le_bytes`, `to_le_bytes`, checked arithmetic | Portable fields, offsets, lengths | Check input length and addition/multiplication before slicing or allocation | [Rust-U32] |
| `u64::count_ones`, `trailing_zeros`, checked arithmetic | Scalar bitmaps and checked counters | Mask unused tail bits; do not treat zero's trailing-zero count as a valid bit index | [Rust-U64] |
| `str::from_utf8` | Validate textual byte boundaries | Do not manufacture `&str` from unchecked SQL/disk bytes | [Rust-UTF8] |
| `Vec::new`, `with_capacity`, `try_reserve`, `spare_capacity_mut`, `set_len` | Bounded owned scratch and decode output | Enforce budgets; `set_len` only covers initialized values; no PostgreSQL allocation adoption | [Rust-Vec] |
| `BinaryHeap::push`, `peek`, `pop` | Bounded top-k state | Define finite score ordering and tie rules; do not mutate ordering keys through interior state | [Rust-Heap] |
| `f64::ln`, `is_finite`, `total_cmp` | Explicit BM25 and total ordering | Reject invalid parameters; define finite arithmetic and deterministic tie handling | [Rust-F64] |
| `slice::from_raw_parts` | Narrow FFI or validated byte views | One allocation, valid extent/alignment, non-null requirements, correct lifetime/aliasing; pin alone is insufficient | [Rust-Slice] |
| `ptr::read_unaligned` | Selected unaligned scalar loads | Alignment relaxation does not waive validity, bounds, initialization, or ownership rules | [Rust-ReadUnaligned] |
| `ptr::copy_nonoverlapping` | Proven disjoint byte copies | Validate both ranges and non-overlap; obey the element alignment and count contract | [Rust-Copy] |
| `MaybeUninit::uninit`, `write`, `assume_init` | Avoid unnecessary initialization where measured | Track initialized prefix/state precisely; no read/drop of uninitialized values | [Rust-MaybeUninit] |
| `NonNull::new`, `as_ptr` | Typed wrappers around host-owned pointers | Non-null is not proof of validity, lifetime, alignment, or ownership | [Rust-NonNull] |
| `UnsafeCell` | Carefully justified interior mutability | Does not legalize data races or arbitrary aliasing; usually unnecessary in pure engine code | [Rust-UnsafeCell] |
| `atomic::Ordering` | Documented publication and coordination | Select ordering from a proven protocol; Rust private atomics do not synchronize PostgreSQL pages by magic | [Rust-Ordering] |
| `OnceLock::get_or_init` | Backend-local immutable CPU dispatch/config initialization | No cross-process sharing of Rust object internals; initialization must not recurse | [Rust-OnceLock] |
| `alloc::Layout` | Rare explicit aligned allocation wrappers | Check layout construction/size arithmetic and pair allocation with the same allocator contract | [Rust-AllocLayout] |
| `is_x86_feature_detected!` | Runtime dispatch | Check every required ISA feature; keep unsupported instructions unreachable | [Rust-Detect] |
| `#[target_feature]` and `std::arch` | Isolated optimized kernels | Preserve a portable entry point and exact caller feature proof | [Rust-TargetFeature] [Rust-Arch] |
| `_mm256_loadu_si256`, `_mm256_and_si256` | Optional AVX2 bitmap kernels | Unaligned does not mean out-of-bounds; validate full-width reads and handle tails | [Rust-AVX2-Load] [Rust-AVX2-And] |
| `_mm512_popcnt_epi64` | Optional AVX-512 population counts | Require the actual documented feature, including `avx512vpopcntdq`; benchmark end-to-end | [Rust-AVX512-Popcount] |
| `vcntq_u8` | Optional AArch64 byte population counts | Correct widening/reduction and target contract; scalar-equivalent output | [Rust-NEON-Popcount] |
| `catch_unwind`, FFI unwind rules | Rust panic boundaries | Not a generic PostgreSQL `longjmp` catcher; use audited pgrx/C error boundaries | [Rust-CatchUnwind] [Rust-FFI] |
| `Pin` module and reference layout rules | Reasoning about address stability and ABI | Rust `Pin` is not a PostgreSQL buffer pin; `repr(C)` is not an on-disk format | [Rust-Pin] [Rust-Layout] |
| Cargo workspaces/features/lockfiles/profiles | Reproducible build configuration | One compatible PG feature set, locked dependencies, measured release options | [Cargo-Workspace] [Cargo-Features] [Cargo-Lock] [Cargo-Profiles] |

`std::simd` is not the baseline: the researched standard-library documentation still marks the portable SIMD API experimental/nightly. Re-evaluate at implementation time, but do not quietly require a nightly compiler to obtain an unmeasured benefit. [Rust-SIMD]

### 26.4 PostgreSQL API register: host-runtime boundaries

The selected PostgreSQL headers and pgrx-generated bindings decide exact signatures, enum values, macro availability, and layouts. A small reviewed C shim is preferable to inventing a Rust declaration for a macro/inline helper whose semantics are unclear.

| Interface / symbol family | Pin responsibility | Required evidence |
|---|---|---|
| `IndexAmRoutine`, all declared capabilities | Match advertised behavior and actual callbacks; no speculative flags | [PG-AM-API] [PG-AMAPI-Source] |
| `ambuild`, `aminsert`, `aminsertcleanup`, `ambulkdelete`, `amvacuumcleanup` | Build/insertion/VACUUM lifecycle and multi-round state | [PG-AM-Functions] |
| `ambeginscan`, `amrescan`, `amgettuple`, `amgetbitmap`, `amendscan` | Exact scan/recheck/resource contract and supported directions | [PG-AM-Functions] [PG-AM-Scan] [PG-AM-Lock] |
| `table_index_build_scan` family | Delegate heap/HOT/expression/partial-index build semantics appropriately | [PG-TableAM-Source] [PG-HeapHandler-Source] |
| `table_index_fetch_begin`, `table_index_fetch_tuple`, `table_index_fetch_end` | Snapshot-visible heap fetch; preserve original root TID before mutable fetch calls | [PG-TableAM-Source] [PG-HeapHandler-Source] |
| `tbm_add_tuples`, `tbm_add_page`, bitmap iterator APIs | Respect caller-owned bitmap, recheck flags, lossy pages, and private representation | [PG-TIDBitmap-Source] |
| `ItemPointer` accessors and heap layout constants | Checked physical identity and offset capacity | [PG-ItemPointer-Source] [PG-HeapTuple-Source] |
| Buffer read/extend/release and `LockBuffer`/cleanup-lock family | Correct pins, lock modes, byte lifetimes, extension and cleanup ordering | [PG-Buffer-API] [PG-Buffer-Source] |
| Page initialization/accessors | Correct page header, special space and payload boundaries | [PG-Page-API] [PG-Page] |
| `visibilitymap_get_status` and related VM access | Core visibility-map protocol; batching after safe acquisition | [PG-VM-Source] [PG-IndexOnly-Source] |
| `GenericXLogStart`, `RegisterBuffer`, `Finish`, `Abort` | Registered copy mutation, bounded records, locks and recovery reachability | [PG-Generic-WAL] [PG-Generic-Header] [PG-Generic-Source] |
| `ResourceOwner` registration/release | Backend/subtransaction/error-safe resource ownership | [PG-ResourceOwner-Source] |
| Memory context and PostgreSQL allocation APIs | Correct allocator pairing, reset lifetime, accounting and cleanup | [PG-Memory-Source] [PG-C-Functions] |
| Snapshot registration/access through executor/table AM | Use host-owned MVCC semantics; no homemade xmin/xmax visibility | [PG-Snapshot-Source] [PG-MVCC] |
| Custom path/plan/execution methods and planner hooks | Legal alternative plans; serialized plan data; runtime state in executor lifetime | [PG-Custom-Path] [PG-Custom-Plan] [PG-Custom-Execution] [PG-Planner-Source] |
| Predicate-lock/SSI integration | Preserve serializable behavior, including empty scans | [PG-AM-Lock] [PG-SSI-Source] [PG-IndexAM-Source] |
| LWLocks, shared memory, DSM and worker lifecycle | Process-safe coordination and offset-based shared structures | [PG-LWLock-Source] [PG-DSM-Source] [PG-BGWorker] |
| PG18 read-stream APIs | Optional I/O scheduling with supported callback and buffer lifetime | [PG-ReadStream-Source] |
| Preload, extension SQL and upgrade machinery | Explicit initialization and install/upgrade contract | [PG-Preload] [PG-Extensions] [PG-CreateExtension] |
| pgrx guards and raw generated bindings | Verified host/error/ABI boundary; matching tool versions | [Pgrx-Guard] [Pgrx-PgSys] [Pgrx-README] |

### 26.5 Code standard: measurable discipline, not unsafe maximalism

Use safe Rust by default, explicit types for identities and lifecycle states, checked parsing, and small fallible APIs with resource effects documented. Keep indexing/ranking algorithms independent of PostgreSQL so they can be fuzzed and compared against oracles. Centralize unsafe and host calls in reviewed modules; do not let raw pointers leak into the search engine for convenience.

A performance patch must explain what work is removed: allocation, copying, branch misprediction, decoded bytes, heap fetches, lock duration, or I/O. “Uses unsafe” and “uses SIMD” are not performance evidence. Avoid premature abstraction, but also avoid duplicating subtly different decoders or snapshot logic across paths.

Every `SAFETY` comment must identify the proof available at that exact call site. Every storage transition must map to its durable state machine. Every public fast path must name its eligibility checks and correct fallback. These standards are the concrete meaning of production-quality low-level Rust in Pin.

## 27. Design review: rejected shortcuts and open decisions

### 27.1 Decisions that survived the review

| Choice | Decision and reason |
|---|---|
| PostgreSQL-native AM versus separate embedded storage | Own AM and native relation pages: reduce duplicated durability/cache/lifecycle machinery |
| Physical TID addressing | Adopt with root/visible/incarnation/generation distinctions, not naked `ctid` identity |
| Page-group postings | Adopt adaptively; do not impose dense bitmaps on sparse terms |
| Mutable tier | Searchable and bounded; no commit-only or worker-only visibility barrier |
| Compressed immutable segments | Adopt with safe source handoff, copying compaction first, and bounded debt |
| Global live bitmap as MVCC | Reject: maintenance liveness is not snapshot visibility |
| Stored DF as exact SQL count | Reject: scoring statistics and SQL cardinality are different contracts |
| Heap-free direct counts | Gated: certified sources, VM protocol, liveness/reuse ordering, exact predicate proof |
| Top-k oversampling | Reject as an exactness strategy; only eligible rows set pruning thresholds |
| Original-text index-only return | Not advertised from an inverted representation that cannot reconstruct text |
| CustomScan for every query | Reject: offer only legally equivalent specialized paths and keep ordinary execution |
| Generic WAL | Baseline for durability; not a proof of safe standby-reader reclamation |
| Custom WAL | Only after measured need or a justified operational/correctness requirement |
| Copy-avoiding merge | Optional after ownership/recovery/deletion proofs; copying merge remains the reference |
| Mandatory `no_std` or pervasive unsafe | Reject: use `std` where useful and unsafe only at audited boundaries |
| Threads calling PostgreSQL | Reject: use PostgreSQL process/worker contracts; pure off-thread work requires a separate design |
| Universal zero-bug/performance guarantee | Reject as unsupported; enforce testable release gates and transparent evidence |

### 27.2 Blocking design questions before format stabilization

**Owner directory and pin granularity.** The proposed document-owner anchor must satisfy plain index-scan cleanup rules without becoming a single hot lock or a large random-read tax. Prototype the exact pin transfer, VACUUM wait, and exception cleanup. Measure common-term scans and concurrent updates. If the proposal fails, redesign it before claiming the synchronous scan path is ready.

**Liveness across retained generations.** Define how one VACUUM callback-approved deletion reaches every reader-reachable posting incarnation, how compaction reconciles concurrent deletion, and how crashes preserve the ordering before heap-slot reuse. A generation reference protects storage lifetime; it does not freeze liveness or grant snapshot visibility.

**Direct-count certification.** Specify which sealed sources can be counted with VM facts, precisely when owner/liveness state is read, what is pinned or locked across the decision, and why concurrent insertion/deletion/reuse cannot introduce a false positive. Until then, heap-check. Do not turn a plausible diagram into an enabled count path.

**Standby replay and generations.** Primary reader registrations cannot protect standby readers. Choose a real conflict/retention protocol or keep recovery-time Pin scans unsupported. Include the decision in format/WAL design before promising read-replica offload.

**Mutable fanout and throughput.** Multiple writer shards reduce contention but increase per-query source work. Select thresholds, shared metadata budgets, and backpressure from mixed-workload measurements. Avoid a design that performs well only while the index is freshly built and idle.

**Statistics freshness and authorization.** Physical-corpus epochs are the initial ranking contract. Verify statement-consistent acquisition across scalar and custom plans, bounded refresh cost, and permissions around corpus-wide information. A later snapshot-exact mode would be a separate expensive semantic feature.

**Exact host compatibility.** G0 must resolve the matching Rust/pgrx/PostgreSQL set and validate every `IndexAmRoutine` field and error boundary. Researching current documentation does not establish binary compatibility by itself.

### 27.3 How the design was tightened during review

The first tempting design is “physical TID plus bitmaps plus SIMD.” That is insufficient because it says nothing about HOT roots, stale postings, or visibility. The revised design separates root identity, visible tuple identity, and document incarnation.

The next tempting design is “keep old segments until readers finish.” That protects bytes but can still preserve stale terms across tuple-slot reuse. The revised design makes VACUUM liveness cover reader-reachable generations and requires deletion reconciliation during compaction.

The next temptation is “a bitmap count and a top-k heap are enough.” The revised design distinguishes VM-certified exact counts from scoring estimates, and allows only snapshot-visible, final-filter-eligible rows to raise the ranking threshold.

Finally, native WAL storage can appear to settle replication automatically. It settles a significant part of byte recovery, not every custom reader/reclamation protocol. The revised support matrix separates physical recovery/promotion from concurrent standby Pin search. These revisions deliberately trade superficial feature completeness for a foundation that can be validated and optimized.

## 28. Release definition and technical-lead priorities

### 28.1 What 1.0 means

A Pin 1.0 release has a published support matrix; fixed query/analyzer/scoring contracts; a versioned recoverable format; correct ordinary PostgreSQL execution; sustainable maintenance; bounded memory; audited unsafe boundaries; reproducible packages; usable diagnostics; and passing security, isolation, crash, backup/restore, and lifecycle tests for every advertised feature.

No known wrong-result, data-loss, memory-safety, or policy-bypass defect is acceptable. Experimental fast paths remain off unless their proof and test gates pass. Hot-standby search, parallelism, and shared-payload merges may be omitted from 1.0 rather than advertised prematurely. A Tin-parity statement is separate from the release number and requires comparable measured evidence.

### 28.2 First priorities for the technical lead

First, settle the host boundary and the identity/publication/reclamation invariants with executable prototypes. Second, build the smallest durable ordinary-scan slice and make it survive updates, VACUUM, cancellation, and restart. Third, add bounded immutable segments and maintenance without changing match semantics. Fourth, add exact ranked execution and validate every threshold against the exhaustive oracle. Only then promote VM counts, SIMD, parallelism, and advanced merges through measured gates.

Prioritize reducing bytes touched, heap fetches, materialized candidates, lock duration, and unnecessary positional work. Use low-level Rust when it improves a demonstrated bottleneck within a clear safety contract. Do not exchange correctness for a benchmark that hides the cost elsewhere.

### 28.3 Research completion versus implementation completion

This document delivers a researched architecture, component/file responsibility map, source policy, API register, implementation sequence, and acceptance criteria. It does **not** deliver an extension binary, compiled callback implementation, proven concurrency protocol, executed test suite, or benchmark result. The selected source review informs the design; implementation must close the explicitly marked proof obligations and pin its dependencies before performance or production-readiness claims are made.

The product objective is straightforward: **do less work per correct query, keep the work bounded under concurrency, and let PostgreSQL remain the authority for transactions and durability.**

## 29. Primary-source register

All external technical references below are primary documentation, upstream source, or the relevant project's own publication. Research access date: **17 September 2026**. PostgreSQL manual references target version 18. Rust/pgrx and upstream branch links are research references, not a release lockfile; preserve immutable versions/commits in the implementation ledger.

The most important reading sequence is the PostgreSQL Internals map, all six Index AM sections, heap/HOT/VM and buffer contracts, WAL and VACUUM, executor/planner integration, and then the Rust ownership/unsafe/CPU APIs used at each boundary. Vendor benchmark material informs experiments; it does not replace PostgreSQL's contracts.

### 29.1 PostgreSQL 18 official manuals

| Reference | Purpose |
|---|---|
| [PG-Internals] | Internals map and extension-relevant subsystem chapters. |
| [PG-AM] | Chapter 63: Index Access Method Interface Definition. |
| [PG-AM-API] | 63.1: basic AM structure and capability fields. |
| [PG-AM-Functions] | 63.2: index AM support function contracts. |
| [PG-AM-Scan] | 63.3: index scanning and result/recheck behavior. |
| [PG-AM-Lock] | 63.4: index locking and scan/VACUUM interaction. |
| [PG-AM-Unique] | 63.5: uniqueness checking; read even though Pin is nonunique. |
| [PG-AM-Cost] | 63.6: index cost-estimation contract. |
| [PG-Overview] | Server query-processing architecture. |
| [PG-Page] | Physical page headers, item identifiers, and storage layout. |
| [PG-Files] | Relation files and forks. |
| [PG-HOT] | Heap-only tuple updates and their index implications. |
| [PG-VM] | Visibility-map bits and meaning. |
| [PG-TOAST] | Out-of-line/compressed variable-length data. |
| [PG-MVCC] | Concurrency control and snapshots. |
| [PG-Vacuum] | VACUUM operational behavior and maintenance. |
| [PG-WAL-Extensions] | WAL facilities available to extensions. |
| [PG-Generic-WAL] | Generic WAL construction, buffer rules, and limitations. |
| [PG-Custom-Rmgr] | Custom WAL resource managers and deployment obligations. |
| [PG-WAL-Internals] | WAL internals and durability context. |
| [PG-GIN] | Built-in inverted-index design and implementation context. |
| [PG-FTS-Indexes] | Built-in full-text index choices and rechecks. |
| [PG-FTS-Limits] | Native PostgreSQL text-search representation limits. |
| [PG-CustomScan] | Custom scan provider overview. |
| [PG-Custom-Path] | Custom path planning interface. |
| [PG-Custom-Plan] | Custom plan creation and representation. |
| [PG-Custom-Execution] | Custom executor, rescan, shutdown, and parallel interfaces. |
| [PG-C-Functions] | C-language extension values, allocation, and calling conventions. |
| [PG-Parallel-Safety] | Parallel-safety classifications and restrictions. |
| [PG-BGWorker] | PostgreSQL background-worker lifecycle. |
| [PG-RLS] | Row-level security semantics. |
| [PG-CreateIndex] | Index creation and concurrent-build behavior. |
| [PG-Extensions] | Extension packaging, control files, and update scripts. |
| [PG-PGXS] | Extension build/install infrastructure. |
| [PG-CreateExtension] | Installation privileges and extension security. |
| [PG-Preload] | Client/runtime settings, including shared library preloading. |
| [PG-Stats] | Server statistics and monitoring facilities. |
| [PG-Populate] | Bulk-loading and index-build operational context. |
| [PG-Tests] | PostgreSQL regression-test infrastructure. |
| [PG-Bench] | Official benchmark driver and custom scripts. |
| [PG-Backup] | Backup and restore approaches. |
| [PG-PITR] | WAL archiving and point-in-time recovery. |
| [PG-Standby] | Physical standby replication and setup. |
| [PG-Hot-Standby] | Read-only recovery, cleanup conflicts, and cancellation. |
| [PG-Logical] | Logical replication architecture. |
| [PG-Logical-Limits] | Logical replication limitations and schema obligations. |
| [PG-Upgrade] | Major-version upgrade tooling and requirements. |

### 29.2 PostgreSQL upstream source and headers

| Reference | Purpose |
|---|---|
| [PG-AMAPI-Source] | Exact IndexAmRoutine layout and callback typedefs. |
| [PG-TableAM-Source] | Table AM build and tuple-fetch wrappers; mutable TID contract. |
| [PG-HeapHandler-Source] | Heap table-AM implementation of index build/fetch. |
| [PG-HOT-Source] | Detailed HOT/index-root behavior and pruning design. |
| [PG-HeapTuple-Source] | Heap tuple layout and capacity-related constants. |
| [PG-ItemPointer-Source] | Physical tuple identifier representation and accessors. |
| [PG-Buffer-Source] | Buffer pins, content locks, and buffer manager design. |
| [PG-Buffer-API] | Buffer read/extend/pin/lock/release APIs. |
| [PG-Page-API] | Page layout/accessor and initialization definitions. |
| [PG-TIDBitmap-Source] | TIDBitmap insertion, recheck, lossy-page, and iterator contracts. |
| [PG-VM-Source] | Visibility-map access and concurrency implementation. |
| [PG-IndexOnly-Source] | Core VM/heap fallback and visibility ordering commentary. |
| [PG-Vacuum-Source] | Heap VACUUM and index cleanup coordination. |
| [PG-Generic-Header] | Generic WAL declarations and compiled registration capacity. |
| [PG-Generic-Source] | Generic WAL page-image/delta construction and redo. |
| [PG-ResourceOwner-Source] | Resource ownership and release registration APIs. |
| [PG-Memory-Source] | Memory context declarations and lifecycle. |
| [PG-Snapshot-Source] | Snapshot management declarations. |
| [PG-ReadStream-Source] | PostgreSQL 18 read-stream interfaces. |
| [PG-Planner-Source] | Planner and upper-path hook definitions. |
| [PG-IndexAM-Source] | Core index access wrappers and scan lifecycle. |
| [PG-SSI-Source] | Serializable Snapshot Isolation implementation design. |
| [PG-LWLock-Source] | Lightweight-lock declarations. |
| [PG-DSM-Source] | Dynamic shared-memory declarations and lifecycle. |
| [PG-Btree-Page-Source] | B-tree page reuse and standby conflict horizon example. |
| [PG-Btree-WAL-Source] | B-tree redo and recovery conflict handling example. |

### 29.3 Official Rust and Cargo documentation

| Reference | Purpose |
|---|---|
| [Rust-Release] | Official Rust 1.98.1 release announcement; candidate, not tested Pin support. |
| [Rust-FFI] | FFI ownership and unwinding guidance. |
| [Rust-UB] | Undefined behavior and unsafe obligations. |
| [Rust-Layout] | Type layout and representation rules. |
| [Rust-TargetFeature] | Code-generation attributes and target-feature requirements. |
| [Rust-Slice] | Raw-slice validity, extent, lifetime, and aliasing contract. |
| [Rust-ReadUnaligned] | Unaligned-read safety contract. |
| [Rust-Copy] | Non-overlapping copy safety contract. |
| [Rust-MaybeUninit] | Initialization and assume-init rules. |
| [Rust-Vec] | Owned buffers, capacity, reserve, spare capacity, and set_len. |
| [Rust-NonNull] | Non-null pointer wrapper and its limitations. |
| [Rust-UnsafeCell] | Interior mutability, aliasing, and data-race restrictions. |
| [Rust-Ordering] | Atomic memory orderings. |
| [Rust-OnceLock] | One-time process-local initialization. |
| [Rust-Heap] | Heap ordering and mutation requirements. |
| [Rust-AllocLayout] | Checked allocation layout construction. |
| [Rust-U32] | Fixed-width field conversion and checked arithmetic. |
| [Rust-U64] | Bitmap bit operations and checked counters. |
| [Rust-F64] | Finite checks, logarithms, and total floating-point ordering. |
| [Rust-UTF8] | UTF-8 validation before constructing text views. |
| [Rust-CatchUnwind] | Rust panic-catching scope and limitations. |
| [Rust-Pin] | Address-stability pinning, distinct from database buffer pins. |
| [Rust-Arch] | Architecture-specific intrinsics and dispatch context. |
| [Rust-Detect] | Runtime x86 CPU feature detection. |
| [Rust-SIMD] | Portable SIMD experimental/nightly status at research time. |
| [Rust-AVX2-And] | AVX2 bitwise AND intrinsic. |
| [Rust-AVX2-Load] | Unaligned AVX2 load intrinsic and feature requirement. |
| [Rust-AVX512-Popcount] | AVX-512 vector population count and exact feature requirement. |
| [Rust-NEON-Popcount] | AArch64 byte population count intrinsic. |
| [Rust-PGO] | Compiler profile-guided optimization workflow. |
| [Cargo-Workspace] | Workspace configuration and dependency organization. |
| [Cargo-Features] | Feature resolution and compatibility implications. |
| [Cargo-Lock] | Manifest and lockfile responsibilities. |
| [Cargo-Profiles] | Optimization, debug, overflow, LTO, and panic configuration. |

### 29.4 Upstream tooling, text standards, and search references

| Reference | Purpose |
|---|---|
| [Pgrx] | pgrx published package documentation; freeze a matching version set in G0. |
| [Pgrx-Guard] | pgrx PostgreSQL/Rust guard attribute contract. |
| [Pgrx-PgSys] | Generated PostgreSQL bindings and raw API surface. |
| [Pgrx-README] | Upstream setup, support, tooling, and lifecycle guidance. |
| [Pgrx-Manifest] | Upstream version/dependency/toolchain metadata; not a release pin. |
| [Rust-Miri] | Miri upstream project documentation and limitations. |
| [Rust-Fuzz] | Rust Fuzz project documentation for cargo-fuzz. |
| [Unicode-Segmentation] | Unicode text segmentation specification. |
| [Unicode-Normalization] | Unicode normalization specification. |
| [Unicode-ICU] | ICU boundary analysis and implementation context. |
| [Lucene-BM25] | Lucene BM25 API and parameter documentation. |
| [Lucene-BM25-Source] | Upstream BM25 implementation and normalization choices. |
| [Lucene-WAND] | Upstream bounded ranked-retrieval implementation and conservative rounding context. |

### 29.5 PlanetScale primary publications

| Reference | Purpose |
|---|---|
| [Tin-Blog] | PlanetScale introducing-TIN article, published 16 September 2026; vendor design/results. |
| [Tin-Bench] | PlanetScale-published benchmark repository; experimental input, not independent validation. |
| [Tin-Docs] | PlanetScale search product documentation. |
| [Tin-Start] | PlanetScale search getting-started documentation. |

### 29.6 Licensing and contribution provenance

| Reference | Purpose |
|---|---|
| [License-PG] | PostgreSQL license text. |
| [License-Apache] | Apache License 2.0 official text. |
| [License-MIT] | MIT license text published by the Open Source Initiative. |
| [DCO-Official] | Developer Certificate of Origin text. |

---

**End of blueprint.** All source-linked design proposals remain subject to the implementation and release gates above.

<!-- Reference definitions: retain these when copying this document. -->

[PG-Internals]: https://www.postgresql.org/docs/18/internals.html
[PG-AM]: https://www.postgresql.org/docs/18/indexam.html
[PG-AM-API]: https://www.postgresql.org/docs/18/index-api.html
[PG-AM-Functions]: https://www.postgresql.org/docs/18/index-functions.html
[PG-AM-Scan]: https://www.postgresql.org/docs/18/index-scanning.html
[PG-AM-Lock]: https://www.postgresql.org/docs/18/index-locking.html
[PG-AM-Unique]: https://www.postgresql.org/docs/18/index-unique-checks.html
[PG-AM-Cost]: https://www.postgresql.org/docs/18/index-cost-estimation.html
[PG-Overview]: https://www.postgresql.org/docs/18/overview.html
[PG-Page]: https://www.postgresql.org/docs/18/storage-page-layout.html
[PG-Files]: https://www.postgresql.org/docs/18/storage-file-layout.html
[PG-HOT]: https://www.postgresql.org/docs/18/storage-hot.html
[PG-VM]: https://www.postgresql.org/docs/18/storage-vm.html
[PG-TOAST]: https://www.postgresql.org/docs/18/storage-toast.html
[PG-MVCC]: https://www.postgresql.org/docs/18/mvcc.html
[PG-Vacuum]: https://www.postgresql.org/docs/18/routine-vacuuming.html
[PG-WAL-Extensions]: https://www.postgresql.org/docs/18/wal-for-extensions.html
[PG-Generic-WAL]: https://www.postgresql.org/docs/18/generic-wal.html
[PG-Custom-Rmgr]: https://www.postgresql.org/docs/18/custom-rmgr.html
[PG-WAL-Internals]: https://www.postgresql.org/docs/18/wal-internals.html
[PG-GIN]: https://www.postgresql.org/docs/18/gin.html
[PG-FTS-Indexes]: https://www.postgresql.org/docs/18/textsearch-indexes.html
[PG-FTS-Limits]: https://www.postgresql.org/docs/18/textsearch-limitations.html
[PG-CustomScan]: https://www.postgresql.org/docs/18/custom-scan.html
[PG-Custom-Path]: https://www.postgresql.org/docs/18/custom-scan-path.html
[PG-Custom-Plan]: https://www.postgresql.org/docs/18/custom-scan-plan.html
[PG-Custom-Execution]: https://www.postgresql.org/docs/18/custom-scan-execution.html
[PG-C-Functions]: https://www.postgresql.org/docs/18/xfunc-c.html
[PG-Parallel-Safety]: https://www.postgresql.org/docs/18/parallel-safety.html
[PG-BGWorker]: https://www.postgresql.org/docs/18/bgworker.html
[PG-RLS]: https://www.postgresql.org/docs/18/ddl-rowsecurity.html
[PG-CreateIndex]: https://www.postgresql.org/docs/18/sql-createindex.html
[PG-Extensions]: https://www.postgresql.org/docs/18/extend-extensions.html
[PG-PGXS]: https://www.postgresql.org/docs/18/extend-pgxs.html
[PG-CreateExtension]: https://www.postgresql.org/docs/18/sql-createextension.html
[PG-Preload]: https://www.postgresql.org/docs/18/runtime-config-client.html
[PG-Stats]: https://www.postgresql.org/docs/18/monitoring-stats.html
[PG-Populate]: https://www.postgresql.org/docs/18/populate.html
[PG-Tests]: https://www.postgresql.org/docs/18/regress.html
[PG-Bench]: https://www.postgresql.org/docs/18/pgbench.html
[PG-Backup]: https://www.postgresql.org/docs/18/backup.html
[PG-PITR]: https://www.postgresql.org/docs/18/continuous-archiving.html
[PG-Standby]: https://www.postgresql.org/docs/18/warm-standby.html
[PG-Hot-Standby]: https://www.postgresql.org/docs/18/hot-standby.html
[PG-Logical]: https://www.postgresql.org/docs/18/logical-replication-architecture.html
[PG-Logical-Limits]: https://www.postgresql.org/docs/18/logical-replication-restrictions.html
[PG-Upgrade]: https://www.postgresql.org/docs/18/pgupgrade.html
[PG-AMAPI-Source]: https://raw.githubusercontent.com/postgres/postgres/REL_18_STABLE/src/include/access/amapi.h
[PG-TableAM-Source]: https://raw.githubusercontent.com/postgres/postgres/REL_18_STABLE/src/include/access/tableam.h
[PG-HeapHandler-Source]: https://raw.githubusercontent.com/postgres/postgres/REL_18_STABLE/src/backend/access/heap/heapam_handler.c
[PG-HOT-Source]: https://raw.githubusercontent.com/postgres/postgres/REL_18_STABLE/src/backend/access/heap/README.HOT
[PG-HeapTuple-Source]: https://raw.githubusercontent.com/postgres/postgres/REL_18_STABLE/src/include/access/htup_details.h
[PG-ItemPointer-Source]: https://raw.githubusercontent.com/postgres/postgres/REL_18_STABLE/src/include/storage/itemptr.h
[PG-Buffer-Source]: https://raw.githubusercontent.com/postgres/postgres/REL_18_STABLE/src/backend/storage/buffer/README
[PG-Buffer-API]: https://raw.githubusercontent.com/postgres/postgres/REL_18_STABLE/src/include/storage/bufmgr.h
[PG-Page-API]: https://raw.githubusercontent.com/postgres/postgres/REL_18_STABLE/src/include/storage/bufpage.h
[PG-TIDBitmap-Source]: https://raw.githubusercontent.com/postgres/postgres/REL_18_STABLE/src/include/nodes/tidbitmap.h
[PG-VM-Source]: https://raw.githubusercontent.com/postgres/postgres/REL_18_STABLE/src/backend/access/heap/visibilitymap.c
[PG-IndexOnly-Source]: https://raw.githubusercontent.com/postgres/postgres/REL_18_STABLE/src/backend/executor/nodeIndexonlyscan.c
[PG-Vacuum-Source]: https://raw.githubusercontent.com/postgres/postgres/REL_18_STABLE/src/backend/access/heap/vacuumlazy.c
[PG-Generic-Header]: https://raw.githubusercontent.com/postgres/postgres/REL_18_STABLE/src/include/access/generic_xlog.h
[PG-Generic-Source]: https://raw.githubusercontent.com/postgres/postgres/REL_18_STABLE/src/backend/access/transam/generic_xlog.c
[PG-ResourceOwner-Source]: https://raw.githubusercontent.com/postgres/postgres/REL_18_STABLE/src/include/utils/resowner.h
[PG-Memory-Source]: https://raw.githubusercontent.com/postgres/postgres/REL_18_STABLE/src/include/utils/memutils.h
[PG-Snapshot-Source]: https://raw.githubusercontent.com/postgres/postgres/REL_18_STABLE/src/include/utils/snapmgr.h
[PG-ReadStream-Source]: https://raw.githubusercontent.com/postgres/postgres/REL_18_STABLE/src/include/storage/read_stream.h
[PG-Planner-Source]: https://raw.githubusercontent.com/postgres/postgres/REL_18_STABLE/src/include/optimizer/planner.h
[PG-IndexAM-Source]: https://raw.githubusercontent.com/postgres/postgres/REL_18_STABLE/src/backend/access/index/indexam.c
[PG-SSI-Source]: https://raw.githubusercontent.com/postgres/postgres/REL_18_STABLE/src/backend/storage/lmgr/README-SSI
[PG-LWLock-Source]: https://raw.githubusercontent.com/postgres/postgres/REL_18_STABLE/src/include/storage/lwlock.h
[PG-DSM-Source]: https://raw.githubusercontent.com/postgres/postgres/REL_18_STABLE/src/include/storage/dsm.h
[PG-Btree-Page-Source]: https://raw.githubusercontent.com/postgres/postgres/REL_18_STABLE/src/backend/access/nbtree/nbtpage.c
[PG-Btree-WAL-Source]: https://raw.githubusercontent.com/postgres/postgres/REL_18_STABLE/src/backend/access/nbtree/nbtxlog.c
[Rust-Release]: https://blog.rust-lang.org/2026/09/03/Rust-1.98.1/
[Rust-FFI]: https://doc.rust-lang.org/nomicon/ffi.html
[Rust-UB]: https://doc.rust-lang.org/reference/behavior-considered-undefined.html
[Rust-Layout]: https://doc.rust-lang.org/reference/type-layout.html
[Rust-TargetFeature]: https://doc.rust-lang.org/reference/attributes/codegen.html
[Rust-Slice]: https://doc.rust-lang.org/std/slice/fn.from_raw_parts.html
[Rust-ReadUnaligned]: https://doc.rust-lang.org/std/ptr/fn.read_unaligned.html
[Rust-Copy]: https://doc.rust-lang.org/std/ptr/fn.copy_nonoverlapping.html
[Rust-MaybeUninit]: https://doc.rust-lang.org/std/mem/union.MaybeUninit.html
[Rust-Vec]: https://doc.rust-lang.org/std/vec/struct.Vec.html
[Rust-NonNull]: https://doc.rust-lang.org/std/ptr/struct.NonNull.html
[Rust-UnsafeCell]: https://doc.rust-lang.org/std/cell/struct.UnsafeCell.html
[Rust-Ordering]: https://doc.rust-lang.org/std/sync/atomic/enum.Ordering.html
[Rust-OnceLock]: https://doc.rust-lang.org/std/sync/struct.OnceLock.html
[Rust-Heap]: https://doc.rust-lang.org/std/collections/struct.BinaryHeap.html
[Rust-AllocLayout]: https://doc.rust-lang.org/std/alloc/struct.Layout.html
[Rust-U32]: https://doc.rust-lang.org/std/primitive.u32.html
[Rust-U64]: https://doc.rust-lang.org/std/primitive.u64.html
[Rust-F64]: https://doc.rust-lang.org/std/primitive.f64.html
[Rust-UTF8]: https://doc.rust-lang.org/std/str/fn.from_utf8.html
[Rust-CatchUnwind]: https://doc.rust-lang.org/std/panic/fn.catch_unwind.html
[Rust-Pin]: https://doc.rust-lang.org/std/pin/index.html
[Rust-Arch]: https://doc.rust-lang.org/std/arch/index.html
[Rust-Detect]: https://doc.rust-lang.org/std/arch/macro.is_x86_feature_detected.html
[Rust-SIMD]: https://doc.rust-lang.org/std/simd/index.html
[Rust-AVX2-And]: https://doc.rust-lang.org/core/arch/x86_64/fn._mm256_and_si256.html
[Rust-AVX2-Load]: https://doc.rust-lang.org/core/arch/x86_64/fn._mm256_loadu_si256.html
[Rust-AVX512-Popcount]: https://doc.rust-lang.org/core/arch/x86_64/fn._mm512_popcnt_epi64.html
[Rust-NEON-Popcount]: https://doc.rust-lang.org/core/arch/aarch64/fn.vcntq_u8.html
[Rust-PGO]: https://doc.rust-lang.org/rustc/profile-guided-optimization.html
[Cargo-Workspace]: https://doc.rust-lang.org/cargo/reference/workspaces.html
[Cargo-Features]: https://doc.rust-lang.org/cargo/reference/features.html
[Cargo-Lock]: https://doc.rust-lang.org/cargo/guide/cargo-toml-vs-cargo-lock.html
[Cargo-Profiles]: https://doc.rust-lang.org/cargo/reference/profiles.html
[Pgrx]: https://docs.rs/pgrx/latest/pgrx/
[Pgrx-Guard]: https://docs.rs/pgrx/latest/pgrx/attr.pg_guard.html
[Pgrx-PgSys]: https://docs.rs/pgrx-pg-sys/latest/pgrx_pg_sys/
[Pgrx-README]: https://raw.githubusercontent.com/pgcentralfoundation/pgrx/develop/README.md
[Pgrx-Manifest]: https://raw.githubusercontent.com/pgcentralfoundation/pgrx/develop/Cargo.toml
[Rust-Miri]: https://github.com/rust-lang/miri
[Rust-Fuzz]: https://rust-fuzz.github.io/book/cargo-fuzz.html
[Unicode-Segmentation]: https://www.unicode.org/reports/tr29/
[Unicode-Normalization]: https://www.unicode.org/reports/tr15/
[Unicode-ICU]: https://unicode-org.github.io/icu/userguide/boundaryanalysis/
[Lucene-BM25]: https://lucene.apache.org/core/9_12_1/core/org/apache/lucene/search/similarities/BM25Similarity.html
[Lucene-BM25-Source]: https://raw.githubusercontent.com/apache/lucene/main/lucene/core/src/java/org/apache/lucene/search/similarities/BM25Similarity.java
[Lucene-WAND]: https://raw.githubusercontent.com/apache/lucene/main/lucene/core/src/java/org/apache/lucene/search/WANDScorer.java
[Tin-Blog]: https://planetscale.com/blog/introducing-tin
[Tin-Bench]: https://github.com/planetscale/paradedb-benchmarker
[Tin-Docs]: https://planetscale.com/docs/postgres/search
[Tin-Start]: https://planetscale.com/docs/postgres/search/get-started
[License-PG]: https://www.postgresql.org/about/licence/
[License-Apache]: https://www.apache.org/licenses/LICENSE-2.0
[License-MIT]: https://opensource.org/license/mit
[DCO-Official]: https://developercertificate.org/

## Implementation follow-up: grouped COUNT consumer, 2026-09-25

An opt-in implementation of the section 13 exact-count equation is now present
in `pin-pg/src/grouped_count.rs` and the retained PinCount CustomScan. The engine
exposes sealed page masks directly; fresh VM probes certify pages, and other
roots use PostgreSQL's HOT/MVCC callback. A nonblocking shared writer-interlock
acquisition, held through the visibility decisions, prevents owner retirement
and TID-reuse mixing. The structural barrier alone is not used as certification.

This is a coarse-lock vertical slice, not completion of the release gates.
`pin.enable_grouped_count` remains default off; native compilation, exact-head
isolation/recovery, independent unsafe/visibility review, and concurrent-writer
latency measurements are unresolved. No new persistent format or migration is
introduced. Bounded durable incremental sealing/merging, write amplification,
score-aware exact top-k and standby-safe search remain separate work.

See [the implementation/alternatives record](docs/g9-grouped-count.md),
[COUNT03 API obligations](docs/api-evidence.md#count03-grouped-exact-count-generation-interlock)
and [observed versus unrun evidence](docs/runs/2026-09-25-grouped-count/README.md).
No 10x or TIN-parity statement is supported by this local implementation alone.
