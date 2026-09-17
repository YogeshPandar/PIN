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
