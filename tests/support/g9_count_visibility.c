/* test doubles for the actual extracted count bridge, not PostgreSQL behavior. */
#include <assert.h>
#include <stdbool.h>
#include <stdint.h>
#include <setjmp.h>
#include <stdio.h>
#include <stdlib.h>

typedef uint32_t uint32;
typedef uint16_t uint16;
typedef void *Relation;
typedef int Buffer;
typedef struct SnapshotData { bool mvcc; } *Snapshot;
typedef struct ItemPointerData { uint32 block; uint16 offset; } ItemPointerData;
typedef struct PinCountState {
    Relation index, heap;
    bool generation_locked;
    Buffer owner_buffer, vm_buffer;
    Snapshot snapshot;
    void *fetch, *heap_slot;
} PinCountState;

#define InvalidBlockNumber UINT32_MAX
#define InvalidOffsetNumber 0
#define MaxHeapTuplesPerPage 291
#define ShareLock 5
#define VISIBILITYMAP_ALL_VISIBLE 1
#define BufferIsValid(buffer) ((buffer) > 0)
#define IsMVCCSnapshot(snapshot) ((snapshot)->mvcc)
#define ItemPointerSet(tid, block_value, offset_value) \
    (*(tid) = (ItemPointerData){(block_value), (offset_value)})
#define ERROR 1

static jmp_buf jump;
static int errors, locks, unlocks, probes, fetches, polls;
static bool allow_lock = true, vm_visible, found = true, again_value, cancel;
static bool pin_enable_count_vm = true;
static PinCountState *active;
static uint32 expected_block;
static uint16 expected_offset;

#define elog(level, message) do { (void)(level); (void)(message); errors++; longjmp(jump, 1); } while (0)
#define CHECK_FOR_INTERRUPTS() do { polls++; if (cancel) elog(ERROR, "cancelled"); } while (0)
#define REJECT(statement) do { int before = errors; if (setjmp(jump) == 0) { statement; abort(); } \
    assert(errors == before + 1); } while (0)

static bool ConditionalLockPage(Relation index, uint32 block, int mode)
{
    assert(index == active->index && block == 0 && mode == ShareLock);
    if (!allow_lock)
        return false;
    locks++;
    return true;
}

static void UnlockPage(Relation index, uint32 block, int mode)
{
    assert(index == active->index && block == 0 && mode == ShareLock);
    assert(locks == unlocks + 1);
    unlocks++;
}

static int visibilitymap_get_status(Relation heap, uint32 block, Buffer *buffer)
{
    assert(heap == active->heap && block == expected_block);
    assert(active->generation_locked || BufferIsValid(active->owner_buffer));
    probes++;
    *buffer = 4;
    return vm_visible ? VISIBILITYMAP_ALL_VISIBLE : 0;
}

static bool table_index_fetch_tuple(void *fetch, ItemPointerData *tid, Snapshot snapshot,
                                   void *slot, bool *again, bool *all_dead)
{
    assert(fetch == active->fetch && slot == active->heap_slot);
    assert(snapshot == active->snapshot && snapshot->mvcc && all_dead == NULL);
    assert(!*again && tid->block == expected_block && tid->offset == expected_offset);
    fetches++;
    tid->offset = 290; /* the table AM may return a HOT descendant in its private copy. */
    *again = again_value;
    return found;
}

#include "g9_count_functions.inc"

int main(void)
{
    struct SnapshotData snapshot = {true};
    PinCountState state = {
        .index = (void *)1, .heap = (void *)2, .snapshot = &snapshot,
        .fetch = (void *)3, .heap_slot = (void *)4,
    };
    active = &state;
    expected_block = 123;
    expected_offset = 291;
    allow_lock = false;
    assert(!pin_count_generation_try_lock(&state));
    assert(!state.generation_locked && locks == 0);
    pin_count_generation_unlock(&state);
    assert(unlocks == 0);
    REJECT((void)pin_count_all_visible(&state, expected_block));
    REJECT((void)pin_count_fetch_visible(&state, expected_block, expected_offset));
    allow_lock = true;
    assert(pin_count_generation_try_lock(&state));
    REJECT((void)pin_count_generation_try_lock(&state));
    vm_visible = true;
    assert(pin_count_all_visible(&state, expected_block));
    vm_visible = false;
    assert(!pin_count_all_visible(&state, expected_block));
    assert(probes == 2); /* a retained VM buffer is not a retained certification. */
    pin_enable_count_vm = false;
    vm_visible = true;
    assert(!pin_count_all_visible(&state, expected_block) && probes == 2);
    pin_enable_count_vm = true;
    assert(pin_count_fetch_visible(&state, expected_block, expected_offset));
    assert(expected_offset == 291 && fetches == 1 && polls == 1);
    found = false;
    assert(!pin_count_fetch_visible(&state, expected_block, expected_offset));
    again_value = true;
    REJECT((void)pin_count_fetch_visible(&state, expected_block, expected_offset));
    again_value = false;
    snapshot.mvcc = false;
    REJECT((void)pin_count_fetch_visible(&state, expected_block, expected_offset));
    snapshot.mvcc = true;
    state.snapshot = NULL;
    REJECT((void)pin_count_fetch_visible(&state, expected_block, expected_offset));
    state.snapshot = &snapshot;
    REJECT((void)pin_count_fetch_visible(&state, expected_block, 0));
    REJECT((void)pin_count_fetch_visible(&state, expected_block, 292));
    REJECT((void)pin_count_fetch_visible(&state, InvalidBlockNumber, 1));
    REJECT((void)pin_count_all_visible(&state, InvalidBlockNumber));
    cancel = true;
    REJECT((void)pin_count_fetch_visible(&state, expected_block, expected_offset));
    cancel = false;
    pin_count_generation_unlock(&state);
    pin_count_generation_unlock(&state);
    assert(!state.generation_locked && locks == 1 && unlocks == 1);
    state.owner_buffer = 7;
    assert(pin_count_all_visible(&state, expected_block));
    REJECT((void)pin_count_generation_try_lock(&state));
    state.owner_buffer = 0;
    state.index = NULL;
    REJECT((void)pin_count_generation_try_lock(&state));
    assert(errors == 13);
    puts("actual visibility bridge: 13 rejected calls; fresh VM, HOT and guard checks passed");
    return 0;
}
