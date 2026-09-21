/* opt-in upper count node; host lifetimes and proof are in docs/g5-counts.md. */
#include "postgres.h"
#include "access/genam.h"
#include "access/parallel.h"
#include "access/heapam.h"
#include "access/htup_details.h"
#include "access/table.h"
#include "access/tableam.h"
#include "access/visibilitymap.h"
#include "access/xact.h"
#include "access/xlog.h"
#include "catalog/namespace.h"
#include "catalog/pg_type.h"
#include "commands/defrem.h"
#include "commands/explain.h"
#include "commands/explain_format.h"
#include "common/int.h"
#include "executor/executor.h"
#include "miscadmin.h"
#include "nodes/extensible.h"
#include "nodes/makefuncs.h"
#include "optimizer/cost.h"
#include "optimizer/optimizer.h"
#include "optimizer/pathnode.h"
#include "optimizer/planner.h"
#include "parser/parse_func.h"
#include "parser/parse_oper.h"
#include "storage/bufmgr.h"
#include "storage/spin.h"
#include "utils/builtins.h"
#include "utils/guc.h"
#include "utils/lsyscache.h"
#include "utils/memutils.h"
#include "utils/rel.h"
#include "utils/snapmgr.h"
#include "utils/syscache.h"
#include "pin_count.h"
#include "pin_parallel.h"
#include "pin_storage.h"

#define PIN_COUNT_STATS 8
#define PIN_COUNT_WORK_WORDS 11
#define PIN_COUNT_KEY_SHARED UINT64CONST(21)

static bool pin_enable_count = false;
static bool pin_enable_count_vm = false;
static int pin_count_parallel_workers = 0;
static Size pin_count_participant_memory = 0;
static create_upper_paths_hook_type pin_previous_upper = NULL;

typedef struct PinCountParallelShared
{
    Oid heaprelid;
    Oid indexrelid;
    AttrNumber attribute;
    Size query_length;
    slock_t mutex;
    uint64 work[PIN_COUNT_WORK_WORDS];
    int64 count;
    uint64 stats[PIN_COUNT_STATS];
    char query[FLEXIBLE_ARRAY_MEMBER];
} PinCountParallelShared;

typedef struct PinCountState
{
    CustomScanState css;
    PlanState *fallback;
    Relation heap;
    Relation index;
    IndexFetchTableData *fetch;
    TupleTableSlot *heap_slot;
    MemoryContext scratch;
    Buffer owner_buffer;
    Buffer vm_buffer;
    AttrNumber attribute;
    int eflags;
    Snapshot snapshot;
    bool done;
    const char *fallback_reason;
    uint64 stats[PIN_COUNT_STATS];
} PinCountState;

static Plan *pin_count_plan(PlannerInfo *, RelOptInfo *, CustomPath *, List *, List *, List *);
static Node *pin_count_state(CustomScan *);
static void pin_count_begin(CustomScanState *, EState *, int);
static TupleTableSlot *pin_count_next(CustomScanState *);
static void pin_count_end(CustomScanState *);
static void pin_count_rescan(CustomScanState *);
static void pin_count_release(PinCountState *);
static void pin_count_explain(CustomScanState *, List *, ExplainState *);
static bool pin_count_parallel_run(PinCountState *, const uint8 *, Size, int64 *);
static void pin_count_parallel_accumulate(PinCountParallelShared *, int64, const uint64 *);
static void pin_count_worker_open(PinCountState *, PinCountParallelShared *);
void pin_parallel_count_worker(void *, void *);

static const CustomPathMethods pin_count_path_methods = {
    .CustomName = "PinCount", .PlanCustomPath = pin_count_plan
};
static const CustomScanMethods pin_count_scan_methods = {
    .CustomName = "PinCount", .CreateCustomScanState = pin_count_state
};
static const CustomExecMethods pin_count_exec_methods = {
    .CustomName = "PinCount", .BeginCustomScan = pin_count_begin,
    .ExecCustomScan = pin_count_next, .EndCustomScan = pin_count_end,
    .ReScanCustomScan = pin_count_rescan, .ExplainCustomScan = pin_count_explain
};

static Const *
pin_oid_node(Oid oid)
{
    return makeConst(OIDOID, -1, InvalidOid, sizeof(Oid), ObjectIdGetDatum(oid), false, true);
}

