/* core schedules whole indexes; Pin keeps callback-local maintenance state.
 * contracts: pg18.6 commands/vacuum.h and commands/vacuumparallel.c.
 */
#include "postgres.h"
#include "pin_parallel.h"
#include "pin_storage.h"
#include "access/parallel.h"
#include "access/table.h"
#include "access/tableam.h"
#include "catalog/index.h"
#include "commands/vacuum.h"
#include "common/int.h"
#include "executor/instrument.h"
#include "miscadmin.h"
#include "pgstat.h"
#include "storage/bufmgr.h"
#include "storage/spin.h"
#include "storage/lwlock.h"
#include "tcop/tcopprot.h"
#include "utils/guc.h"
#include "utils/snapmgr.h"
#include <stdint.h>
#ifdef PIN_TEST_HOOKS
#include "fmgr.h"
#include "utils/fmgrprotos.h"
#endif

#define PIN_PARALLEL_VACUUM_OPTIONS \
    (VACUUM_OPTION_PARALLEL_BULKDEL | VACUUM_OPTION_PARALLEL_CLEANUP)

StaticAssertDecl(PIN_PARALLEL_VACUUM_OPTIONS <= UINT8_MAX,
                 "parallel vacuum options exceed the AM field");
StaticAssertDecl((PIN_PARALLEL_VACUUM_OPTIONS & ~VACUUM_OPTION_MAX_VALID_VALUE) == 0,
                 "parallel vacuum options contain unsupported flags");


#define PIN_BUILD_KEY_SHARED UINT64CONST(1)
#define PIN_BUILD_KEY_QUERY UINT64CONST(2)
#define PIN_BUILD_KEY_WAL UINT64CONST(3)
#define PIN_BUILD_KEY_BUFFER UINT64CONST(4)

typedef struct PinBuildShared
{
    Oid heaprelid;
    Oid indexrelid;
    uint64 prepare_memory;
    int writer_tranche;
    LWLock writer_lock;
    slock_t mutex;
    double heap_tuples;
    uint64 index_tuples;
    bool broken_hot_chain;
} PinBuildShared;

typedef struct PinBuildLocal
{
    Relation heap;
    uint64 prepare_memory;
    LWLock *writer_lock;
    uint64 index_tuples;
} PinBuildLocal;

#define PIN_BUILD_SCAN(shared) \
    ((ParallelTableScanDesc) ((char *) (shared) + BUFFERALIGN(sizeof(PinBuildShared))))

extern bool pin_parallel_build_tuple(Relation index, Relation heap, ItemPointer tid,
                                     Datum *values, bool *nulls, uint64 memory_bytes,
                                     void *writer_lock);

static Size pin_parallel_build_shared_size(Relation heap);
static void pin_parallel_build_scan(PinBuildShared *shared, Relation heap,
                                    Relation index, bool progress);
static void pin_parallel_build_callback(Relation index, ItemPointer tid,
                                        Datum *values, bool *nulls,
                                        bool tuple_is_alive, void *state);
PGDLLEXPORT void pin_parallel_build_main(dsm_segment *seg, shm_toc *toc);

static bool pin_enable_parallel_vacuum = false;
static int pin_build_tranche_id = -1;
#ifdef PIN_TEST_HOOKS
static int pin_pause_worker_stage = 0;
#endif

void
pin_parallel_init(void)
{
    /* relcache handlers and workers must see one immutable capability mask. */
    DefineCustomBoolVariable("pin.enable_parallel_vacuum",
                             "Enables experimental PostgreSQL-managed parallel Pin VACUUM.",
                             "Parallelism is across indexes, not within one index.",
                             &pin_enable_parallel_vacuum, false, PGC_POSTMASTER, 0,
                             NULL, NULL, NULL);
#ifdef PIN_TEST_HOOKS
    /* core serializes this test-only GUC into worker startup state. */
    DefineCustomIntVariable("pin.g7_pause_worker_stage",
                            "Pauses a test worker at a storage transition.",
                            "Disposable test clusters only; the driver holds advisory key (180006, 4).",
                            &pin_pause_worker_stage, 0, 0, 18, PGC_SUSET,
                            GUC_NOT_IN_SAMPLE, NULL, NULL, NULL);
#endif
}

uint8
pin_parallel_vacuum_options(void)
{
    /* cleanup also compacts after bulk deletion, so conditional cleanup is wrong. */
    return pin_enable_parallel_vacuum ? PIN_PARALLEL_VACUUM_OPTIONS :
                                       VACUUM_OPTION_NO_PARALLEL;
}

