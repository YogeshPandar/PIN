#ifndef PIN_GROUPED_H
#define PIN_GROUPED_H

#include "postgres.h"

/* records are 32-byte big-endian keys; both sides batch at most 256. */
extern void *pin_group_sort_begin(uint64 reserved_bytes);
extern void pin_group_sort_put(void *sort, const uint8 *records, uint32 count);
extern void pin_group_sort_finish(void *sort);
extern uint32 pin_group_sort_read(void *sort, uint8 *records, uint32 capacity);
extern bool pin_group_sort_end(void *sort);

#endif