static Oid
pin_private_oid(List *values, int position)
{
    Const *value = castNode(Const, list_nth(values, position));
    if (value->consttype != OIDOID || value->constisnull)
        elog(ERROR, "invalid PinCount relation identity");
    return DatumGetObjectId(value->constvalue);
}

static void
pin_count_upper(PlannerInfo *root, UpperRelationKind stage, RelOptInfo *input,
                RelOptInfo *output, void *extra)
{
    Query *query = root->parse;
    RelOptInfo *base;
    RangeTblEntry *rte;
    RangeTblRef *from;
    RestrictInfo *restriction;
    OpExpr *predicate;
    Var *column;
    Const *argument;
    Aggref *aggregate;
    AggPath *fallback = NULL;
    Oid schema, query_type, match, am;
    ListCell *cell;
    if (pin_previous_upper != NULL)
        pin_previous_upper(root, stage, input, output, extra);
    if (!pin_enable_count || stage != UPPERREL_GROUP_AGG || root->parent_root != NULL ||
        query->commandType != CMD_SELECT || !query->hasAggs || query->hasRowSecurity ||
        query->hasSubLinks || query->hasWindowFuncs || query->hasTargetSRFs ||
        query->groupClause != NIL || query->groupingSets != NIL || query->havingQual != NULL ||
        query->distinctClause != NIL || query->sortClause != NIL || query->rowMarks != NIL ||
        query->limitCount != NULL || query->limitOffset != NULL || query->setOperations != NULL ||
        query->cteList != NIL || query->jointree == NULL ||
        list_length(query->jointree->fromlist) != 1 || list_length(query->targetList) != 1 ||
        IsolationIsSerializable() || RecoveryInProgress())
        return;
    if (!IsA(linitial(query->jointree->fromlist), RangeTblRef) ||
        !IsA(((TargetEntry *) linitial(query->targetList))->expr, Aggref))
        return;
    aggregate = castNode(Aggref, ((TargetEntry *) linitial(query->targetList))->expr);
    if (!aggregate->aggstar || aggregate->args != NIL || aggregate->aggdirectargs != NIL ||
        aggregate->aggfilter != NULL || aggregate->aggdistinct != NIL ||
        aggregate->aggorder != NIL || aggregate->agglevelsup != 0 ||
        aggregate->aggtype != INT8OID || aggregate->aggsplit != AGGSPLIT_SIMPLE ||
        aggregate->aggfnoid != LookupFuncName(list_make2(makeString("pg_catalog"),
                                             makeString("count")), 0, NULL, true))
        return;
    from = castNode(RangeTblRef, linitial(query->jointree->fromlist));
    base = root->simple_rel_array[from->rtindex];
    rte = root->simple_rte_array[from->rtindex];
    if (base == NULL || base->reloptkind != RELOPT_BASEREL || rte->rtekind != RTE_RELATION ||
        rte->relkind != RELKIND_RELATION || rte->inh || rte->lateral ||
        rte->security_barrier || rte->securityQuals != NIL || rte->tablesample != NULL ||
        !bms_is_empty(base->lateral_relids) || list_length(base->baserestrictinfo) != 1)
        return;
    restriction = castNode(RestrictInfo, linitial(base->baserestrictinfo));
    if (restriction->pseudoconstant || restriction->security_level != 0 ||
        !IsA(restriction->clause, OpExpr))
        return;
    predicate = castNode(OpExpr, restriction->clause);
    if (list_length(predicate->args) != 2 || !IsA(linitial(predicate->args), Var) ||
        !IsA(lsecond(predicate->args), Const))
        return;
    column = castNode(Var, linitial(predicate->args));
    argument = castNode(Const, lsecond(predicate->args));
    if (column->varno != from->rtindex || column->varlevelsup != 0 ||
        column->varattno <= 0 || column->vartype != TEXTOID || argument->constisnull)
        return;
    schema = get_namespace_oid("pin", true);
    if (!OidIsValid(schema))
        return;
    query_type = GetSysCacheOid2(TYPENAMENSP, Anum_pg_type_oid,
                                CStringGetDatum("query"), ObjectIdGetDatum(schema));
    if (!OidIsValid(query_type) || argument->consttype != query_type)
        return;
    {
        bytea *encoded = DatumGetByteaPP(argument->constvalue);
        bool single_term = pin_count_single_term((const uint8 *) VARDATA_ANY(encoded),
                                                  VARSIZE_ANY_EXHDR(encoded));
        if ((Pointer) encoded != DatumGetPointer(argument->constvalue))
            pfree(encoded);
        if (!single_term)
            return;
    }
    match = OpernameGetOprid(list_make2(makeString("pin"), makeString("@@@")),
                            TEXTOID, query_type);
    am = get_am_oid("pin", true);
    if (!OidIsValid(am) || !OidIsValid(match) || predicate->opno != match)
        return;
    {
        Relation heap = table_open(rte->relid, NoLock);
        bool supported = heap->rd_tableam == GetHeapamTableAmRoutine() &&
            heap->rd_rel->relpersistence == RELPERSISTENCE_PERMANENT &&
            !heap->rd_rel->relrowsecurity;
        table_close(heap, NoLock);
        if (!supported)
            return;
    }
    /* retain a real core aggregate for cached-plan runtime fallback. */
    foreach(cell, output->pathlist)
    {
        Path *path = lfirst(cell);
        if (IsA(path, AggPath) && path->param_info == NULL &&
            ((AggPath *) path)->aggstrategy == AGG_PLAIN &&
            ((AggPath *) path)->aggsplit == AGGSPLIT_SIMPLE &&
            (fallback == NULL || path->disabled_nodes < fallback->path.disabled_nodes ||
             (path->disabled_nodes == fallback->path.disabled_nodes &&
              path->total_cost < fallback->path.total_cost)))
            fallback = (AggPath *) path;
    }
    if (fallback == NULL)
        return;
    foreach(cell, base->indexlist)
    {
        IndexOptInfo *index = lfirst_node(IndexOptInfo, cell);
        CustomPath *path;
        AggPath *saved;
        Path *scan = fallback->subpath;
        Cost saved_transition;
        if (!IsA(scan, BitmapHeapPath) ||
            !IsA(((BitmapHeapPath *) scan)->bitmapqual, IndexPath) ||
            ((IndexPath *) ((BitmapHeapPath *) scan)->bitmapqual)->indexinfo != index)
            continue;
        if (index->relam != am || index->ncolumns != 1 || index->nkeycolumns != 1 ||
            index->indexkeys[0] != column->varattno || index->indexprs != NIL ||
            index->indpred != NIL || index->hypothetical || index->opcintype[0] != TEXTOID ||
            index->indexcollations[0] != predicate->inputcollid ||
            get_opfamily_member(index->opfamily[0], TEXTOID, query_type, 1) != match)
            continue;
        /* add_path may free a dominated sibling; retain an independent shallow path. */
        saved = palloc(sizeof(AggPath));
        memcpy(saved, fallback, sizeof(AggPath));
        path = makeNode(CustomPath);
        path->path.pathtype = T_CustomScan;
        path->path.parent = output;
        path->path.pathtarget = output->reltarget;
        path->path.param_info = NULL;
        path->path.parallel_aware = false;
        path->path.parallel_safe = false;
        path->path.parallel_workers = 0;
        path->path.rows = 1;
        path->path.pathkeys = NIL;
        path->path.disabled_nodes = saved->path.disabled_nodes;
        /* retain baseline heap/index cost; credit only the omitted aggregate transition. */
        saved_transition = cpu_operator_cost * base->rows;
        path->path.total_cost = Max(cpu_operator_cost,
                                    saved->path.total_cost - saved_transition) +
                                8 * cpu_operator_cost;
        path->path.startup_cost = path->path.total_cost;
        path->flags = CUSTOMPATH_SUPPORT_PROJECTION;
        path->custom_paths = list_make1(saved);
        path->custom_private = list_make5(pin_oid_node(rte->relid), pin_oid_node(index->indexoid),
                                          makeInteger(column->varattno), copyObjectImpl(argument),
                                          pin_oid_node(predicate->inputcollid));
        path->methods = &pin_count_path_methods;
        add_path(output, &path->path);
        /* the original fallback may have been freed by add_path. */
        break;
    }
}

