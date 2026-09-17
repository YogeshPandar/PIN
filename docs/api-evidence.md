# API evidence and verification ledger

## Publication graph

The model in `crates/pin-core/tests/publication_model.rs` represents complete
publication, merge-time deletion reconciliation, and reader-reachable retired
liveness. It does not establish PostgreSQL lock, memory-order, WAL, or MVCC safety.
The existing assertion of more than 50 reachable states was unsupported: a
separate Python enumeration found 37 reachable states and 76 non-self transitions.
The test now fixes that graph size, covers every action, checks retained-reader
reuse, and replays both deliberately broken protocols' shortest counterexamples.

Rust 1.98.1 contracts:

- [HashMap entry](https://doc.rust-lang.org/1.98.1/std/collections/struct.HashMap.html#method.entry):
  immutable `Eq`/`Hash` keys; one graph node per state.
- [Vec](https://doc.rust-lang.org/1.98.1/std/vec/struct.Vec.html):
  indices, not element references, survive vector growth. Each node stores a
  predecessor index; paths are reconstructed only for failures.

The graph's cardinality is a regression fixture, not a measure of proof strength.
Changes to the protocol must review the invariant, transitions, negative controls,
and graph fixture together. Independent protocol review remains pending.
