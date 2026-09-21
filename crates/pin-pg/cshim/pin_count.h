#ifndef PIN_COUNT_H
#define PIN_COUNT_H

#include "postgres.h"
#include "utils/relcache.h"

/* scalar bridge; PostgreSQL owns the opaque context and all resource handles. */
extern void pin_count_init(void);
extern uint32 pin_count_owner_lock(void *context, uint32 block, uint8 *out, uint32 capacity);
extern void pin_count_owner_unlock(void *context);
extern bool pin_count_all_visible(void *context, uint32 block);
extern bool pin_count_fetch(void *context, uint32 block, uint16 offset,
                            const uint8 **bytes, Size *length);
extern void pin_count_clear(void *context);
extern bool pin_count_single_term(const uint8 *query, Size length);
extern int64 pin_count_execute(Relation index, void *context,
                                const uint8 *query, Size length, uint64 *stats);
extern void pin_count_parallel_capture(Relation index, const uint8 *query, Size length,
                                       uint64 *words);
extern int64 pin_count_parallel_execute(Relation index, void *context, void *shared,
                                        const uint8 *query, Size length, uint64 *stats);
extern void pin_count_work_snapshot(void *shared, uint64 *words, uint32 count);
extern bool pin_count_work_claim(void *shared, const uint64 *expected,
                                 const uint64 *next, uint32 count);

#endif