void
pin_count_init(Size participant_memory)
{
    if (participant_memory == 0)
        elog(ERROR, "invalid PinCount participant memory");
    pin_count_participant_memory = participant_memory;
    DefineCustomBoolVariable("pin.enable_count_fastpath", "Enable experimental direct counts.",
                             "Off until count qualification is complete.",
                             &pin_enable_count, false, PGC_SUSET, 0, NULL, NULL, NULL);
    DefineCustomBoolVariable("pin.enable_count_vm", "Enable experimental count VM certification.",
                             "Uncertified candidates always use heap visibility.",
                             &pin_enable_count_vm, false, PGC_SUSET, 0, NULL, NULL, NULL);
    DefineCustomIntVariable("pin.parallel_count_workers",
                            "Maximum PostgreSQL workers for experimental PinCount.",
                            "Zero keeps direct count execution serial.",
                            &pin_count_parallel_workers, 0, 0, 64, PGC_SUSET,
                            GUC_NOT_IN_SAMPLE, NULL, NULL, NULL);
    RegisterCustomScanMethods(&pin_count_scan_methods);
    pin_previous_upper = create_upper_paths_hook;
    create_upper_paths_hook = pin_count_upper;
}

static Plan *
pin_count_plan(PlannerInfo *root, RelOptInfo *rel, CustomPath *path,
                List *target, List *clauses, List *children)
{
    CustomScan *scan = makeNode(CustomScan);
    Const *index = castNode(Const, list_nth(path->custom_private, 1));
    (void) rel;
    (void) clauses;
    if (list_length(target) != 1 || list_length(children) != 1 ||
        list_length(path->custom_private) != 5 || clauses != NIL)
        elog(ERROR, "unsupported PinCount plan target");
    scan->scan.plan.targetlist = target;
    scan->custom_scan_tlist = (List *) copyObjectImpl(target);
    scan->custom_plans = children;
    scan->custom_private = list_make4(
        copyObjectImpl(linitial(path->custom_private)),
        copyObjectImpl(index),
        copyObjectImpl(lthird(path->custom_private)),
        copyObjectImpl(list_nth(path->custom_private, 4)));
    scan->custom_exprs = list_make1(copyObjectImpl(list_nth(path->custom_private, 3)));
    scan->flags = path->flags;
    scan->methods = &pin_count_scan_methods;
    if (!list_member_oid(root->glob->relationOids, DatumGetObjectId(index->constvalue)))
        root->glob->relationOids = lappend_oid(root->glob->relationOids,
                                               DatumGetObjectId(index->constvalue));
    return &scan->scan.plan;
}

