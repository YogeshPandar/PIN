# Addressable position reader

Status: codec interface implemented; PB01 is not yet used by native PIN storage.
No PostgreSQL performance improvement is claimed by this change.

## Why this interface

Native mapped documents can skip unrelated terms, but selected PD02 terms still
require copying their whole counted-delta stream. The 60K negative benchmark
remains slower than legacy PIN despite wider dense-run comparisons. PB01 already
encodes independently decodable blocks, but its original API required a slice of
the complete positional payload. That would preserve unnecessary physical reads.

`PositionDirectory` separates checked metadata from externally located bytes.
`encoded_len` validates the exact eight-byte header and bounds the metadata read.
`open` validates the complete directory against the known external payload length:
counts, contiguous extents, per-block byte limits, strict cross-block order and
first/last feasibility. Directory size and payload size are separate quantities.

`select` binary-searches last-position bounds and returns an unforgeable checked
request with a payload-relative byte range. `seek_with` calls a supplied reader
at most once, using 635 bytes of stack scratch (127 deltas, at most five bytes
each). A target beyond the final block reads no payload. Singleton blocks also
need no payload bytes. Caller errors are preserved through a generic error type.
Selected blocks are fully decoded and checked even after an early match.

The existing contiguous `PositionBlocks` API delegates to the same directory and
request decoder. PB01 serialized bytes are unchanged. Full `validate_all` remains
necessary to certify payloads that individual searches skip. Neither directory
bounds nor a successful selected read certify all data or provide a checksum.

## Adapter obligations

- Bind a directory to its immutable document and term identity for the full read.
- Map payload-relative offsets into the checked physical extent reader without
  silently truncating or substituting another document's bytes.
- Charge directory buffers and fixed scratch to the query budget; fall back or
  reject explicitly if the complete operation cannot fit.
- Preserve cancellation and storage errors; a failed read is not an empty result.
- Apply document token-domain checks in addition to this codec's cardinality,
  order and overflow checks. `max_positions` is a count limit, not a maximum
  position value; existing codec tests allow a singleton at u32::MAX.
- Cache decoded selected blocks for repeated targets; repeatedly decoding all
  128 positions for nearby witnesses would waste CPU.

## Evidence

Eight focused tests pass, including sorted-vector oracles, corrupted/truncated
metadata and payloads, malformed selected tails, zero/singleton/extreme values,
external fetch lengths, propagated storage errors and the 60K selected-read bound.
All-target core Clippy is clean. Logs are under
`docs/runs/2026-09-26-position-reader/`.

## Native integration still required

The next document-format capability must discriminate PD02 counted streams from
block streams explicitly and reject incompatible old readers. Changes must cover
PreparedDocument encoding, complete validation and positional iteration, grouped
build/frontier consumers, inline and mapped phrase readers, owner length checks,
small-budget fallback and legacy indexes. Block descriptors and exact frequencies
must be available before positional payload reads. Repeated phrase terms need
independent cursors over shared immutable metadata.

Only after those paths share a coherent format can native build/query/WAL/size,
MVCC/VACUUM/replay and independent semantic tests qualify the representation.
This interface does not complete A1 packed canonical postings or A2 SQL BM25.
