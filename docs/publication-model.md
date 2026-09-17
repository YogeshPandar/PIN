# Publication-model regression

The merged main test required more than 50 reachable states. The actual abstract
protocol has 37 states. Main CI run `35252527766`, job `105308083472`, reported that
mismatch; its five pure unit tests passed. This was a model-coverage assertion
failure, not evidence of a PostgreSQL runtime defect.

`tools/publication_model.py` encodes the state independently in 11 bits. A queue
traversal and a whole-domain fixed-point traversal agree on the exact reachable
set. The Rust test compares that sorted set with the checked-in fixture and
checks all 76 non-stuttering edges, including coverage of all 11 actions.
Idempotent operations are excluded from edge coverage.

Both deliberately broken protocols still yield replayable counterexamples:
lost merge-time deletion reconciliation and deletion that ignores a retained
source. Each trace is replayed under the corrected rules as well. A graph change
requires reviewing the protocol and regenerating the fixture, not lowering a
threshold until tests pass.

The Rust explorer records a predecessor only when discovering a state. It
constructs a trace only upon failure, rather than allocating and cloning one
per outgoing edge. This is a test-harness allocation reduction, not a measured
search-engine improvement.

Run the independent model with `python3 tools/publication_model.py`, and the
Rust model with `cargo test --locked -p pin-core --test publication_model`.
The Python run does not substitute for the Rust or real PostgreSQL tests.
This model does not represent locks, snapshots, WAL, process death, or recovery.

Rust 1.98.1 API contracts:
<https://doc.rust-lang.org/1.98.1/std/collections/struct.VecDeque.html>
and <https://doc.rust-lang.org/1.98.1/std/collections/hash_map/enum.Entry.html>.
Only the test crate owns these collections; no PostgreSQL pointer enters them.