static Node *
pin_count_state(CustomScan *scan)
{
    PinCountState *state = palloc0(sizeof(PinCountState));
    (void) scan;
    NodeSetTag(&state->css, T_CustomScanState);
    state->css.methods = &pin_count_exec_methods;
    state->owner_buffer = InvalidBuffer;
    state->vm_buffer = InvalidBuffer;
    return (Node *) state;
}

static void
pin_count_begin(CustomScanState *node, EState *estate, int flags)
{
    PinCountState *state = (PinCountState *) node;
    CustomScan *scan = castNode(CustomScan, node->ss.ps.plan);
    if (list_length(scan->custom_plans) != 1 || list_length(scan->custom_private) != 4 ||
        list_length(scan->custom_exprs) != 1)
        elog(ERROR, "invalid PinCount plan");
    state->eflags = flags;
    state->attribute = intVal(list_nth(scan->custom_private, 2));
    state->fallback = ExecInitNode((Plan *) linitial(scan->custom_plans), estate, flags);
    node->custom_ps = list_make1(state->fallback);
}

static bool
pin_count_open(PinCountState *state)
{
    CustomScan *scan = castNode(CustomScan, state->css.ss.ps.plan);
    EState *estate = state->css.ss.ps.state;
    if (!pin_enable_count)
        state->fallback_reason = "disabled at execution";
    else if (IsolationIsSerializable())
        state->fallback_reason = "serializable snapshot";
    else if (RecoveryInProgress())
        state->fallback_reason = "recovery";
    else if (estate->es_snapshot == NULL || !IsMVCCSnapshot(estate->es_snapshot))
        state->fallback_reason = "non-MVCC snapshot";
    else if ((state->eflags & (EXEC_FLAG_BACKWARD | EXEC_FLAG_MARK)) != 0)
        state->fallback_reason = "cursor capability";
    if (state->fallback_reason != NULL)
        return false;
    state->heap = table_open(pin_private_oid(scan->custom_private, 0), AccessShareLock);
    if (state->heap->rd_rel->relrowsecurity ||
        state->heap->rd_tableam != GetHeapamTableAmRoutine() ||
        state->heap->rd_rel->relpersistence != RELPERSISTENCE_PERMANENT)
    {
        state->fallback_reason = "heap or security eligibility";
        pin_count_release(state);
        return false;
    }
    state->index = index_open(pin_private_oid(scan->custom_private, 1), AccessShareLock);
    if (state->index->rd_rel->relam != get_am_oid("pin", false) ||
        state->index->rd_index == NULL || !state->index->rd_index->indisvalid ||
        !state->index->rd_index->indisready || !state->index->rd_index->indislive ||
        state->index->rd_index->indrelid != RelationGetRelid(state->heap) ||
        state->index->rd_index->indnatts != 1 || state->index->rd_index->indnkeyatts != 1 ||
        state->attribute <= 0 || state->attribute > RelationGetDescr(state->heap)->natts ||
        TupleDescAttr(RelationGetDescr(state->heap), state->attribute - 1)->attisdropped ||
        TupleDescAttr(RelationGetDescr(state->heap), state->attribute - 1)->atttypid != TEXTOID ||
        state->index->rd_index->indkey.values[0] != state->attribute ||
        state->index->rd_indcollation[0] !=
            pin_private_oid(scan->custom_private, 3) ||
        RelationGetIndexPredicate(state->index) != NIL ||
        RelationGetIndexExpressions(state->index) != NIL ||
        get_opfamily_member(state->index->rd_opfamily[0], TEXTOID,
                            ((Const *) linitial(scan->custom_exprs))->consttype, 1) !=
            OpernameGetOprid(list_make2(makeString("pin"), makeString("@@@")),
                             TEXTOID, ((Const *) linitial(scan->custom_exprs))->consttype) ||
        (state->index->rd_index->indcheckxmin &&
         !TransactionIdPrecedes(
             HeapTupleHeaderGetXmin(state->index->rd_indextuple->t_data), TransactionXmin)))
    {
        state->fallback_reason = "index eligibility changed";
        pin_count_release(state);
        return false;
    }
    pin_storage_check(state->index, state->heap, NULL);
    state->snapshot = estate->es_snapshot;
    state->fetch = table_index_fetch_begin(state->heap);
    state->heap_slot = table_slot_create(state->heap, NULL);
    state->scratch = AllocSetContextCreate(estate->es_query_cxt, "PinCount tuple", ALLOCSET_SMALL_SIZES);
    return true;
}

