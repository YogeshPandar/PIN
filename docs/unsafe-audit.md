# G0 unsafe boundary register

Review state: implementation self-review and executable CI validation are
complete on code head `6b4c93938f6fd252ed66ebf0debafcc352769eed`.
G0 boundary run 62 passed every executable validation listed below. Independent
reviewers are not assigned, so none of these entries is independently approved.
See the identically named entries in [api-evidence.md](api-evidence.md) for
pinned upstream contracts and exact local obligations.

| ID | Operations | Safety argument | Required validation |
|---|---|---|---|
| ABI01 | Two non-throwing C probe declarations/calls | Fixed-width scalar ABI; any key accepted; no input pointers or host state | C compile, all 51 offsets, runtime `abi_check` |
| AM01 | `palloc`, pointer cast, `write`, Datum conversion | Zero-argument runtime handler; one fresh aligned context allocation, complete initialization before exposure, no Rust allocator adoption | SQL AM registration and catalog lifecycle |
| AM01 | Installed `extern "C-unwind"` callbacks | Selected signature and pgrx guard; unused opaque inputs never dereferenced; no storage acquired | Rejected index/opclass creation; callback error Drop count |
| COMPAT01 | `_PG_init`, preload flag read | PostgreSQL-controlled single-thread startup or guarded late-load attempt | Startup, restart, repeated rejected late loads |
| COMPAT01 | `GetConfigOption`, `CStr::from_ptr` | Fixed preset name; initialized GUCs; null checked; terminated borrowed bytes consumed before another host call | Exact server version/page-size checks, incompatible build gate |
| COMPAT01 | `GetDatabaseEncoding` | Only guarded entries in a connected backend | UTF-8 success, LATIN1 install rejection |

No raw shared-buffer slices, unchecked decoders, pointer arithmetic, atomics,
SIMD, or manual heap-visibility checks are added. The pure crate and build script
forbid unsafe code. There are no buffer pins, relation handles, generation
registrations, or ResourceOwner resources to release in this slice. Their future
introduction requires a new audit and real ERROR/abort/backend-death testing.

The test-only Drop probe makes no host calls and uses no unsafe code. It tests
stack unwinding, not the safety of a future resource wrapper. G0 boundary run 62
passed the normal lifecycle suite, host-boundary Clippy with warnings denied,
test-hook installation, repeated guarded PostgreSQL/Rust/Pin errors, permission
checks, and exact destructor accounting. Source inventory checks are drift
detection, not type checking or a memory-safety proof. Independent unsafe review
remains a separate acceptance requirement.


## G5 count boundary additions

Review state: implementation self-review completed for the G5 count changes.
Independent unsafe and visibility review is not recorded. Rust/C/PostgreSQL
execution for this head must be observed in CI before changing that status.

