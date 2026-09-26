# Experimental PD03 document encoding

Status: an opt-in persisted index capability now permits PD03 under the existing
writer, reader and WAL barriers. The creation option `pin.enable_blocked_positions`
defaults off; it implies mapped document extents. Native measurements remain
necessary before default enablement or a performance claim.

## Encoding

The 16-byte header retains profile id, document token count and term count.
Magic is PD03 instead of PD02. The lexically ordered term record retains:

| Field | Bytes | Meaning |
| --- | ---: | --- |
| UTF-8 term length | 2 | Existing nonzero bounded length |
| Position encoding | 2 | 0 counted delta stream, 1 PB01 block stream |
| Position byte length | 4 | Exact following positional stream length |
| Term | variable | Existing normalized UTF-8 bytes |
| Positions | variable | Encoding selected above |

PD02 continues requiring encoding 0. PD03 rejects encoding values above 1.
PB01 retains its unchanged block directory and per-block delta payload. Frequencies
come from the selected codec's count; their sum must equal the document count.
The experimental encoder selects PB01 at 256 occurrences and counted deltas below
that threshold. If no term reaches the threshold, it emits PD02 under the blocked
index capability; native inserts retain their provisional PD02 document. This is a provisional crossover, not a tuned production constant.

Preparation keeps explicit peak-budget charges for analyzed input, sorted token
references, largest block-term position scratch, reusable encoding scratch and
output. Each term's encoded length is computed before output allocation. Full
validation retains the document-wide position bitset, rejecting missing/duplicate
positions across terms as well as malformed streams and out-of-domain positions.

## Consumers in this checkpoint

- Complete `PreparedDocument` copy/validation and term iteration.
- A common position view/iterator for counted and block streams.
- Full and selected complete-document phrase matching.
- Grouped consumers' validated term iteration and membership reader.
- PB01 streaming iteration with bounded state, overflow/order/trailing checks.

Metadata field 28 uses bit 0 for mapped extents and bit 1 for PD03; only values
0, 1 and 3 are accepted. Bit 1 requires bit 0. Old readers reject the new
capability. The writer chooses PD03 using this persisted flag, even after the
creation GUC is switched off. The optimized inline and mapped phrase readers use
selected directory metadata and one checked block payload at a time. They cache
decoded blocks for successive targets and retain independent cursors for repeated
query terms. Complete grouped readers reconstruct/validate both formats. A PD03 document in a legacy index is rejected before owner allocation. The
blocked capability accepts both PD02 and PD03 documents.

## Evidence and remaining qualification

Tests compare exact per-term positions and both phrase consumers against an
independent split-word oracle across both encodings, occurrence thresholds,
128-position boundaries, repeated words, absent terms and long documents.
Additional tests cover truncated documents, unknown encodings, mislabeling PD03
as PD02, empty input, insufficient budgets, pre-publication rejection and duplicate
positions across individually valid PB01 streams. Membership and full term
iteration are checked on both formats. Codec tests cover selected block reads.

The fixed cursor and metadata arrays, selected directory bytes, additional
page image and byte scratch are charged against query scratch. A budget shortfall
returns heap recheck. The native adapter currently prepares PD02 before acquiring
the writer interlock, then reanalyzes and prepares PD03 inside that interlock if
the persisted flag requires it. This preserves existing lock ordering and default
index behavior but adds write CPU and lock hold time to experimental PD03 indexes.

Native paired CPU, index size, build/WAL, MVCC/VACUUM/replay and broad query
controls remain qualification gates. Metadata can consume pages and CPU; PB01's
work bound alone does not guarantee SQL speedup.