static TupleTableSlot *
pin_count_next(CustomScanState *node)
{
    PinCountState *state = (PinCountState *) node;
    TupleTableSlot *slot = node->ss.ss_ScanTupleSlot;
    CustomScan *scan = castNode(CustomScan, node->ss.ps.plan);
    Const *query;
    bytea *bytes;
    int64 count;
    if (state->fallback_reason != NULL)
        return ExecProcNode(state->fallback);
    if (state->done)
        return ExecClearTuple(slot);
    if (!pin_count_open(state))
        return ExecProcNode(state->fallback);
    query = linitial_node(Const, scan->custom_exprs);
    bytes = DatumGetByteaPP(query->constvalue);
    if (!pin_count_parallel_run(state, (const uint8 *) VARDATA_ANY(bytes),
                                VARSIZE_ANY_EXHDR(bytes), &count))
        count = pin_count_execute(state->index, state, (const uint8 *) VARDATA_ANY(bytes),
                                  VARSIZE_ANY_EXHDR(bytes), state->stats);
    if ((Pointer) bytes != DatumGetPointer(query->constvalue))
        pfree(bytes);
    state->done = true;
    pin_count_release(state);
    ExecClearTuple(slot);
    slot->tts_values[0] = Int64GetDatum(count);
    slot->tts_isnull[0] = false;
    ExecStoreVirtualTuple(slot);
    if (node->ss.ps.ps_ProjInfo != NULL)
    {
        node->ss.ps.ps_ExprContext->ecxt_scantuple = slot;
        return ExecProject(node->ss.ps.ps_ProjInfo);
    }
    return slot;
}

