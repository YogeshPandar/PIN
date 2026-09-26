#ifndef PIN_COUNT_H
#define PIN_COUNT_H

#include "postgres.h"
#include "utils/relcache.h"

/* scalar bridge; PostgreSQL owns the opaque context and all resource handles. */
extern void pin_count_init(Size participant_memory);
extern void pin_parallel_count_worker(void *segment, void *table);
extern uint32 pin_count_owner_lock(void *context, uint32 block, uint8 *out, uint32 capacity);
extern void pin_count_owner_unlock(void *context);
extern bool pin_count_generation_try_lock(void *context);
extern void pin_count_generation_unlock(void *context);
extern bool pin_count_fetch_visible(void *context, uint32 block, uint16 offset);
extern uint64 pin_count_fetch_visible_page(void *context, uint32 block,
                                           const uint64 *offsets, uint32 words);
extern bool pin_count_grouped_enabled(void);
extern bool pin_count_grouped_eligible(const uint8 *query, Size length);
extern bool pin_count_grouped_execute(Relation index, void *context,
                                      const uint8 *query, Size length, Size memory_bytes,
                                      int64 *result, uint64 *stats);
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