| ID | Operations | Safety argument | Required validation |
|---|---|---|---|
| G5COUNT01 | Owner buffer read, retained buffer pin, VM status | C copies under a shared content lock, unlocks content access, and retains one ResourceOwner-backed pin; Rust receives only private initialized bytes | C/Rust compile, cancellation, ERROR and backend-death cleanup |
| G5COUNT01 | \`visibilitymap_get_status\` | Called only with a protected canonical owner and supported MVCC execution; result is reread per candidate and never cached | old-snapshot, VM transition, delete/reuse schedules |
| G5COUNT01 | \`table_index_fetch_tuple\` | Uses a validated root copy, active MVCC snapshot and host-owned slot; HOT may change only the local TID copy | HOT/non-HOT update and old-snapshot equality |
| G5COUNT01 | \`slice::from_raw_parts\` on returned text bytes | C returns an initialized byte range owned by the tuple/scratch context; length is checked and the borrow ends before \`pin_count_clear\` | Rust/C compile, SQL text/TOAST cases, sanitizer-compatible host run |
| G5COUNT01 | \`LockBufferForCleanup\` owner removal | Called by VACUUM while the writer interlock protects the private owner image; cleanup permission drains count-reader pins before WAL publication | deterministic BufferPin wait and cancellation/backend-death schedules |
| G5COUNT01 | Planner path/node pointers and shallow AggPath copy | PostgreSQL planner context owns all allocations; \`add_path\` may free the original path, so Pin copies it before insertion and does not dereference it afterward | planner source tripwire, cached-plan and DDL invalidation tests |

The pure G5 candidate iterator remains safe Rust and does not retain PostgreSQL
pointers. No raw shared-buffer slice enters Rust. The owner pin is represented
only inside the C executor state and is released explicitly on normal paths.
PostgreSQL ResourceOwner cleanup is relied on for C ERROR, cancellation and
backend death, which is why the real host schedules remain an acceptance gate.

The VM shortcut is not accepted merely because these operations are memory-safe.
Its correctness also depends on PostgreSQL publication, snapshot and cleanup
ordering. That proof and the negative controls are documented in
[g5-counts.md](g5-counts.md). Both count switches remain off by default.

## G6 kernel boundary additions

Review state: implementation self-review completed for the G6 AVX2 kernel.
Independent unsafe review is not recorded. The production PostgreSQL paths keep
automatic SIMD disabled until end-to-end measurement and independent review.

| ID | Operations | Safety argument | Required validation |
|---|---|---|---|
| G6KERNEL01 | `#[target_feature(enable = "avx2")]` kernel entry | The private backend can be constructed only after `is_x86_feature_detected!("avx2")`; scalar remains universally available | forced scalar/AVX2 differential tests and unsupported-mode rejection |
| G6KERNEL01 | `_mm256_loadu_si256` | Each load receives one complete initialized four-`u64` chunk; unaligned access does not extend beyond the borrowed slice | every length tail and alignment modulo 32 |
| G6KERNEL01 | `_mm256_storeu_si256` | Output is an exclusive four-`u64` chunk with the same validated length; valid Rust callers cannot overlap the output with immutable inputs | guard-word clobber tests and AddressSanitizer |
| G6KERNEL01 | AVX2 boolean intrinsics | Union, intersection and difference map directly to scalar operations; difference reverses operands for `andnot` semantics | independent per-word oracle over sparse/dense/random inputs |

G6 run `35498740070` passed the kernel debug/release suite, Clippy, rustdoc,
Miri scalar/tail checks and AddressSanitizer native-kernel tests at
`93c5bbeba0d53c064b6f55a9be4b51849493114c`. Those checks cover memory and
scalar-equivalence properties of the isolated kernel. They do not establish
whole-query performance or PostgreSQL visibility correctness.



## G7 parallel boundary additions

Review state: implementation self-review completed. Independent FFI, worker and
storage review is not recorded. Parallel build, direct count and parallel VACUUM
must pass the pinned PostgreSQL 18.6 qualification before their evidence status
is promoted.

