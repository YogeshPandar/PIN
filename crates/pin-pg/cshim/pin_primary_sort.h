#ifndef PIN_PRIMARY_SORT_H
#define PIN_PRIMARY_SORT_H

#include "postgres.h"

/* TermSortRecord: UTF-8 term, NUL, then 8-byte big-endian root key. */
#define PIN_PRIMARY_SORT_MAX_KEY_BYTES 1033

extern void *pin_primary_sort_begin(uint64 reserved_bytes);
extern void pin_primary_sort_put(void *opaque, const uint8 *key, uint32 length);
extern void pin_primary_sort_finish(void *opaque);
/* Returns key length, or zero at EOF. */
extern uint32 pin_primary_sort_read(void *opaque, uint8 *key, uint32 capacity);
extern bool pin_primary_sort_end(void *opaque);

#endif
