# G1 API evidence

This is the isolated pure-engine supplement to `api-evidence.md`; it does not
change G0's host-boundary evidence or independent-review status.

Verified against official Rust 1.98.1 documentation (source revision
`48a229cea`, 2026-09-01), read 2026-09-18. Final review remains open.

| Entry | Official contract | Local obligation | Evidence |
|---|---|---|---|
| G1-BYTES | [u32 fields](https://doc.rust-lang.org/std/primitive.u32.html), [slice access](https://doc.rust-lang.org/std/primitive.slice.html) | Check lengths and addition before slicing; serialize explicit LE fields, not Rust layout | `g1_codecs.rs`: independent golden bytes, truncations, overflow, canonical varints |
| G1-ALLOC | [Vec](https://doc.rust-lang.org/std/vec/struct.Vec.html) | Fallible reservation; actual capacity may exceed requested capacity; no process-OOM guarantee | Resource tests added with allocating engine components |
| G1-SCORE | [f64](https://doc.rust-lang.org/std/primitive.f64.html), [BinaryHeap](https://doc.rust-lang.org/std/collections/struct.BinaryHeap.html) | Finite scores, fixed accumulation order, immutable heap keys; logarithms do not promise cross-platform bit identity | Ranking fixtures added with reference ranking |

No new unsafe code or PostgreSQL API is used. The codecs are independently
implemented from the specified bytes and standard-library contracts.

## Additional contracts

`str::from_utf8` validates all term/value byte slices before exposing `&str`:
https://doc.rust-lang.org/std/str/fn.from_utf8.html . Slice `get` and checked
arithmetic protect untrusted offset tables. `u64::trailing_zeros` is used only
on nonzero words; `FusedIterator` implementations stay exhausted after `None`:
https://doc.rust-lang.org/std/iter/trait.FusedIterator.html . Tests exercise tails,
malformed counts, duplicate IDs, source generations, NULL/empty values and all
nine mixed container pairs. Validation is allocation-free in these codecs.

The official documentation pages displayed Rust 1.98.1 at inspection; the
version-qualified mirror URLs were unavailable. The observed source revision is
recorded above rather than claiming those mirror URLs were retrievable.

The isolated G1 workflow reports exact compiler/lint/formatter diagnostics in the
PR because the connected GitHub reader cannot retrieve binary log/artifact
archives. It never pushes code, changes branch refs or touches the G0 workflow.
It uses read-only contents and same-repository pull-request comment permission:
https://docs.github.com/en/rest/issues/comments . Final check failures are not
suppressed. Formatter diffs are review inputs, not automatic source changes.
