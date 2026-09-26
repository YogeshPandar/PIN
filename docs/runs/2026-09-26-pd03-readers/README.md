# PD03 encoder and complete consumers

Source: `6f833be` on the draft document-extents branch.

Full core suite: 230 passed, 3 existing ignored, 0 failures.
Core all-target Clippy passes with warnings denied.

Tests cover exact position and phrase parity across PD02/PD03, counted/PB01
thresholds, repeated phrases, empty and long documents, position-domain and
cross-term uniqueness checks, membership readers, malformed/truncated streams,
budget failures and rejection before native-format capability publication.

No native PD03 benchmark was run: insertion is intentionally gated until the
persisted capability and optimized prefix/mapped readers are implemented.
[Format and remaining obligations](../../pd03-document-format.md).
