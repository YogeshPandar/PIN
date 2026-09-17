# G1 API evidence

This is the isolated pure-engine supplement to `api-evidence.md`; it does not
change G0's host-boundary evidence or independent-review status.

Verified against official Rust 1.98.1 documentation (source revision
`48a229cea`, 2026-09-01), read 2026-09-18. Final review remains open.

| Entry | Official contract | Local obligation | Evidence |
|---|---|---|---|
| G1-BYTES | [u32 fields](https://doc.rust-lang.org/1.98.1/std/primitive.u32.html), [slice access](https://doc.rust-lang.org/1.98.1/std/primitive.slice.html) | Check lengths and addition before slicing; serialize explicit LE fields, not Rust layout | `g1_codecs.rs`: independent golden bytes, truncations, overflow, canonical varints |
| G1-ALLOC | [Vec](https://doc.rust-lang.org/1.98.1/std/vec/struct.Vec.html) | Fallible reservation; actual capacity may exceed requested capacity; no process-OOM guarantee | Resource tests added with allocating engine components |
| G1-SCORE | [f64](https://doc.rust-lang.org/1.98.1/std/primitive.f64.html), [BinaryHeap](https://doc.rust-lang.org/1.98.1/std/collections/struct.BinaryHeap.html) | Finite scores, fixed accumulation order, immutable heap keys; logarithms do not promise cross-platform bit identity | Ranking fixtures added with reference ranking |

No new unsafe code or PostgreSQL API is used. The codecs are independently
implemented from the specified bytes and standard-library contracts.
