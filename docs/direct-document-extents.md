# Optional direct document extents

Experimental opt-in storage capability. It preserves PD02 token/position bytes,
adds physical addressing of document tail fragments, and supports selected-stream
phrase reads. It is an A3 bridge toward B1, not the packed primary index or PB01
integration. PB01 is still not used by native storage.

## Creation and compatibility

```sql
SET pin.enable_direct_documents = on;
CREATE INDEX documents_search ON documents USING pin(body);
SET pin.enable_phrase_positions = on;
```

The creation setting defaults off. Metapage u32 field at payload offset 28 uses
bit 0 for mapped extents and bit 1 for PD03 positions. Only 0, 1, and 3 are
supported; bit 1 requires bit 0. The writer reads
this persisted flag for subsequent inserts, regardless of the session setting.
REINDEX chooses the format using the setting active for that rebuild. Existing
flag-zero indexes retain the legacy layout and remain readable by the new code.
Older PIN binaries require zero in field 28 and reject the new format. Deploy
compatible readers before enabling the capability; a downgrade requires rebuilding
in the legacy format using a compatible binary first. This is an explicit storage
compatibility change, not a silent upgrade of existing indexes.

## Physical layout

Documents no larger than INLINE_BYTES retain their existing inline owner payload.
For larger documents, owner.data_head references PIN2 page kind 11, the version-one
document directory. Integers are little endian, with no serialized native structs.

| Payload offset | Bytes | Field |
| --- | ---: | --- |
| 0 | 16 | Existing PIN2 header, kind 11, next = first tail fragment |
| 16 | 16 | OwnerRef, including incarnation |
| 32 | 4 | Complete PD02 byte length |
| 36 | 4 | Tail fragment count |
| 40 | 4 | Prefix length, computed from available page space |
| 44 | 4 | Directory layout version, 1 |
| 48 | 4 x count | Physical tail blocks, in logical document order |
| after map | prefix length | First PD02 bytes |

Tail pages retain kind 5 and the existing owner/offset/payload format. Their
logical offsets are `prefix_length + index * FRAGMENT_BYTES`. Every non-final tail payload
has FRAGMENT_BYTES bytes; the final one has the exact remainder. Links must agree
with the map and the last next pointer is NO_BLOCK. The maximum count derives
from the existing 8 MiB document bound; a compile-time assertion proves the whole
map plus prefix fits one standard page payload. There is no multi-level directory
or unbounded allocation.

The writer chooses `count = ceil(max(total - 8120, 0) / 8128)` and
`prefix = min(total, 8120 - 4 * count)` for this 8 KiB page format. Each tail
adds 8132 payload bytes and costs four bytes in the head map. A directory can
have zero tails, with next = NO_BLOCK. Version 0 remains readable with its fixed
3072-byte prefix; unknown versions are rejected. The adaptive version fills the
head when tails exist, avoiding the prototype's unnecessary third page for the
16 KiB benchmark documents. Metadata still consumes space, so page counts can
exceed the legacy layout near boundaries; acceleration requires measurement.

## Publication, recovery and readers

Tail fragments are written in reverse order, then the directory, then owner
PayloadReady, dictionary links, and final Published state. The owner already
exists before any payload page is stored. Removing a free page from the free
list and assigning its new identity remains one existing generic WAL batch.
There are no new WAL entry points or independently published side tables.

VACUUM recognizes both payload kinds by OwnerRef and reclaims them for unpublished
or removed owners. Allocation-orphan checking includes every directory reference.
The existing structural barrier protects immutable payloads from concurrent
reclamation/reuse; normal PostgreSQL heap visibility still decides row visibility.

Full grouped build/frontier consumers reconstruct and validate the entire document
through the mapped reader, checking all offsets, owners, exact lengths and links.
Selected phrase reads first try the head prefix. On an inconclusive prefix, they
scan PD02 term headers through virtual offsets, skip unrelated positional byte
ranges, copy each unique requested stream once, and reuse the existing witness
kernel. Repeated terms have independent cursors over the shared copied stream.

The mapping reader validates each fetched fragment's owner/incarnation, expected
logical offset, exact payload length and next link. Queries do not certify skipped
payload bytes. Full document reconstruction plus the document validator supplies
the complete positional integrity check. Directory metadata is checked for
extents and valid block identities; it is not a cryptographic integrity proof.

The head buffer, additional tail image, selected-range metadata and retained
selected bytes are bounded. If the remaining budget cannot hold the required
scratch, the query retains heap recheck. A query can now prove a selected phrase
with a budget smaller than the complete document. No PostgreSQL pointer enters
the pure engine and no new unsafe operation is added.

## Qualification requirements

The core suite covers legacy/new semantic parity, physical skipped-page counts,
small-budget exact proof and fallback, repeated/Unicode terms, header crossings,
maximum map size, truncation, malformed identities and interrupted insertion /
VACUUM boundaries followed by free-page reuse. Native qualification must include
actual plans/identities, grouped maintenance, committed/uncommitted crash replay,
and build/WAL/index-size costs alongside query CPU. A synthetic kernel or a
single fast phrase does not establish production readiness or TIN parity.
