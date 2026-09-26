# Experimental packed canonical dictionary

`pin.enable_packed_postings` is a superuser creation setting and defaults off.
New indexes created with it on persist metapage capability bit 2. Subsequent
inserts use the persisted bit even if the setting is off. REINDEX selects the
setting active during the rebuild. Legacy pages remain readable.

This is the first narrow part of `pin_next.md` A1. A one-owner term already
stores its owner in the dictionary. A packed dictionary page reserves 16 more
bytes per term for a second owner. The second insert writes that owner into the
same page in one WAL publication. A third insert creates a regular posting page
containing owners two and three, and atomically switches the dictionary chain.
All later inserts use the regular chain. This removes one dedicated physical
page for terms occurring in exactly two documents. It does not implement the
shared arena or adaptive dense containers required by the full A1/B1 plan.

## Format and reader lifetime

The existing PIN2 dictionary page tag remains 3. Header byte 7 distinguishes
legacy (0) and packed (1) entries. Packed entries reserve 16 bytes after the
term text. The existing 32-byte term header's last u32 is a state:

| State | Chain | Reserved owner |
| --- | --- | --- |
| 0 | none or ordinary posting chain | zero bytes |
| 1 | head and tail equal dictionary page | active second owner |
| 2 | ordinary posting chain | shadow of the former second owner |

State 2 is required by the current reader/writer lock design. A scan can
capture the dictionary page as a posting head while a concurrent third insert
promotes that term. The writer retains the second owner as a shadow, so the
captured scan can still synthesize its old single-posting view. New scans use
the new chain. Owner incarnations and checked ordering remain mandatory.
Compaction clears a dead state-1 owner only while holding the exclusive
structural barrier. It does not reclaim a shared dictionary page. Changing a
posting head invalidates published grouped frontier anchors before the switch;
a scan with an earlier captured snapshot can use a full chain walk when a
promoted shadow proves why the head changed.

Each page is still validated before use. The read path for a current state-1
term uses the already captured second owner, avoiding a second dictionary
buffer read. The virtual posting page fallback remains for captured readers
and older grouped consumers. No PostgreSQL pointer is stored in this format.

## Qualification gates

The bounded pure tests cover 1,000 two-document terms, ordinary promotion,
VACUUM, slot reuse, compaction, grouped build and post-promotion scans, and a
promotion interleaved after dictionary capture. The native suite covers
MVCC/HOT/indexed updates, deletion, VACUUM, REINDEX and immediate-crash WAL
replay. [Measurements and raw artifacts](runs/2026-09-26-packed-canonical/README.md).

The fixed 16-byte reservation can hurt a vocabulary with many terms per
dictionary page. The 2,000-term singleton fixture did not add a physical page,
but that does not prove it never will. Query CPU is roughly tied on the current
small paired fixture; this change has not demonstrated a TIN-like read gain.
Before enabling by default, qualify a larger vocabulary distribution, packed
index upgrade/downgrade expectations, concurrency stress with PostgreSQL
backends, standby replay, resource pressure, and the full feature matrix.
