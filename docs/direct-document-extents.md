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
bit 0 for this capability; all other bits remain unsupported. The writer reads
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
| 40 | 4 | Prefix length, exactly 3072 |
| 44 | 4 | Reserved, zero |
| 48 | 4 x count | Physical tail blocks, in logical document order |
| after map | 3072 | First PD02 bytes |

Tail pages retain kind 5 and the existing owner/offset/payload format. Their
logical offsets are `3072 + index * FRAGMENT_BYTES`. Every non-final tail payload
has FRAGMENT_BYTES bytes; the final one has the exact remainder. Links must agree
with the map and the last next pointer is NO_BLOCK. The maximum count derives
from the existing 8 MiB document bound; a compile-time assertion proves the whole
map plus prefix fits one standard page payload. There is no multi-level directory
or unbounded allocation.

The head replaces the old first fragment. Its shorter prefix can increase the
number of physical pages by one. This is a measured storage/write trade-off and
must not be represented as free acceleration or compression.

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
