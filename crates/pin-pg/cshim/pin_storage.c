/* PostgreSQL-only page operations; Rust owns formats and publication state.
 * contracts: PostgreSQL 18.6 generic_xlog, tableam, bufpage, genam and lmgr.
 * lock order: writer page lock, ascending buffer content locks, generic WAL.
 * every entry runs inside pgrx's PG_TRY boundary; resource owners clean ERROR.
 */
#include "postgres.h"
#include "pin_storage.h"
#include "access/generic_xlog.h"
#include "access/heapam.h"
#include "access/htup_details.h"
#include "access/relscan.h"
#include "access/xlog.h"
#include "catalog/namespace.h"
#include "catalog/pg_amop.h"
#include "catalog/pg_opclass.h"
#include "catalog/pg_type.h"
#include "commands/defrem.h"
#include "miscadmin.h"
#include "nodes/execnodes.h"
#include "nodes/pathnodes.h"
#include "parser/parse_func.h"
#include "parser/parse_oper.h"
#include "storage/bufmgr.h"
#include "storage/bufpage.h"
#include "storage/lmgr.h"
#include "utils/lsyscache.h"
#include "utils/rel.h"
#include "utils/selfuncs.h"
#include "utils/snapmgr.h"
#include "utils/syscache.h"

#define PIN_BATCH_PAGES 3
#define PIN_BITMAP_BATCH 256
#define PIN_PAYLOAD_BYTES (BLCKSZ - MAXALIGN(SizeOfPageHeaderData))

StaticAssertDecl(PG_VERSION_NUM == 180006, "Pin requires PostgreSQL 18.6 headers");
StaticAssertDecl(BLCKSZ == 8192 && MAXALIGN(SizeOfPageHeaderData) == 24,
                 "Pin payload capacity must match the Rust codec");
StaticAssertDecl(MAX_GENERIC_XLOG_PAGES >= PIN_BATCH_PAGES,
                 "Pin requires at least three pages per atomic WAL batch");

static void
pin_corrupt(void)
{
    ereport(ERROR, (errcode(ERRCODE_INDEX_CORRUPTED),
                    errmsg("Pin index has an invalid PostgreSQL page header")));
}

static void
pin_page_check(Page page)
{
    PageHeader header = (PageHeader) page;
    if (PageGetPageSize(page) != BLCKSZ ||
        PageGetPageLayoutVersion(page) != PG_PAGE_LAYOUT_VERSION ||
        header->pd_lower < MAXALIGN(SizeOfPageHeaderData) + 16 ||
        header->pd_lower > BLCKSZ || header->pd_upper != BLCKSZ ||
        header->pd_special != BLCKSZ || header->pd_flags != 0 ||
        header->pd_prune_xid != InvalidTransactionId)
        pin_corrupt();
}

void
pin_storage_check(Relation index, Relation heap, struct IndexInfo *info)
{
    if (index == NULL || index->rd_rel->relpersistence != RELPERSISTENCE_PERMANENT ||
        !RelationNeedsWAL(index) || RelationGetDescr(index)->natts != 1 ||
        TupleDescAttr(RelationGetDescr(index), 0)->atttypid != TEXTOID ||
        (heap != NULL && (heap->rd_rel->relpersistence != RELPERSISTENCE_PERMANENT ||
                          heap->rd_tableam != GetHeapamTableAmRoutine())) ||
        (info != NULL && (info->ii_Concurrent || info->ii_NumIndexAttrs != 1 ||
                          info->ii_NumIndexKeyAttrs != 1)))
        ereport(ERROR, (errcode(ERRCODE_FEATURE_NOT_SUPPORTED),
                        errmsg("Pin G2 requires a logged permanent heap table, one text key and a nonconcurrent build")));
    if (RecoveryInProgress())
        ereport(ERROR, (errcode(ERRCODE_FEATURE_NOT_SUPPORTED),
                        errmsg("Pin index access during recovery is not supported")));
}

void
pin_writer_lock(Relation index)
{
    LockPage(index, 0, ExclusiveLock);
}

void
pin_writer_unlock(Relation index)
{
    UnlockPage(index, 0, ExclusiveLock);
}

uint32
pin_storage_blocks(Relation index)
{
    return RelationGetNumberOfBlocks(index);
}

uint32
pin_storage_extend(Relation index)
{
    Buffer buffer = ReadBuffer(index, P_NEW);
    BlockNumber block = BufferGetBlockNumber(buffer);
    ReleaseBuffer(buffer);
    return block;
}

uint32
pin_storage_read(Relation index, uint32 block, uint8 *out, uint32 capacity)
{
    Buffer buffer;
    Page page;
    uint32 length;
    if (out == NULL || capacity != PIN_PAYLOAD_BYTES ||
        block >= RelationGetNumberOfBlocks(index))
        pin_corrupt();
    buffer = ReadBuffer(index, block);
    LockBuffer(buffer, BUFFER_LOCK_SHARE);
    page = BufferGetPage(buffer);
    if (PageIsNew(page))
    {
        /* only an entirely zero page is an uninitialized allocation orphan. */
        for (uint32 i = 0; i < BLCKSZ; i++)
            if (((const uint8 *) page)[i] != 0)
                pin_corrupt();
        UnlockReleaseBuffer(buffer);
        return 0;
    }
    pin_page_check(page);
    length = ((PageHeader) page)->pd_lower - MAXALIGN(SizeOfPageHeaderData);
    memcpy(out, PageGetContents(page), length);
    UnlockReleaseBuffer(buffer);
    return length;
}

