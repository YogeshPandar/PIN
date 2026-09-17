// pg18.6 header-only abi probes; source contracts are recorded in docs/api-evidence.md.
#include "postgres.h"
#include "access/amapi.h"
#include "access/generic_xlog.h"
#include "access/htup_details.h"
#include "mb/pg_wchar.h"
#include "storage/bufpage.h"
#include "storage/itemptr.h"
#include "pin_abi.h"

#include <stddef.h>
#include <stdint.h>

_Static_assert(PG_VERSION_NUM == 180006, "G0 requires PostgreSQL 18.6 headers");
_Static_assert(BLCKSZ == 8192, "G0 requires 8 KiB pages");
_Static_assert(sizeof(void *) == 8, "G0 requires 64-bit pointers");
_Static_assert(sizeof(Datum) == 8, "G0 requires 64-bit Datum");
_Static_assert(_Alignof(IndexAmRoutine) <= MAXIMUM_ALIGNOF, "AM needs excessive alignment");
_Static_assert(MAXIMUM_ALIGNOF == 8, "G0 requires 8-byte maximum alignment");
_Static_assert(MaxHeapTuplesPerPage > 0, "heap offset domain must be nonempty");
_Static_assert(MaxHeapTuplesPerPage <= 512, "heap offsets exceed private scratch capacity");
_Static_assert(InvalidBlockNumber == UINT32_MAX, "invalid block sentinel changed");
_Static_assert(InvalidOffsetNumber == 0, "invalid offset sentinel changed");
_Static_assert(sizeof(BlockNumber) == 4, "heap block width changed");
_Static_assert(sizeof(OffsetNumber) == 2, "heap offset width changed");
_Static_assert(MAX_GENERIC_XLOG_PAGES > 0 && MAX_GENERIC_XLOG_PAGES <= INT32_MAX,
               "generic WAL limit is outside the SQL diagnostic domain");

uint64_t
pin_abi_constant(uint32_t key)
{
    switch (key)
    {
        case 0: return PG_VERSION_NUM;
        case 1: return BLCKSZ;
        case 2: return sizeof(void *);
        case 3: return sizeof(Datum);
        case 4: return sizeof(IndexAmRoutine);
        case 5: return _Alignof(IndexAmRoutine);
        case 6: return T_IndexAmRoutine;
        case 7: return sizeof(ItemPointerData);
        case 8: return _Alignof(ItemPointerData);
        case 9: return MaxHeapTuplesPerPage;
        case 10: return MAX_GENERIC_XLOG_PAGES;
        case 11: return SizeOfPageHeaderData;
        case 12: return PG_UTF8;
        default: return UINT64_MAX;
    }
}

uint64_t
pin_abi_am_offset(uint32_t key)
{
    static const size_t offsets[] = {
#define PIN_AM_FIELD(field) offsetof(IndexAmRoutine, field),
#include "am_fields.def"
#undef PIN_AM_FIELD
    };
    if (key >= sizeof(offsets) / sizeof(offsets[0]))
        return UINT64_MAX;
    return offsets[key];
}
