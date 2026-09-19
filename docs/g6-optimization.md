# G6: measured low-level optimization

## Status

Work starts from main `5a3c5f04aab0ae9fa67a0ecbc0c10ca210f978e8`.
G6 is not an assertion that the earlier G4/G5 qualification gates passed.
No throughput, latency, memory-parity or production-readiness result is claimed.

## Scope

Reduce avoidable allocation and repeated scalar work in existing pure paths.
Introduce separately audited CPU kernels with a safe, force-scalar interface,
explicit runtime feature checks, and differential tests. Preserve the pure
engine's unsafe prohibition and PostgreSQL's visibility, buffer and WAL rules.

SIMD activation is an experiment, not the default merely because an ISA exists.
Tiny containers can be slower with dispatch overhead. Keep scalar production
entry points until controlled end-to-end results justify a different default.
Do not change group sizes, disk format, durability, source limits or planner
cost constants without workload evidence. PG18 read-stream batching remains an
optional experiment, not a reason to bypass buffer ownership or add prefetch
without a demonstrated I/O bottleneck.

## Checkpoints

1. Establish this branch, the source contracts and reviewable CI diagnostics.
2. Add isolated scalar/AVX2 kernels and bounds, tail and alignment tests.
3. Reduce measured-work opportunities in container sizing and query scratch;
   retain reference paths and add regression tests.
4. Add reproducible kernel/container and PostgreSQL workload measurements.
5. Review CI diagnostics, correct regressions and record observed evidence.

## Required evidence

- Exact scalar agreement for all kernels, including empty inputs, every tail,
  unaligned vector addresses, dense/sparse inputs and unsupported-mode rejection.
- Allocations stay bounded and fallible; no result truncation on budget failure.
- No ISA detection in the per-posting loop and no target-cpu=native distribution.
- Rustfmt, Clippy, rustdoc, debug/release tests and applicable Miri/sanitizers.
- Same SQL rows under the same snapshots and ordinary heap rechecks.
- Controlled end-to-end latency, throughput, memory, WAL, write p99 and maintenance
  debt comparisons before enabling an experimental optimization by default.

The development VM installs no Rust toolchain. CI is the compilation authority.
CI artifacts and diagnostics are review evidence, not automatic source writes.

## Official contracts reviewed before implementation

Rust 1.98.1 documentation identifies compiler commit `48a229cea`.
The implementation ledger must record full immutable source identities before
an unsafe kernel is accepted.

- `OnceLock::get_or_init`: backend-local immutable CPU capability selection;
  initialization must not recurse or call PostgreSQL.
  https://doc.rust-lang.org/std/sync/struct.OnceLock.html#method.get_or_init
- `is_x86_feature_detected!`: check the exact runtime feature, including OS
  support; unsupported instructions must remain unreachable.
  https://doc.rust-lang.org/std/arch/macro.is_x86_feature_detected.html
- `target_feature`: callers must establish the target feature before executing
  target-specific functions; generic callers stay portable.
  https://doc.rust-lang.org/reference/attributes/codegen.html#the-target_feature-attribute
- `_mm256_loadu_si256`: unaligned loads still need the complete 32-byte readable
  range inside one initialized live allocation. Tails use the scalar path.
  https://doc.rust-lang.org/core/arch/x86_64/fn._mm256_loadu_si256.html

## Acceptance

G6 remains open until correctness, host qualification and controlled end-to-end
measurement are observed. Microbenchmark wins alone do not close the gate.
New unsafe code additionally requires independent review; self-review is not
represented as independent approval.
