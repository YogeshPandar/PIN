# Experimental PD03 document encoding

Status: pure encoder and complete consumers implemented. Storage insertion rejects
PD03 before allocation/publication. The native adapter continues producing PD02.
This is an integration checkpoint, not a native performance result.

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
that threshold. This is a provisional crossover, not a tuned production constant.

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

The optimized prefix reader and mapped selective reader are not yet PD03-aware.
A storage insertion guard prevents publishing PD03 until those paths and a
persisted metadata capability are implemented. Existing readers must reject
unsupported index capabilities before interpreting document bytes. A writer
setting alone is insufficient because later sessions must obey the index format.

## Evidence and remaining qualification

Tests compare exact per-term positions and both phrase consumers against an
independent split-word oracle across both encodings, occurrence thresholds,
128-position boundaries, repeated words, absent terms and long documents.
Additional tests cover truncated documents, unknown encodings, mislabeling PD03
as PD02, empty input, insufficient budgets, pre-publication rejection and duplicate
positions across individually valid PB01 streams. Membership and full term
iteration are checked on both formats. Codec tests cover selected block reads.

Native integration must add a persisted capability and complete both inline and
mapped readers. The mapped reader should retain directory metadata and use the
external range callback, rather than reassemble a whole selected stream. Cursor
state must avoid repeated block decodes for nearby targets and repeated phrase
terms. Every allocation retained by those cursors must count against scratch.

After integration, rerun semantic/MVCC/VACUUM/replay qualification and native
paired CPU, index-size, build/WAL and broad query controls. Metadata itself can
consume pages and CPU; the PB01 work bound does not guarantee a SQL speedup.
