/* Bounded PostgreSQL bytea sorter for primary TermSortRecord keys. */
#include "postgres.h"
#include "pin_primary_sort.h"
#include "catalog/pg_operator_d.h"
#include "catalog/pg_type_d.h"
#include "miscadmin.h"
#include "postmaster/autovacuum.h"
#include "utils/tuplesort.h"
#include "varatt.h"
#include <limits.h>

#define PIN_PRIMARY_SORT_MIN_KB 64
#define PIN_PRIMARY_SORT_MIN_KEY_BYTES 10

typedef struct PinPrimarySort
{
	Tuplesortstate *state;
	bool sorted;
} PinPrimarySort;

typedef struct PinPrimaryDatum
{
	int32 header;
	uint8 bytes[PIN_PRIMARY_SORT_MAX_KEY_BYTES];
} PinPrimaryDatum;

StaticAssertDecl(offsetof(PinPrimaryDatum, bytes) == VARHDRSZ,
				 "primary sort datum payload must follow its varlena header");

static bool
pin_primary_key_valid(const uint8 *key, uint32 length)
{
	uint32 delimiter = 0;

	if (key == NULL || length < PIN_PRIMARY_SORT_MIN_KEY_BYTES ||
		length > PIN_PRIMARY_SORT_MAX_KEY_BYTES)
		return false;
	while (delimiter < length && key[delimiter] != 0)
		delimiter++;
	return delimiter > 0 && delimiter <= 1024 && length == delimiter + 9;
}

void *
pin_primary_sort_begin(uint64 reserved_bytes)
{
	PinPrimarySort *sort;
	int budget_kb = maintenance_work_mem;
	uint64 reserved_kb;

	if (AmAutoVacuumWorkerProcess() && autovacuum_work_mem >= 0)
		budget_kb = autovacuum_work_mem;
	if (budget_kb <= 0 || reserved_bytes > (uint64) INT_MAX * 1024)
		return NULL;
	reserved_kb = reserved_bytes / 1024 + (reserved_bytes % 1024 != 0);
	if ((uint64) budget_kb < reserved_kb + PIN_PRIMARY_SORT_MIN_KB)
		return NULL;

	/* The caller's memory context/resource owner cleans up on ERROR. */
	sort = palloc0(sizeof(*sort));
	sort->state = tuplesort_begin_datum(BYTEAOID, ByteaLessOperator,
									   InvalidOid, false,
									   budget_kb - (int) reserved_kb,
									   NULL, TUPLESORT_NONE);
	return sort;
}

void
pin_primary_sort_put(void *opaque, const uint8 *key, uint32 length)
{
	PinPrimarySort *sort = opaque;
	PinPrimaryDatum datum;

	if (sort == NULL || sort->state == NULL || sort->sorted ||
		!pin_primary_key_valid(key, length))
		elog(ERROR, "invalid Pin primary sort input");
	SET_VARSIZE(&datum, VARHDRSZ + length);
	memcpy(datum.bytes, key, length);
	CHECK_FOR_INTERRUPTS();
	/* tuplesort_putdatum copies every pass-by-reference datum. */
	tuplesort_putdatum(sort->state, PointerGetDatum(&datum), false);
}

void
pin_primary_sort_finish(void *opaque)
{
	PinPrimarySort *sort = opaque;

	if (sort == NULL || sort->state == NULL || sort->sorted)
		elog(ERROR, "invalid Pin primary sort transition");
	tuplesort_performsort(sort->state);
	sort->sorted = true;
}

uint32
pin_primary_sort_read(void *opaque, uint8 *key, uint32 capacity)
{
	PinPrimarySort *sort = opaque;
	Datum value;
	bool isnull;
	struct varlena *datum;
	uint32 length;

	if (sort == NULL || sort->state == NULL || !sort->sorted || key == NULL ||
		capacity > PIN_PRIMARY_SORT_MAX_KEY_BYTES)
		elog(ERROR, "invalid Pin primary sort output");
	CHECK_FOR_INTERRUPTS();
	if (!tuplesort_getdatum(sort->state, true, false, &value, &isnull, NULL))
		return 0;
	if (isnull)
		elog(ERROR, "null Pin primary sort key");
	datum = (struct varlena *) DatumGetPointer(value);
	if (datum == NULL || !VARATT_IS_4B_U(datum) ||
		VARSIZE(datum) < VARHDRSZ + PIN_PRIMARY_SORT_MIN_KEY_BYTES ||
		VARSIZE(datum) > VARHDRSZ + PIN_PRIMARY_SORT_MAX_KEY_BYTES)
		elog(ERROR, "invalid Pin primary sort datum size");
	length = VARSIZE(datum) - VARHDRSZ;
	if (length > capacity || !pin_primary_key_valid((const uint8 *) VARDATA(datum), length))
		elog(ERROR, "invalid Pin primary sort key");
	memcpy(key, VARDATA(datum), length);
	return length;
}

bool
pin_primary_sort_end(void *opaque)
{
	PinPrimarySort *sort = opaque;
	bool spilled = false;

	if (sort == NULL || sort->state == NULL)
		elog(ERROR, "invalid Pin primary sort cleanup");
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
