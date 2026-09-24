/* exercises the production bridge with invalidating, bounded sort test doubles. */
#include "g9_pg_mock.h"
#include "pin_grouped.h"
#include <limits.h>
#include <setjmp.h>
#include <stdio.h>

struct Tuplesortstate
{
    uint8 rows[1024][32];
    size_t count;
    size_t position;
    bool sorted;
    struct { uint32 header; uint8 bytes[32]; } borrowed;
};
int maintenance_work_mem = 4096;
int autovacuum_work_mem = -1;
bool mock_autovacuum = false;
static jmp_buf error_target;
static int allocations;
static int observed_memory;
static int interrupts;
static int expected_errors;
static int malformed;
static bool error_expected;
static bool spill;

#define EXPECT_ERROR(statement) do { \
    error_expected = true; \
    if (setjmp(error_target) == 0) { statement; assert(!"missing error"); } \
    error_expected = false; \
} while (0)

void *mock_allocate(size_t size)
{
    void *pointer = calloc(1, size);
    assert(pointer != NULL);
    allocations++;
    return pointer;
}
void mock_free(void *pointer)
{
    assert(pointer != NULL && allocations > 0);
    allocations--;
    free(pointer);
}
void mock_interrupt(void) { interrupts++; }
_Noreturn void mock_error(const char *message, ...)
{
    (void) message;
    assert(error_expected);
    expected_errors++;
    longjmp(error_target, 1);
}

Tuplesortstate *tuplesort_begin_datum(uint32 type, uint32 order, uint32 collation,
                                    bool nullsfirst, int memory, void *coordinate, int options)
{
    assert(type == BYTEAOID && order == ByteaLessOperator && collation == InvalidOid);
    assert(!nullsfirst && coordinate == NULL && options == TUPLESORT_NONE);
    observed_memory = memory;
    return mock_allocate(sizeof(Tuplesortstate));
}
void tuplesort_putdatum(Tuplesortstate *state, Datum value, bool isnull)
{
    const void *pointer = DatumGetPointer(value);
    assert(!state->sorted && !isnull && state->count < 1024);
    assert(VARSIZE(pointer) == 36);
    memcpy(state->rows[state->count++], VARDATA(pointer), 32);
}
static int compare(const void *left, const void *right) { return memcmp(left, right, 32); }
void tuplesort_performsort(Tuplesortstate *state)
{
    assert(!state->sorted);
    qsort(state->rows, state->count, 32, compare);
    state->sorted = true;
}
bool tuplesort_getdatum(Tuplesortstate *state, bool forward, bool copy,
                       Datum *value, bool *isnull, Datum *abbreviation)
{
    assert(state->sorted && forward && !copy && abbreviation == NULL);
    /* every read invalidates the preceding borrowed value. */
    memset(&state->borrowed, 0xcc, sizeof(state->borrowed));
    if (state->position == state->count)
        return false;
    SET_VARSIZE(&state->borrowed, malformed == 2 ? 35 : 36);
    memcpy(state->borrowed.bytes, state->rows[state->position++], 32);
    if (malformed == 3)
        state->borrowed.header |= 1;
    *value = PointerGetDatum(&state->borrowed);
    *isnull = malformed == 1;
    return true;
}
void tuplesort_get_stats(Tuplesortstate *state, TuplesortInstrumentation *stats)
{
    assert(state->sorted);
    stats->spaceType = spill ? SORT_SPACE_TYPE_DISK : SORT_SPACE_TYPE_MEMORY;
}
void tuplesort_end(Tuplesortstate *state) { mock_free(state); }

