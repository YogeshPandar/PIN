#ifndef PIN_PARALLEL_H
#define PIN_PARALLEL_H

#include "postgres.h"
#include "access/genam.h"

/* startup owns configuration; no backend-local pointer enters shared state. */
extern void pin_parallel_init(void);
extern uint8 pin_parallel_vacuum_options(void);
extern bool pin_parallel_build(Relation heap, Relation index, struct IndexInfo *info,
                               uint64 prepare_memory, uint64 participant_memory,
                               double *heap_tuples, uint64 *index_tuples);
extern void pin_parallel_build_writer_lock(void *lock);
extern void pin_parallel_build_writer_unlock(void *lock);
extern void pin_parallel_build_worker(void *segment, void *table);
#ifdef PIN_TEST_HOOKS
extern void pin_parallel_test_event(uint8 stage);
#endif

#endif
