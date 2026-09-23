/* bounded, forward-only sorting through the pinned postgres datum-sort api. */
#include "postgres.h"
#include "pin_grouped.h"
#include "catalog/pg_operator_d.h"
#include "catalog/pg_type_d.h"
#include "miscadmin.h"
#include "postmaster/autovacuum.h"
#include "utils/tuplesort.h"
#include "varatt.h"
#include <limits.h>

#define PIN_GROUP_RECORD_BYTES 32
#define PIN_GROUP_SORT_BATCH 256
#define PIN_GROUP_MIN_SORT_KB 64

typedef struct PinGroupSort
{
    Tuplesortstate *state;
    bool sorted;
} PinGroupSort;

typedef struct PinGroupDatum
{
    int32 header;
    uint8 bytes[PIN_GROUP_RECORD_BYTES];
} PinGroupDatum;

StaticAssertDecl(sizeof(PinGroupDatum) == VARHDRSZ + PIN_GROUP_RECORD_BYTES,
                 "group sort datum must not contain padding");

void *
pin_group_sort_begin(uint64 reserved_bytes)
{
    PinGroupSort *sort;
    int budget_kb = maintenance_work_mem;
    uint64 reserved_kb;

    if (AmAutoVacuumWorkerProcess() && autovacuum_work_mem >= 0)
        budget_kb = autovacuum_work_mem;
    if (budget_kb <= 0 || reserved_bytes > (uint64) INT_MAX * 1024)
        return NULL;
    reserved_kb = (reserved_bytes + 1023) / 1024;
    if ((uint64) budget_kb < reserved_kb + PIN_GROUP_MIN_SORT_KB)
        return NULL;

    /* caller context and resource owner reclaim memory and tapes on error. */
    sort = palloc0(sizeof(*sort));
    sort->state = tuplesort_begin_datum(BYTEAOID, ByteaLessOperator,
                                       InvalidOid, false,
                                       budget_kb - (int) reserved_kb,
                                       NULL, TUPLESORT_NONE);
    return sort;
}

void
pin_group_sort_put(void *opaque, const uint8 *records, uint32 count)
{
    PinGroupSort *sort = opaque;
    PinGroupDatum datum;

    if (sort == NULL || sort->state == NULL || sort->sorted ||
        count > PIN_GROUP_SORT_BATCH || (count != 0 && records == NULL))
        elog(ERROR, "invalid Pin grouped sort input");
    SET_VARSIZE(&datum, sizeof(datum));
    CHECK_FOR_INTERRUPTS();
    for (uint32 index = 0; index < count; index++)
    {
        memcpy(datum.bytes, records + index * PIN_GROUP_RECORD_BYTES,
               PIN_GROUP_RECORD_BYTES);
        /* tuplesort copies the datum before the stack buffer is reused. */
        tuplesort_putdatum(sort->state, PointerGetDatum(&datum), false);
    }
}

void
pin_group_sort_finish(void *opaque)
{
    PinGroupSort *sort = opaque;

    if (sort == NULL || sort->state == NULL || sort->sorted)
        elog(ERROR, "invalid Pin grouped sort transition");
    tuplesort_performsort(sort->state);
    sort->sorted = true;
}

uint32
pin_group_sort_read(void *opaque, uint8 *records, uint32 capacity)
{
    PinGroupSort *sort = opaque;
    uint32 count = 0;

    if (sort == NULL || sort->state == NULL || !sort->sorted ||
        capacity > PIN_GROUP_SORT_BATCH || (capacity != 0 && records == NULL))
        elog(ERROR, "invalid Pin grouped sort output");
    CHECK_FOR_INTERRUPTS();
    while (count < capacity)
    {
        Datum value;
        bool isnull;
        struct varlena *datum;

        if (!tuplesort_getdatum(sort->state, true, false, &value, &isnull, NULL))
            break;
        if (isnull)
            elog(ERROR, "null Pin grouped sort record");
        datum = (struct varlena *) DatumGetPointer(value);
        if (datum == NULL || !VARATT_IS_4B_U(datum) ||
            VARSIZE(datum) != VARHDRSZ + PIN_GROUP_RECORD_BYTES)
            elog(ERROR, "invalid Pin grouped sort record size");
        /* copy before the next read invalidates the borrowed sort datum. */
        memcpy(records + count * PIN_GROUP_RECORD_BYTES, VARDATA(datum),
               PIN_GROUP_RECORD_BYTES);
        count++;
    }
    return count;
}

bool
pin_group_sort_end(void *opaque)
{
    PinGroupSort *sort = opaque;
    bool spilled = false;

    if (sort == NULL || sort->state == NULL)
        elog(ERROR, "invalid Pin grouped sort cleanup");
    if (sort->sorted)
    {
        TuplesortInstrumentation stats;

        tuplesort_get_stats(sort->state, &stats);
        spilled = stats.spaceType == SORT_SPACE_TYPE_DISK;
    }
    tuplesort_end(sort->state);
    sort->state = NULL;
    pfree(sort);
    return spilled;
}