static void
pin_count_release(PinCountState *state)
{
    pin_count_owner_unlock(state);
    if (BufferIsValid(state->vm_buffer))
        ReleaseBuffer(state->vm_buffer);
    state->vm_buffer = InvalidBuffer;
    if (state->heap_slot != NULL)
        ExecDropSingleTupleTableSlot(state->heap_slot);
    state->heap_slot = NULL;
    if (state->fetch != NULL)
        table_index_fetch_end(state->fetch);
    state->fetch = NULL;
    if (state->scratch != NULL)
        MemoryContextDelete(state->scratch);
    state->scratch = NULL;
    state->snapshot = NULL;
    if (state->index != NULL)
        index_close(state->index, AccessShareLock);
    state->index = NULL;
    if (state->heap != NULL)
        table_close(state->heap, AccessShareLock);
    state->heap = NULL;
}

static void
pin_count_end(CustomScanState *node)
{
    PinCountState *state = (PinCountState *) node;
    pin_count_release(state);
    if (state->fallback != NULL)
        ExecEndNode(state->fallback);
    state->fallback = NULL;
    ExecClearTuple(node->ss.ss_ScanTupleSlot);
}

static void
pin_count_rescan(CustomScanState *node)
{
    PinCountState *state = (PinCountState *) node;
    pin_count_release(state);
    state->done = false;
    state->fallback_reason = NULL;
    ExecReScan(state->fallback);
    ExecClearTuple(node->ss.ss_ScanTupleSlot);
}

static void
pin_count_explain(CustomScanState *node, List *ancestors, ExplainState *es)
{
    PinCountState *state = (PinCountState *) node;
    static const char *names[PIN_COUNT_STATS] = {
        "Candidate Owners", "Owner Lock Batches", "Liveness Rejects", "VM Probes",
        "VM Certified Roots", "Heap Fetches", "Heap Matches", "Uncertified Source Roots"
    };
    (void) ancestors;
    ExplainPropertyText("Certification", "sealed exact term with protected canonical owner", es);
    if (state->fallback_reason != NULL)
        ExplainPropertyText("Fallback", state->fallback_reason, es);
    if (es->analyze)
        for (int i = 0; i < PIN_COUNT_STATS; i++)
            ExplainPropertyUInteger(names[i], NULL, state->stats[i], es);
}

uint32
pin_count_owner_lock(void *context, uint32 block, uint8 *out, uint32 capacity)
{
    PinCountState *state = context;
    if (BufferIsValid(state->owner_buffer))
        elog(ERROR, "PinCount already holds an owner buffer");
    return pin_storage_owner_read(state->index, block, out, capacity, &state->owner_buffer);
}

void
pin_count_owner_unlock(void *context)
{
    PinCountState *state = context;
    if (BufferIsValid(state->owner_buffer))
        ReleaseBuffer(state->owner_buffer);
    state->owner_buffer = InvalidBuffer;
}

bool
pin_count_all_visible(void *context, uint32 block)
{
    PinCountState *state = context;
    if (!BufferIsValid(state->owner_buffer))
        elog(ERROR, "PinCount VM check requires owner protection");
    if (!pin_enable_count_vm)
        return false;
    return (visibilitymap_get_status(state->heap, block, &state->vm_buffer) &
            VISIBILITYMAP_ALL_VISIBLE) != 0;
}

bool
pin_count_fetch(void *context, uint32 block, uint16 offset, const uint8 **bytes, Size *length)
{
    PinCountState *state = context;
    ItemPointerData visible;
    MemoryContext previous;
    bool again = false, isnull;
    Datum value;
    text *body;
    *bytes = NULL;
    *length = 0;
    if (!BufferIsValid(state->owner_buffer) || block == InvalidBlockNumber ||
        offset == InvalidOffsetNumber || offset > MaxHeapTuplesPerPage)
        elog(ERROR, "invalid PinCount heap fetch");
    CHECK_FOR_INTERRUPTS();
    ItemPointerSet(&visible, block, offset);
    if (state->snapshot == NULL ||
        !table_index_fetch_tuple(state->fetch, &visible, state->snapshot,
                                 state->heap_slot, &again, NULL))
        return false;
    if (again)
        elog(ERROR, "PinCount requires one MVCC-visible HOT version");
    previous = MemoryContextSwitchTo(state->scratch);
    value = slot_getattr(state->heap_slot, state->attribute, &isnull);
    if (!isnull)
    {
        body = DatumGetTextPP(value);
        *bytes = (const uint8 *) VARDATA_ANY(body);
        *length = VARSIZE_ANY_EXHDR(body);
    }
    MemoryContextSwitchTo(previous);
    return !isnull;
}