| ID | Operations | Safety argument | Required validation |
|---|---|---|---|
| G7SCAN01 | Parallel IndexScan DSM and fixed root output arrays | DSM stores one spinlock, ready flag and eleven integer work words only; Rust receives caller-sized block/offset arrays, bounds every write, retains no raw pointer, and returns before C reuses the arrays | compile/Clippy, source tripwires, real parallel Index Scan, post-claim worker termination, serial equality |
| G7BUILD01 | C parallel table-scan DSM and Rust build callback | DSM stores OIDs/scalars and a PostgreSQL scan descriptor only; worker callback pointers remain live only for the synchronous call; Rust retains none | C/Rust compile, real worker observation, build equality, ERROR cleanup |
| G7BUILD01 | `Datum`, null array and `ItemPointer` forwarded to Rust | PostgreSQL owns all callback storage; the guarded Rust entry uses the same validated insertion path and returns before core can reuse inputs | build with NULL/TOAST/large text, worker termination, transactional suite |
| G7COUNT01 | `slice::from_raw_parts` over DSM query bytes | C allocates and initializes the complete query extent; pointer is non-null and length is bounded by `isize::MAX`; immutable borrow ends after synchronous decode | host compile, malformed query, repeated execution |
| G7COUNT01 | PostgreSQL spinlock over fixed DSM words and counters | No Rust call, I/O, allocation, visibility operation or ERROR-capable work executes while the spinlock is held | competing claims, worker termination, cancellation |
| G7COUNT01 | Worker relation, snapshot, heap fetch and slot state | Worker reopens process-local resources after PostgreSQL restores transaction state; existing G5 visibility safety rules apply independently in each worker | exact count equality, HOT/VM cases, cleanup after worker death |
| G7VACUUM01 | Borrowed `BufferAccessStrategy` and cost-delay call | Strategy lifetime is the callback phase; C retains no pointer; delay runs outside content locks and WAL critical work | parallel/serial VACUUM, cancellation, worker failure |
| G7MERGE01 | Retained on-disk sealed pages | No unsafe Rust added; exclusive reader quiescence prevents concurrent stale chain use; retained pages never enter retirement ownership | copying differential, fault injection, restart/reclaim tests |

The pure `WorkState` and retained-prefix implementations contain no unsafe Rust.
They do not turn PostgreSQL shared memory or shared buffers into Rust references.
Process synchronization remains in the C/PostgreSQL boundary.


## G9 grouped boundary additions

Review state: self-review only. Native PostgreSQL ABI/SQL/recovery validation and
independent FFI/storage review are not recorded for the new adapter. Earlier G0,
G6 and G7 approvals or CI results do not cover these new operations. Both grouped
settings remain off by default. Exact upstream contracts are listed in `G9PG01`
in [api-evidence.md](api-evidence.md).

| ID | Operations | Safety argument | Required validation |
| --- | --- | --- | --- |
| G9PG01 | `pin_group_sort_begin` and opaque `NonNull<c_void>` | Main-thread guarded call with one scalar budget; PostgreSQL owns state and tapes; null means preflight skip without publication | Pinned C/Rust compile, low-memory and autovacuum budget tests |
| G9PG01 | Batched input raw byte pointer | Rust asserts 32-byte alignment-one records and bounds count to 256; C copies input synchronously into tuplesort | Debug/optimized marshalling, batch limits, upstream copying contract |
| G9PG01 | Batched output raw byte pointer | Exclusive initialized Rust extent; C copies each borrowed datum before another sort operation; returned count checked | Unaligned output, canaries, borrowed-buffer invalidation, native spilled-sort tests |
| G9PG01 | `pin_group_sort_finish` | Unique invocation-local handle, single checked state transition, trivial guarded call | Invalid transition tests, cancellation during real sort |
| G9PG01 | Consuming `pin_group_sort_end` | Explicit normal/core-error cleanup, no retained raw datum and no PostgreSQL call in Rust Drop | Normal cleanup and native ERROR/cancellation/ResourceOwner tests |
| G9PG01 | Grouped AM integration | Existing shared/exclusive structural barriers, writer interlock and copied page-store boundary; no new raw shared-buffer slice | Row-identity oracles, snapshot/concurrent schedules, WAL durability and heap reuse |

The C adapters also adopt the pinned PostgreSQL aliasing, overflow and precision
flags. Their previous handwritten flags omitted these compiler-contract settings;
this change requires native regression coverage for all C adapters, not just G9.
The local C harness compiles the actual bridge but substitutes PostgreSQL APIs.
Its debug and optimized UBSan/bounds runs pass; they are not a native ABI proof.
The added PostgreSQL driver is required to observe spill and cleanup, prove actual
selection of the grouped executor, and check crash/recovery boundaries. Those
native runs and an independent reviewer are still missing. The pure grouped core
continues to forbid unsafe code.
