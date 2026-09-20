# G7 selected implementation and acceptance contract

Status: implementation in progress; not an accepted performance or release gate.
Base: ae69a2e0fc2f2c241b7dfbe5a09f592061115351.

## Selected work

1. Qualify PostgreSQL's native parallel bitmap heap path. One backend produces
   the Pin bitmap; PostgreSQL distributes heap blocks, visibility and rechecks.
   Do not set `amcanparallel` or invent a CustomScan DSM contract for this path.
2. Add optional sealed-prefix retention to the copying compactor. Preserve
   canonical owners, dictionary-local term identity and the current format.
   Keep the last fully live sealed page in the rewrite suffix so repeated small
   appends coalesce instead of retaining one underfilled tail per append.
3. Keep the copying implementation as the default and differential reference.
   New storage behavior requires independent review and recovery qualification.

## Prefix-retention protocol

The host holds the existing exclusive structural barrier before the writer
interlock, through inspection, publication and retirement. No old reader can
retain an affected posting chain. The optimization is not concurrent shared
extent ownership and does not require a reference-count format.

Inspect the complete chain, validating strict owner/incarnation order and
canonical publication/liveness. Only a contiguous, entirely live sealed prefix
may be retained. Rewrite at least its last page with the remaining suffix.
The retained prefix still belongs to the same term and its payload is unchanged.

Prepare replacement suffix pages under the existing Building journal. Publish
one atomic operation covering the metapage, dictionary page and retained
boundary page. Preserve the dictionary head, replace the boundary next link,
update the dictionary tail, and record only the old suffix as Retiring. With no
retained prefix, use the existing whole-chain publication. The host's three-page
commit limit is unchanged.

Before publication, recovery discards unpublished replacement pages and the
old chain remains authoritative. After publication, recovery reclaims only the
old suffix. Retained pages must never appear in the retirement journal or free
list. No heap visibility shortcut or owner/incarnation rewrite is introduced.

## Acceptance evidence required

- Copying and retention produce identical exact owner streams and query results.
- Retained payload bytes and interior links stay unchanged; only the boundary
  link may change. Work counters distinguish retained pages from free-list reuse.
- Dead prefix owners, suffix deletion, slot reuse, repeated small appends,
  dictionary collisions and malformed chains are covered.
- Failure before and after every commit, and at every publication/reclamation
  event, recovers without loss, duplicate coverage or reachable free pages.
- Parallel qualification checks EXPLAIN ANALYZE JSON for actual launched workers
  and the intended bitmap nodes, not just planner settings. Compare serial and
  parallel results, exact/lossy bitmaps and leader participation modes.
- Keep memory bounded independently of document count. Report physical work and
  measured latency separately; no speedup or Tin-parity claim without evidence.

## Deliberately not advertised

Custom parallel count/top-k, parallel index build, parallel maintenance workers,
standby index reads and general shared-payload merges remain gated. Completing
this selected slice does not imply the full G7 or operational G8 exit has passed.

## Official contracts

- PostgreSQL 18 parallel plans: https://www.postgresql.org/docs/18/parallel-plans.html
- PostgreSQL 18 parallel safety: https://www.postgresql.org/docs/18/parallel-safety.html
- PostgreSQL 18 generic WAL: https://www.postgresql.org/docs/18/generic-wal.html
- PostgreSQL source pin: 724edf9bde9d356724ad384a2e196edc3c9f80f7.
- Rust 1.98.1 checked arithmetic: https://doc.rust-lang.org/std/primitive.u32.html#method.checked_add
- Rust 1.98.1 optional state: https://doc.rust-lang.org/std/option/enum.Option.html

Independent storage reviewer: unassigned. Compilation, fault tests, PostgreSQL
qualification and controlled performance measurements are pending.