static void record(uint8 *bytes, uint64 value)
{
    for (int word = 0; word < 4; word++)
    {
        uint64 field = value + (uint64) word * 10000;
        for (int byte = 7; byte >= 0; byte--)
        {
            bytes[word * 8 + byte] = (uint8) field;
            field >>= 8;
        }
    }
}
static void budgets(void)
{
    void *sort;
    assert(pin_group_sort_begin(UINT64_MAX) == NULL);
    assert(pin_group_sort_begin((uint64) INT_MAX * 1024 + 1) == NULL);
    maintenance_work_mem = 128;
    assert(pin_group_sort_begin(64 * 1024 + 1) == NULL);
    sort = pin_group_sort_begin(64 * 1024);
    assert(sort != NULL && observed_memory == 64);
    assert(!pin_group_sort_end(sort));
    maintenance_work_mem = 4096;
    sort = pin_group_sort_begin(123 * 1024 + 1);
    assert(observed_memory == 4096 - 124);
    assert(!pin_group_sort_end(sort));
    mock_autovacuum = true;
    autovacuum_work_mem = 1024;
    sort = pin_group_sort_begin(1);
    assert(observed_memory == 1023);
    assert(!pin_group_sort_end(sort));
    autovacuum_work_mem = -1;
    sort = pin_group_sort_begin(1);
    assert(observed_memory == 4095);
    assert(!pin_group_sort_end(sort));
    mock_autovacuum = false;
    assert(allocations == 0);
}
static void batches(void)
{
    uint8 input[256 * 32 + 1];
    uint8 output[256 * 32 + 2];
    uint8 expected[32];
    uint32 returned;
    uint64 next = 1;
    void *sort = pin_group_sort_begin(0);
    EXPECT_ERROR(pin_group_sort_read(sort, NULL, 0));
    EXPECT_ERROR(pin_group_sort_put(sort, NULL, 1));
    EXPECT_ERROR(pin_group_sort_put(sort, input, 257));
    pin_group_sort_put(sort, NULL, 0);
    for (uint32 start = 0; start < 300; start += 256)
    {
        uint32 count = start == 0 ? 256 : 44;
        for (uint32 index = 0; index < count; index++)
            record(input + 1 + index * 32, 300 - start - index);
        pin_group_sort_put(sort, input + 1, count);
        memset(input, 0xdd, sizeof(input));
    }
    pin_group_sort_finish(sort);
    EXPECT_ERROR(pin_group_sort_finish(sort));
    EXPECT_ERROR(pin_group_sort_put(sort, NULL, 0));
    EXPECT_ERROR(pin_group_sort_read(sort, NULL, 1));
    EXPECT_ERROR(pin_group_sort_read(sort, output, 257));
    assert(pin_group_sort_read(sort, NULL, 0) == 0);
    do
    {
        memset(output, 0xaa, sizeof(output));
        returned = pin_group_sort_read(sort, output + 1, 256);
        assert(output[0] == 0xaa && output[sizeof(output) - 1] == 0xaa);
        for (uint32 index = 0; index < returned; index++)
        {
            record(expected, next++);
            assert(memcmp(output + 1 + index * 32, expected, 32) == 0);
        }
    } while (returned != 0);
    assert(next == 301 && interrupts >= 7);
    spill = true;
    assert(pin_group_sort_end(sort));
    spill = false;
    assert(allocations == 0);
}
static void malformed_records(void)
{
    uint8 bytes[32] = {0};
    for (malformed = 1; malformed <= 3; malformed++)
    {
        void *sort = pin_group_sort_begin(0);
        pin_group_sort_put(sort, bytes, 1);
        pin_group_sort_finish(sort);
        EXPECT_ERROR(pin_group_sort_read(sort, bytes, 1));
        assert(!pin_group_sort_end(sort));
    }
    malformed = 0;
    EXPECT_ERROR(pin_group_sort_finish(NULL));
    EXPECT_ERROR(pin_group_sort_end(NULL));
    assert(allocations == 0);
}
int main(void)
{
    budgets();
    batches();
    malformed_records();
    assert(expected_errors == 12);
    puts("g9 bridge: 300 records, 12 rejected calls, budgets and borrowed copies passed");
    return 0;
}
