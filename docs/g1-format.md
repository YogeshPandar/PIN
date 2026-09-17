# G1 experimental payload format, version 1

These are pure-Rust payloads, not PostgreSQL pages. No stable disk ABI is promised
before format review. Integer fields are fixed-width little-endian except where
canonical u32 varints are explicitly specified. No native structs are persisted.
Output belongs to the caller. Only a successful encoder's returned prefix is a
record; low-level writer failures may leave a partial prefix. Decoders never
skip malformed entries to return partial exact results.

## Canonical u32 varint

Unsigned base-128, least-significant seven bits first. Bit 7 means another group
follows. One through five bytes; the fifth byte must be at most 15. Multi-byte
encodings may not finish with a zero group. Zero is `00`. A failed varint read
does not advance the reader. Distinguish truncation, overflow and noncanonical
encoding.

## Positions

A `u32` count followed by that many canonical u32 varints. The first position is
absolute and may be zero; later values are strictly positive deltas. Checked
addition reconstructs exact positions, including gaps. The maximum position is
`u32::MAX`; repeated positions for one term are rejected. Repeated *terms* remain
valid and have different positions. The caller supplies a maximum count.
Trailing bytes are invalid. Validation and iteration allocate no memory.

`[0, 1, 128, 300]` is `04 00 00 00 00 01 7f ac 01`.

## Offset containers

Each container starts with `domain:u16, cardinality:u16, tag:u8, reserved:u8`.
The domain must equal the supplied `HeapLayout`; offsets are one-based. Reserved
bytes are zero. Encodings are `0=sparse`, `1=bitmap`, `2=runs`:

- Sparse: exactly `cardinality` strictly increasing `u16` offsets.
- Bitmap: exactly `ceil(domain/8)` bytes, least-significant bit first, offset 1
  at bit 0. Bits above the domain are zero; popcount must equal cardinality.
- Runs: `run_count:u16`, then `(start:u16, length:u16)` pairs. Runs are nonempty,
  ordered, disjoint, nonadjacent, and entirely within the domain.

Every valid encoding is readable. Writers minimize complete encoded byte size;
ties prefer sparse, then bitmap, then runs. This is a byte-size policy, not a
benchmarked latency threshold. Set operations use eight private `u64` words.
Difference subtracts from an explicit left universe; no API complements the
entire heap coordinate space. Mixed-domain operations are errors.

## Record envelope

| Byte offset | Field |
|---|---|
| 0 | Four-byte magic `PIN1` |
| 4 | Kind: document 1, nullable value 2, dictionary 3, manifest 4 |
| 5 | Reserved zero byte |
| 6 | Format version `u16`, currently 1 |
| 8 | Feature bits `u32`, currently zero |
| 12 | Exact payload byte length `u32` |
| 16 | Payload |

The entire input must contain exactly one record. Unknown versions/features/tags,
nonzero reserved bytes, invalid UTF-8, and trailing bytes are rejected. Reported
error offsets are local to the envelope or payload decoder that rejected input.

## Document owner summary (kind 1)

The 36-byte payload is `segment:u64, incarnation:u64, block:u32, offset:u16,
reserved:u16, token_count:u32, profile:u32, publication:u8, live:u8, reserved:u16`.
Segment/incarnation/profile are nonzero. The root is checked against caller-supplied
heap layout. Publication tags are allocated 0, fragments-written 1, published 2,
abandoned 3. Live is exactly 0 or 1 and can be 1 only for a published summary.
A published summary may be dead. These fields do not establish MVCC visibility.

The enclosing source supplies the validated relation physical generation. A bare
document payload is not a global identity and must not be reused in another
relation. The G0 `DocumentRef` API is unchanged. Encoding a summary does not
implement or certify a durable publication transition.

## Nullable UTF-8 value (kind 2)

Null is the single payload byte `00`. Non-null is `01, byte_length:u32, utf8`.
Empty text is non-null with byte length zero, not null. NUL characters are allowed
in this pure value format; a future PostgreSQL adapter enforces its own text
input constraints. The decoder borrows the input and preserves exact bytes.

## Term dictionary (kind 3)

Payload: `profile:u32, term_count:u32, offsets:[u32; term_count+1], term_bytes`.
Offsets are relative to `term_bytes`, start at zero and end at its exact length.
Each term is nonempty UTF-8. Terms are strictly increasing by UTF-8 byte order.
The empty dictionary has one zero offset and no term bytes. Profile is nonzero;
the storage decoder cannot prove that a caller actually ran that analyzer.

IDs are zero-based and local to this dictionary. Exact equality always compares
full term bytes. Offsets permit binary-search lookup without allocating or
reconstructing a prefix-compressed block. Prefix traversal returns a complete
ID interval or a resource-limit error, never a truncated expansion. Input byte,
term count and per-term byte limits are explicit caller parameters.

## Source manifest (kind 4)

Payload: `generation:u64, source_count:u32`, followed by 16-byte entries:
`segment:u64, sealed:u8, reserved:[u8;7]`. Identifiers are nonzero, entries strictly
increase by segment ID, and sealed is exactly 0 or 1. Empty manifests are valid.
The caller supplies source-count and total-byte limits.

This codec validates structure, not logical coverage, writer quiescence, page
reachability, reader registration, WAL ordering or reclaimability. Those remain
G2/G3 host-storage obligations; a source cannot become safely sealed merely by
changing this flag.