#ifdef PIN_TEST_HOOKS
void
pin_parallel_test_event(uint8 stage)
{
    /* never run SPI or acquire a session lock inside a parallel worker. */
    if (IsParallelWorker() &&
        ((stage >= 7 && stage <= 12) || stage == 16 || stage == 17 || stage == 18) &&
        pin_pause_worker_stage == stage)
        (void) DirectFunctionCall2(pg_advisory_xact_lock_int4,
                                   Int32GetDatum(180006), Int32GetDatum(4));
}
#endif


static int
pin_parallel_build_tranche(void)
{
    if (pin_build_tranche_id < 0)
        pin_build_tranche_id = LWLockNewTrancheId();
    LWLockRegisterTranche(pin_build_tranche_id, "PinParallelBuild");
    return pin_build_tranche_id;
}

void
pin_parallel_build_writer_lock(void *lock)
{
    if (lock == NULL)
        elog(ERROR, "invalid Pin parallel build lock");
    LWLockAcquire((LWLock *) lock, LW_EXCLUSIVE);
}

void
pin_parallel_build_writer_unlock(void *lock)
{
    if (lock == NULL || !LWLockHeldByMeInMode((LWLock *) lock, LW_EXCLUSIVE))
        elog(ERROR, "invalid Pin parallel build unlock");
    LWLockRelease((LWLock *) lock);
}

static Size
pin_parallel_build_shared_size(Relation heap)
{
    return add_size(BUFFERALIGN(sizeof(PinBuildShared)),
                    table_parallelscan_estimate(heap, SnapshotAny));
}

static void
pin_parallel_build_callback(Relation index, ItemPointer tid, Datum *values,
                            bool *nulls, bool tuple_is_alive, void *state)
{
    PinBuildLocal *local = state;
    (void) tuple_is_alive;
#ifdef PIN_TEST_HOOKS
    pin_parallel_test_event(16);
#endif
    if (pin_parallel_build_tuple(index, local->heap, tid, values, nulls,
                                 local->prepare_memory, local->writer_lock) &&
        pg_add_u64_overflow(local->index_tuples, 1, &local->index_tuples))
        ereport(ERROR, (errcode(ERRCODE_PROGRAM_LIMIT_EXCEEDED),
                        errmsg("Pin parallel build tuple count overflow")));
}

static void
pin_parallel_build_scan(PinBuildShared *shared, Relation heap, Relation index,
                        bool progress)
{
    PinBuildLocal local = {
        .heap = heap,
        .prepare_memory = shared->prepare_memory,
        .writer_lock = &shared->writer_lock,
        .index_tuples = 0
    };
    IndexInfo *info;
    TableScanDesc scan;
    double heap_tuples;

    LWLockRegisterTranche(shared->writer_tranche, "PinParallelBuild");
    info = BuildIndexInfo(index);
    scan = table_beginscan_parallel(heap, PIN_BUILD_SCAN(shared));

    info->ii_Concurrent = false;
    heap_tuples = table_index_build_scan(heap, index, info, true, progress,
                                         pin_parallel_build_callback, &local, scan);

    SpinLockAcquire(&shared->mutex);
    if (pg_add_u64_overflow(shared->index_tuples, local.index_tuples,
                            &local.index_tuples))
    {
        SpinLockRelease(&shared->mutex);
        ereport(ERROR, (errcode(ERRCODE_PROGRAM_LIMIT_EXCEEDED),
                        errmsg("Pin parallel build tuple count overflow")));
    }
    shared->heap_tuples += heap_tuples;
    shared->index_tuples = local.index_tuples;
    if (info->ii_BrokenHotChain)
        shared->broken_hot_chain = true;
    SpinLockRelease(&shared->mutex);
}

