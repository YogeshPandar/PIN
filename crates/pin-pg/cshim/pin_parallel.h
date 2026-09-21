#ifndef PIN_PARALLEL_H
#define PIN_PARALLEL_H

#include "postgres.h"

/* startup owns configuration; no backend-local pointer enters shared state. */
extern void pin_parallel_init(void);
extern uint8 pin_parallel_vacuum_options(void);
#ifdef PIN_TEST_HOOKS
extern void pin_parallel_test_event(uint8 stage);
#endif

#endif