void
pin_count_clear(void *context)
{
    PinCountState *state = context;
    ExecClearTuple(state->heap_slot);
    table_index_fetch_reset(state->fetch);
    MemoryContextReset(state->scratch);
}


void
pin_count_work_snapshot(void *shared_ptr, uint64 *words, uint32 count)
{
    PinCountParallelShared *shared = shared_ptr;
    if (shared == NULL || words == NULL || count != PIN_COUNT_WORK_WORDS)
        elog(ERROR, "invalid PinCount parallel work snapshot");
    SpinLockAcquire(&shared->mutex);
    memcpy(words, shared->work, sizeof(shared->work));
    SpinLockRelease(&shared->mutex);
}

bool
pin_count_work_claim(void *shared_ptr, const uint64 *expected,
                     const uint64 *next, uint32 count)
{
    PinCountParallelShared *shared = shared_ptr;
    bool claimed = false;
    if (shared == NULL || expected == NULL || next == NULL ||
        count != PIN_COUNT_WORK_WORDS)
        elog(ERROR, "invalid PinCount parallel work claim");
    SpinLockAcquire(&shared->mutex);
    if (memcmp(shared->work, expected, sizeof(shared->work)) == 0)
    {
        memcpy(shared->work, next, sizeof(shared->work));
        claimed = true;
    }
    SpinLockRelease(&shared->mutex);
    return claimed;
}

static void
pin_count_parallel_accumulate(PinCountParallelShared *shared, int64 count,
                              const uint64 *stats)
{
    int64 next_count;
    uint64 next_stats[PIN_COUNT_STATS];

    if (count < 0 || stats == NULL)
        elog(ERROR, "invalid PinCount parallel result");
    SpinLockAcquire(&shared->mutex);
    if (pg_add_s64_overflow(shared->count, count, &next_count))
    {
        SpinLockRelease(&shared->mutex);
        ereport(ERROR, (errcode(ERRCODE_PROGRAM_LIMIT_EXCEEDED),
                        errmsg("Pin parallel count overflow")));
    }
    for (int i = 0; i < PIN_COUNT_STATS; i++)
    {
        if (pg_add_u64_overflow(shared->stats[i], stats[i], &next_stats[i]))
        {
            SpinLockRelease(&shared->mutex);
            ereport(ERROR, (errcode(ERRCODE_PROGRAM_LIMIT_EXCEEDED),
                            errmsg("Pin parallel count instrumentation overflow")));
        }
    }
    shared->count = next_count;
    memcpy(shared->stats, next_stats, sizeof(next_stats));
    SpinLockRelease(&shared->mutex);
}

static void
pin_count_worker_open(PinCountState *state, PinCountParallelShared *shared)
{
    memset(state, 0, sizeof(*state));
    state->owner_buffer = InvalidBuffer;
    state->vm_buffer = InvalidBuffer;
    state->attribute = shared->attribute;
    state->snapshot = GetActiveSnapshot();
    if (state->snapshot == NULL || !IsMVCCSnapshot(state->snapshot))
        ereport(ERROR, (errcode(ERRCODE_FEATURE_NOT_SUPPORTED),
                        errmsg("Pin parallel count requires an MVCC snapshot")));

    state->heap = table_open(shared->heaprelid, AccessShareLock);
    state->index = index_open(shared->indexrelid, AccessShareLock);
    if (state->index->rd_index == NULL ||
        state->index->rd_index->indrelid != RelationGetRelid(state->heap) ||
        state->attribute <= 0 ||
        state->attribute > RelationGetDescr(state->heap)->natts)
        elog(ERROR, "invalid PinCount parallel relation state");
    pin_storage_check(state->index, state->heap, NULL);
    state->fetch = table_index_fetch_begin(state->heap);
    state->heap_slot = table_slot_create(state->heap, NULL);
    state->scratch = AllocSetContextCreate(CurrentMemoryContext,
                                           "PinCount parallel tuple",
                                           ALLOCSET_SMALL_SIZES);
}