bool
pin_parallel_build(Relation heap, Relation index, struct IndexInfo *info,
                   uint64 prepare_memory, uint64 participant_memory,
                   double *heap_tuples, uint64 *index_tuples)
{
    ParallelContext *pcxt;
    PinBuildShared *shared;
    WalUsage *walusage;
    BufferUsage *bufferusage;
    Snapshot snapshot = SnapshotAny;
    Size shared_size;
    Size total_memory;
    int request;
    int max_participants;
    int querylen = 0;

    if (heap == NULL || index == NULL || info == NULL || heap_tuples == NULL ||
        index_tuples == NULL || prepare_memory == 0 || participant_memory == 0 ||
        info->ii_Concurrent || info->ii_ParallelWorkers <= 0)
        return false;

    total_memory = mul_size((Size) maintenance_work_mem, (Size) 1024);
    max_participants = (int) (total_memory / participant_memory);
    if (max_participants <= 1)
        return false;
    request = Min(info->ii_ParallelWorkers, max_participants - 1);
    if (request <= 0)
        return false;

    EnterParallelMode();
    pcxt = CreateParallelContext("$libdir/pin", "pin_parallel_build_main", request);
    shared_size = pin_parallel_build_shared_size(heap);
    shm_toc_estimate_chunk(&pcxt->estimator, shared_size);
    shm_toc_estimate_keys(&pcxt->estimator, 1);

    shm_toc_estimate_chunk(&pcxt->estimator,
                           mul_size(sizeof(WalUsage), pcxt->nworkers));
    shm_toc_estimate_keys(&pcxt->estimator, 1);
    shm_toc_estimate_chunk(&pcxt->estimator,
                           mul_size(sizeof(BufferUsage), pcxt->nworkers));
    shm_toc_estimate_keys(&pcxt->estimator, 1);

    if (debug_query_string != NULL)
    {
        querylen = strlen(debug_query_string);
        shm_toc_estimate_chunk(&pcxt->estimator, querylen + 1);
        shm_toc_estimate_keys(&pcxt->estimator, 1);
    }

    InitializeParallelDSM(pcxt);
    if (pcxt->seg == NULL)
    {
        DestroyParallelContext(pcxt);
        ExitParallelMode();
        return false;
    }

    shared = shm_toc_allocate(pcxt->toc, shared_size);
    shared->heaprelid = RelationGetRelid(heap);
    shared->indexrelid = RelationGetRelid(index);
    shared->prepare_memory = prepare_memory;
    shared->writer_tranche = pin_parallel_build_tranche();
    LWLockInitialize(&shared->writer_lock, shared->writer_tranche);
    SpinLockInit(&shared->mutex);
    shared->heap_tuples = 0;
    shared->index_tuples = 0;
    shared->broken_hot_chain = false;
    table_parallelscan_initialize(heap, PIN_BUILD_SCAN(shared), snapshot);
    shm_toc_insert(pcxt->toc, PIN_BUILD_KEY_SHARED, shared);

    if (debug_query_string != NULL)
    {
        char *query = shm_toc_allocate(pcxt->toc, querylen + 1);
        memcpy(query, debug_query_string, querylen + 1);
        shm_toc_insert(pcxt->toc, PIN_BUILD_KEY_QUERY, query);
    }

    walusage = shm_toc_allocate(pcxt->toc,
                                mul_size(sizeof(WalUsage), pcxt->nworkers));
    memset(walusage, 0, mul_size(sizeof(WalUsage), pcxt->nworkers));
    shm_toc_insert(pcxt->toc, PIN_BUILD_KEY_WAL, walusage);
    bufferusage = shm_toc_allocate(pcxt->toc,
                                   mul_size(sizeof(BufferUsage), pcxt->nworkers));
    memset(bufferusage, 0, mul_size(sizeof(BufferUsage), pcxt->nworkers));
    shm_toc_insert(pcxt->toc, PIN_BUILD_KEY_BUFFER, bufferusage);

    LaunchParallelWorkers(pcxt);
    if (pcxt->nworkers_launched == 0)
    {
        DestroyParallelContext(pcxt);
        ExitParallelMode();
        return false;
    }

    pin_parallel_build_scan(shared, heap, index, true);
    WaitForParallelWorkersToAttach(pcxt);
    WaitForParallelWorkersToFinish(pcxt);

    for (int i = 0; i < pcxt->nworkers_launched; i++)
        InstrAccumParallelQuery(&bufferusage[i], &walusage[i]);

    SpinLockAcquire(&shared->mutex);
    *heap_tuples = shared->heap_tuples;
    *index_tuples = shared->index_tuples;
    if (shared->broken_hot_chain)
        info->ii_BrokenHotChain = true;
    SpinLockRelease(&shared->mutex);

    DestroyParallelContext(pcxt);
    ExitParallelMode();
    return true;
}

void
pin_parallel_build_main(dsm_segment *seg, shm_toc *toc)
{
    PinBuildShared *shared;
    Relation heap;
    Relation index;
    WalUsage *walusage;
    BufferUsage *bufferusage;
    char *query;

    (void) seg;
    shared = shm_toc_lookup(toc, PIN_BUILD_KEY_SHARED, false);
    query = shm_toc_lookup(toc, PIN_BUILD_KEY_QUERY, true);
    debug_query_string = query;
    if (query != NULL)
        pgstat_report_activity(STATE_RUNNING, query);

    heap = table_open(shared->heaprelid, ShareLock);
    index = index_open(shared->indexrelid, AccessExclusiveLock);
    pin_storage_check(index, heap, NULL);

    InstrStartParallelQuery();
    pin_parallel_build_scan(shared, heap, index, false);
    bufferusage = shm_toc_lookup(toc, PIN_BUILD_KEY_BUFFER, false);
    walusage = shm_toc_lookup(toc, PIN_BUILD_KEY_WAL, false);
    InstrEndParallelQuery(&bufferusage[ParallelWorkerNumber],
                          &walusage[ParallelWorkerNumber]);

    index_close(index, AccessExclusiveLock);
    table_close(heap, ShareLock);
}