void
pin_storage_commit(Relation index, uint32 count, const uint32 *blocks,
                   const uint8 *const *bytes, const uint32 *lengths,
                   const bool *full_images)
{
    Buffer buffers[PIN_BATCH_PAGES];
    GenericXLogState *state;
    BlockNumber nblocks = RelationGetNumberOfBlocks(index);
    if (count == 0 || count > PIN_BATCH_PAGES)
        elog(ERROR, "invalid Pin WAL batch size");
    for (uint32 i = 0; i < count; i++)
        if (blocks[i] >= nblocks || (i > 0 && blocks[i] <= blocks[i - 1]) ||
            bytes[i] == NULL || lengths[i] < 16 || lengths[i] > PIN_PAYLOAD_BYTES)
            elog(ERROR, "invalid Pin WAL page image");
    /* lock order and replay registration order are identical. */
    for (uint32 i = 0; i < count; i++)
    {
        buffers[i] = ReadBuffer(index, blocks[i]);
        LockBuffer(buffers[i], BUFFER_LOCK_EXCLUSIVE);
    }
    state = GenericXLogStart(index);
    for (uint32 i = 0; i < count; i++)
    {
        Page image = GenericXLogRegisterBuffer(state, buffers[i],
                         full_images[i] ? GENERIC_XLOG_FULL_IMAGE : 0);
        if (full_images[i])
            PageInit(image, BLCKSZ, 0);
        else
            pin_page_check(image);
        memcpy(PageGetContents(image), bytes[i], lengths[i]);
        ((PageHeader) image)->pd_lower = MAXALIGN(SizeOfPageHeaderData) + lengths[i];
    }
    /* PostgreSQL owns the only critical section, dirty marks and page LSNs. */
    GenericXLogFinish(state);
    for (uint32 i = 0; i < count; i++)
        UnlockReleaseBuffer(buffers[i]);
}

void
pin_storage_interrupt(void)
{
    CHECK_FOR_INTERRUPTS();
}

void
pin_root_coordinates(ItemPointer tid, uint32 *block, uint16 *offset)
{
    if (tid == NULL || !ItemPointerIsValid(tid) ||
        ItemPointerGetOffsetNumber(tid) > MaxHeapTuplesPerPage)
        elog(ERROR, "invalid Pin heap root");
    *block = ItemPointerGetBlockNumber(tid);
    *offset = ItemPointerGetOffsetNumber(tid);
}

double
pin_heap_build_scan(Relation heap, Relation index, struct IndexInfo *info,
                    IndexBuildCallback callback, void *state)
{
    return table_index_build_scan(heap, index, info, true, true, callback, state, NULL);
}

void
pin_bitmap_add(TIDBitmap *bitmap, uint32 count,
               const uint32 *blocks, const uint16 *offsets)
{
    ItemPointerData roots[PIN_BITMAP_BATCH];
    if (bitmap == NULL || count > PIN_BITMAP_BATCH)
        elog(ERROR, "invalid Pin bitmap batch");
    for (uint32 i = 0; i < count; i++)
    {
        if (blocks[i] == InvalidBlockNumber || offsets[i] == 0 ||
            offsets[i] > MaxHeapTuplesPerPage)
            elog(ERROR, "invalid Pin bitmap root");
        ItemPointerSet(&roots[i], blocks[i], offsets[i]);
    }
    /* all candidates require exact SQL rechecks, including lossy bitmap pages. */
    tbm_add_tuples(bitmap, roots, (int) count, true);
}

bool
pin_vacuum_removable(IndexBulkDeleteCallback callback, void *state,
                     uint32 block, uint16 offset)
{
    ItemPointerData tid;
    if (callback == NULL || block == InvalidBlockNumber ||
        offset == 0 || offset > MaxHeapTuplesPerPage)
        elog(ERROR, "invalid Pin VACUUM callback");
    ItemPointerSet(&tid, block, offset);
    return callback(&tid, state);
}

static Oid
pin_query_type(void)
{
    Oid namespace = get_namespace_oid("pin", false);
    Oid type = GetSysCacheOid2(TYPENAMENSP, Anum_pg_type_oid,
                              CStringGetDatum("query"), ObjectIdGetDatum(namespace));
    if (!OidIsValid(type) || get_typtype(type) != TYPTYPE_DOMAIN ||
        getBaseType(type) != BYTEAOID)
        elog(ERROR, "Pin query domain is missing or incompatible");
    return type;
}

static Oid
pin_match_operator(Oid query_type)
{
    return OpernameGetOprid(list_make2(makeString("pin"), makeString("@@@")),
                           TEXTOID, query_type);
}