static bool
pin_count_parallel_run(PinCountState *state, const uint8 *query, Size length,
                       int64 *result)
{
    ParallelContext *pcxt;
    PinCountParallelShared *shared;
    Size shared_size;
    uint64 work[PIN_COUNT_WORK_WORDS];
    uint64 local_stats[PIN_COUNT_STATS] = {0};
    int request;
    int max_participants;
    int64 local_count;

    if (pin_count_participant_memory == 0 || IsParallelWorker() || IsInParallelMode())
        return false;
    shared_size = add_size(offsetof(PinCountParallelShared, query), length);
    {
        Size budget = mul_size((Size) work_mem, (Size) 1024);

        if (shared_size >= budget)
            return false;
        max_participants = (int) ((budget - shared_size) /
                                  pin_count_participant_memory);
    }
    if (max_participants <= 1)
        return false;
    request = Min(pin_count_parallel_workers, max_parallel_workers_per_gather);
    request = Min(request, max_participants - 1);
    if (request <= 0)
        return false;

    pin_structure_lock(state->index, false);
    pin_count_parallel_capture(state->index, query, length, work);

    EnterParallelMode();
    pcxt = CreateParallelContext("$libdir/pin", "pin_parallel_count_main", request);
    shm_toc_estimate_chunk(&pcxt->estimator, shared_size);
    shm_toc_estimate_keys(&pcxt->estimator, 1);
    InitializeParallelDSM(pcxt);
    if (pcxt->seg == NULL)
    {
        DestroyParallelContext(pcxt);
        ExitParallelMode();
        pin_structure_unlock(state->index, false);
        return false;
    }

    shared = shm_toc_allocate(pcxt->toc, shared_size);
    shared->heaprelid = RelationGetRelid(state->heap);
    shared->indexrelid = RelationGetRelid(state->index);
    shared->attribute = state->attribute;
    shared->query_length = length;
    SpinLockInit(&shared->mutex);
    memcpy(shared->work, work, sizeof(work));
    shared->count = 0;
    memset(shared->stats, 0, sizeof(shared->stats));
    memcpy(shared->query, query, length);
    shm_toc_insert(pcxt->toc, PIN_COUNT_KEY_SHARED, shared);

    LaunchParallelWorkers(pcxt);
    if (pcxt->nworkers_launched == 0)
    {
        DestroyParallelContext(pcxt);
        ExitParallelMode();
        pin_structure_unlock(state->index, false);
        return false;
    }

    local_count = pin_count_parallel_execute(state->index, state, shared,
                                             (const uint8 *) shared->query,
                                             shared->query_length, local_stats);
    pin_count_parallel_accumulate(shared, local_count, local_stats);
    WaitForParallelWorkersToAttach(pcxt);
    WaitForParallelWorkersToFinish(pcxt);

    SpinLockAcquire(&shared->mutex);
    *result = shared->count;
    memcpy(state->stats, shared->stats, sizeof(state->stats));
    SpinLockRelease(&shared->mutex);

    DestroyParallelContext(pcxt);
    ExitParallelMode();
    pin_structure_unlock(state->index, false);
    return true;
}

void
pin_parallel_count_worker(void *segment, void *table)
{
    dsm_segment *seg = segment;
    shm_toc *toc = table;
    PinCountParallelShared *shared;
    PinCountState state;
    uint64 stats[PIN_COUNT_STATS] = {0};
    int64 count;

    (void) seg;
    shared = shm_toc_lookup(toc, PIN_COUNT_KEY_SHARED, false);
    pin_count_worker_open(&state, shared);
#ifdef PIN_TEST_HOOKS
    pin_parallel_test_event(17);
#endif
    pin_structure_lock(state.index, false);
    count = pin_count_parallel_execute(state.index, &state, shared,
                                       (const uint8 *) shared->query,
                                       shared->query_length, stats);
    pin_count_parallel_accumulate(shared, count, stats);
    pin_structure_unlock(state.index, false);
    pin_count_release(&state);
}
