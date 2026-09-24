#ifndef G9_PG_MOCK_H
#define G9_PG_MOCK_H
/* test doubles for marshalling only; not postgres headers or a wal simulator. */
#include <assert.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

typedef uint8_t uint8;
typedef uint32_t uint32;
typedef uint64_t uint64;
typedef int32_t int32;
typedef uintptr_t Datum;
typedef struct Tuplesortstate Tuplesortstate;
struct varlena { uint32 header; char data[]; };
typedef struct TuplesortInstrumentation { int spaceType; } TuplesortInstrumentation;

extern int maintenance_work_mem;
extern int autovacuum_work_mem;
extern bool mock_autovacuum;
extern void mock_interrupt(void);
extern _Noreturn void mock_error(const char *message, ...);
extern void *mock_allocate(size_t size);
extern void mock_free(void *pointer);

#define VARHDRSZ 4
#define BYTEAOID 17
#define ByteaLessOperator 1957
#define InvalidOid 0
#define TUPLESORT_NONE 0
#define SORT_SPACE_TYPE_DISK 0
#define SORT_SPACE_TYPE_MEMORY 1
#define ERROR 20
#define elog(level, ...) mock_error(__VA_ARGS__)
#define palloc0(size) mock_allocate(size)
#define pfree(pointer) mock_free(pointer)
#define CHECK_FOR_INTERRUPTS() mock_interrupt()
#define AmAutoVacuumWorkerProcess() mock_autovacuum
#define StaticAssertDecl(condition, message) _Static_assert(condition, message)
#define PointerGetDatum(pointer) ((Datum) (pointer))
#define DatumGetPointer(datum) ((void *) (datum))
#define VARDATA(pointer) ((char *) (pointer) + VARHDRSZ)
#define VARATT_IS_4B_U(pointer) ((*(const uint8 *) (pointer) & 3) == 0)

static inline void mock_set_size(void *pointer, uint32 size)
{
    uint32 header = size << 2;
    memcpy(pointer, &header, sizeof(header));
}
static inline uint32 mock_size(const void *pointer)
{
    uint32 header;
    memcpy(&header, pointer, sizeof(header));
    return header >> 2;
}
#define SET_VARSIZE(pointer, size) mock_set_size(pointer, size)
#define VARSIZE(pointer) mock_size(pointer)

Tuplesortstate *tuplesort_begin_datum(uint32 type, uint32 order, uint32 collation,
                                    bool nullsfirst, int memory, void *coordinate, int options);
void tuplesort_putdatum(Tuplesortstate *state, Datum value, bool isnull);
void tuplesort_performsort(Tuplesortstate *state);
bool tuplesort_getdatum(Tuplesortstate *state, bool forward, bool copy,
                       Datum *value, bool *isnull, Datum *abbreviation);
void tuplesort_get_stats(Tuplesortstate *state, TuplesortInstrumentation *stats);
void tuplesort_end(Tuplesortstate *state);
#endif
