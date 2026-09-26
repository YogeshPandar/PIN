# Experimental seekable position blocks

Status: pure Rust codec and benchmark only. Native PD02 documents, writer,
fragment reader, WAL, VACUUM, and SQL execution do not use PB01. Existing indexes
remain unchanged. This is groundwork for A3/B1, not a new supported index format.

## Motivation and evidence

The [native prefix profile](runs/2026-09-26-prefix-witness/README.md) still samples
40.56% of CPU in memmove. The synthetic negative phrase costs 9.873 ms of backend
CPU versus stored-vector GIN's 0.214 ms. Copying complete document fragments and
walking long occurrence streams remain work to remove.

[TIN's architecture](https://planetscale.com/blog/anatomy-of-a-postgres-search-engine)
separates positional data from membership and loads it only for positional
queries. PB01 explores independently addressable position extents. It is an
original implementation; no TIN or Lead source was copied. The public article
does not establish TIN's exact position codec, block size, or directory format.

## PB01 byte contract

All integers are unsigned little endian. No native structs or pointers are
serialized. Caller owns input and output buffers; the codec allocates nothing.

| Field | Bytes | Meaning |
| --- | --- | --- |
| magic/version | 4 | ASCII PB01 |
| count | 4 | Total occurrences, checked against caller limit |
| directory | 16 per block | first position, last position, payload start, payload end |
| payload | variable | Canonical positive u32 varint deltas after the first occurrence of each block |

Blocks contain 128 occurrences except the final block. Count determines the
number of blocks and occurrences in each block. The first position is stored in
the directory, allowing each block to restart independently. Positions are
strictly increasing, including across block boundaries. Zero is a valid first
position; u32::MAX is valid if no subsequent position exists. Empty streams use
only the eight-byte header. A one-position block has no delta payload.

Offsets are relative to the payload start. Extents must be contiguous with no
gaps, overlaps, or trailing bytes. Payload size must fit u32. Encoding validates
input order, caller limits, arithmetic and capacity before mutating output.
`open` checks the complete directory and extents, including minimum/maximum byte
length and whether the endpoint range can contain the declared count.

`seek_ge` finds the first block whose last position reaches the target, then
validates and decodes that entire block. It returns the first occurrence at or
after the target and exact decoded-byte/occurrence counts. Work after directory
validation is O(log(blocks) + 128). `open` is O(blocks); it is not free and must
be counted for independently opened streams. Views may only be reused while
their borrowed bytes remain stable.

## Integrity scope

Seeking does not certify skipped payloads. Directory bounds are format metadata,
not a cryptographic integrity proof. `validate_all` verifies every delta and
cross-checks each block endpoint. A malformed skipped block can coexist with a
successful selected-block lookup. Tests explicitly preserve this distinction.
Never use successful `open` or `seek_ge` as an integrity checker.

The selected block is always validated fully, even after an early match. This
bounds validation to 128 occurrences but is slower than the existing lazy prefix
reader for early witnesses. It should not replace that reader indiscriminately.

## Native integration gates

1. Define a versioned term/position directory with physical block locators. PB01
   currently borrows a contiguous slice. Putting it inside the existing linked
   document payload would still require chain traversal and copying.
2. Prove direct reads retain owner/publication/incarnation protection and reader
   lifetime rules during append, compaction, VACUUM, REINDEX, and crash recovery.
3. Implement writer/build/maintenance and compatibility together. Keep old-format
   readers or require an explicit documented rebuild; never relabel PD02 bytes.
4. Add phrase/proximity cursors that retain the checked directory per query,
   use early-witness paths for low targets, and seek for large advances. Budget
   retained directory bytes and fetched blocks, and support repeated terms.
5. Repeat native identity/lifecycle oracles and paired backend CPU measurements
   against stored-vector GIN. Cover early/late/negative and short/long documents.

Heap visibility, HOT chains, residual predicates, and security checks remain
PostgreSQL's obligations. A position lookup cannot bypass them. This codec does
not implement native ranking, fuzzy traversal, positional filters, or TIN parity.