void
pin_scan_validate(IndexScanDesc scan)
{
    Oid query_type = pin_query_type();
    Oid match = get_opcode(pin_match_operator(query_type));
    if (scan == NULL || scan->numberOfKeys <= 0 || scan->numberOfKeys > 1024 ||
        scan->numberOfOrderBys != 0 || scan->keyData == NULL ||
        scan->xs_snapshot == NULL || !IsMVCCSnapshot(scan->xs_snapshot) ||
        !OidIsValid(match))
        ereport(ERROR, (errcode(ERRCODE_FEATURE_NOT_SUPPORTED),
                        errmsg("Pin supports keyed MVCC bitmap scans only")));
    for (int i = 0; i < scan->numberOfKeys; i++)
    {
        ScanKey key = &scan->keyData[i];
        if (key->sk_attno != 1 || key->sk_strategy != 1 ||
            (key->sk_flags & ~SK_ISNULL) != 0 || key->sk_func.fn_oid != match ||
            (OidIsValid(key->sk_subtype) && key->sk_subtype != query_type))
            ereport(ERROR, (errcode(ERRCODE_FEATURE_NOT_SUPPORTED),
                            errmsg("Pin scan key does not match pin.text_ops")));
    }
}

typedef struct PinScanState
{
    int capacity;
} PinScanState;

IndexScanDesc
pin_scan_begin(Relation index, int nkeys, int norderbys)
{
    IndexScanDesc scan;
    PinScanState *state;
    if (nkeys <= 0 || nkeys > 1024 || norderbys != 0)
        ereport(ERROR, (errcode(ERRCODE_FEATURE_NOT_SUPPORTED),
                        errmsg("Pin supports keyed forward bitmap scans only")));
    scan = RelationGetIndexScan(index, nkeys, 0);
    state = palloc(sizeof(PinScanState));
    state->capacity = nkeys;
    scan->opaque = state;
    return scan;
}

void
pin_scan_rescan(IndexScanDesc scan, ScanKey keys, int nkeys, int norderbys)
{
    PinScanState *state;
    if (scan == NULL || scan->opaque == NULL)
        elog(ERROR, "invalid Pin scan state");
    state = (PinScanState *) scan->opaque;
    if (nkeys < 0 || nkeys > state->capacity || norderbys != 0)
        elog(ERROR, "invalid Pin rescan dimensions");
    if (keys != NULL)
    {
        memmove(scan->keyData, keys, nkeys * sizeof(ScanKeyData));
        scan->numberOfKeys = nkeys;
    }
}

void
pin_scan_end(IndexScanDesc scan)
{
    if (scan->opaque != NULL)
    {
        pfree(scan->opaque);
        scan->opaque = NULL;
    }
}

bool
pin_opclass_validate(Oid opclass)
{
    HeapTuple tuple = SearchSysCache1(CLAOID, ObjectIdGetDatum(opclass));
    Form_pg_opclass form;
    Oid family;
    bool valid;
    if (!HeapTupleIsValid(tuple))
        return false;
    form = (Form_pg_opclass) GETSTRUCT(tuple);
    family = form->opcfamily;
    valid = form->opcnamespace == get_namespace_oid("pin", false) &&
            strcmp(NameStr(form->opcname), "text_ops") == 0 &&
            form->opcmethod == get_am_oid("pin", false) &&
            form->opcintype == TEXTOID && !OidIsValid(form->opckeytype);
    ReleaseSysCache(tuple);
    if (valid)
    {
        Oid query_type = pin_query_type();
        Oid expected = pin_match_operator(query_type);
        valid = OidIsValid(expected) &&
                get_opfamily_member(family, TEXTOID, query_type, 1) == expected;
    }
    return valid;
}

void
pin_opclass_adjust(Oid opclass, List *operators, List *functions)
{
    OpFamilyMember *member;
    Oid query_type;
    /* CREATE OPCLASS has not advanced the catalog command counter yet. */
    if (!OidIsValid(opclass) || list_length(operators) != 1 || functions != NIL)
        ereport(ERROR, (errcode(ERRCODE_FEATURE_NOT_SUPPORTED),
                        errmsg("Pin accepts only its closed match operator class")));
    member = (OpFamilyMember *) linitial(operators);
    query_type = pin_query_type();
    if (member->is_func || member->number != 1 || member->lefttype != TEXTOID ||
        member->righttype != query_type || OidIsValid(member->sortfamily) ||
        member->object != pin_match_operator(query_type))
        ereport(ERROR, (errcode(ERRCODE_FEATURE_NOT_SUPPORTED),
                        errmsg("Pin accepts only its closed match operator class")));
}

void
pin_index_cost(struct PlannerInfo *root, struct IndexPath *path, double loops,
               Cost *startup, Cost *total, Selectivity *selectivity,
               double *correlation, double *pages)
{
    GenericCosts costs = {0};
    /* no calibrated term statistics yet: price a full index traversal. */
    costs.numIndexTuples = Max(1.0, path->indexinfo->tuples);
    genericcostestimate(root, path, loops, &costs);
    *startup = costs.indexStartupCost;
    *total = costs.indexTotalCost;
    *selectivity = costs.indexSelectivity;
    *correlation = 0;
    *pages = costs.numIndexPages;
}
