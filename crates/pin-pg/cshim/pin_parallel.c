/* core schedules whole indexes; Pin keeps callback-local maintenance state.
 * contracts: pg18.6 commands/vacuum.h and commands/vacuumparallel.c.
 */
#include "postgres.h"
#include "pin_parallel.h"
#include "commands/vacuum.h"
#include "utils/guc.h"
#include <stdint.h>
#ifdef PIN_TEST_HOOKS
#include "access/parallel.h"
#include "fmgr.h"
#include "utils/fmgrprotos.h"
#endif

#define PIN_PARALLEL_VACUUM_OPTIONS \
    (VACUUM_OPTION_PARALLEL_BULKDEL | VACUUM_OPTION_PARALLEL_CLEANUP)

StaticAssertDecl(PIN_PARALLEL_VACUUM_OPTIONS <= UINT8_MAX,
                 "parallel vacuum options exceed the AM field");
StaticAssertDecl((PIN_PARALLEL_VACUUM_OPTIONS & ~VACUUM_OPTION_MAX_VALID_VALUE) == 0,
                 "parallel vacuum options contain unsupported flags");

static bool pin_enable_parallel_vacuum = false;
#ifdef PIN_TEST_HOOKS
static int pin_pause_worker_stage = 0;
#endif

void
pin_parallel_init(void)
{
    /* relcache handlers and workers must see one immutable capability mask. */
    DefineCustomBoolVariable("pin.enable_parallel_vacuum",
                             "Enables experimental PostgreSQL-managed parallel Pin VACUUM.",
                             "Parallelism is across indexes, not within one index.",
                             &pin_enable_parallel_vacuum, false, PGC_POSTMASTER, 0,
                             NULL, NULL, NULL);
#ifdef PIN_TEST_HOOKS
    /* core serializes this test-only GUC into worker startup state. */
    DefineCustomIntVariable("pin.g7_pause_worker_stage",
                            "Pauses a test worker at a storage transition.",
                            "Disposable test clusters only; the driver holds advisory key (180006, 4).",
                            &pin_pause_worker_stage, 0, 0, 15, PGC_SUSET,
                            GUC_NOT_IN_SAMPLE, NULL, NULL, NULL);
#endif
}

uint8
pin_parallel_vacuum_options(void)
{
    /* cleanup also compacts after bulk deletion, so conditional cleanup is wrong. */
    return pin_enable_parallel_vacuum ? PIN_PARALLEL_VACUUM_OPTIONS :
                                       VACUUM_OPTION_NO_PARALLEL;
}

#ifdef PIN_TEST_HOOKS
void
pin_parallel_test_event(uint8 stage)
{
    /* never run SPI or acquire a session lock inside a parallel worker. */
    if (IsParallelWorker() && stage >= 7 && stage <= 12 &&
        pin_pause_worker_stage == stage)
        (void) DirectFunctionCall2(pg_advisory_xact_lock_int4,
                                   Int32GetDatum(180006), Int32GetDatum(4));
}
#endif
