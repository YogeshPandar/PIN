//! fixed error probes for disposable test clusters only.
//! no host calls run during drop; production builds omit every function here.

use pgrx::prelude::*;
use std::cell::Cell;

thread_local! {
    static DROPS: Cell<u32> = const { Cell::new(0) };
}

pub(crate) struct DropProbe;

impl Drop for DropProbe {
    fn drop(&mut self) {
        let _ = DROPS.try_with(|count| count.set(count.get().saturating_add(1)));
    }
}

#[pg_extern(volatile, parallel_unsafe)]
fn g0_drop_count() -> i64 {
    DROPS.with(|count| i64::from(count.get()))
}

#[pg_extern(volatile, parallel_unsafe)]
fn g0_raise_error() {
    crate::compatibility::database();
    let _probe = DropProbe;
    crate::am::unavailable();
}

#[pg_extern(volatile, parallel_unsafe)]
fn g0_raise_panic() {
    crate::compatibility::database();
    let _probe = DropProbe;
    panic!("Pin G0 deliberate test panic");
}

#[pg_extern(volatile, parallel_unsafe)]
fn g0_raise_pg_error() {
    crate::compatibility::database();
    let _probe = DropProbe;
    // the fixed query enters postgres through pgrx's guarded spi call.
    let sql = "DO $pin$ BEGIN RAISE EXCEPTION USING ERRCODE = '22012', \
               MESSAGE = 'Pin G0 PostgreSQL error', DETAIL = 'Pin G0 retained detail'; END; $pin$";
    if let Err(error) = Spi::run(sql) {
        pgrx::error!("unexpected SPI status: {error}");
    }
}

pgrx::extension_sql!(
    "REVOKE ALL ON FUNCTION pin.g0_drop_count(), pin.g0_raise_error(), \
     pin.g0_raise_panic(), pin.g0_raise_pg_error() FROM PUBLIC;",
    name = "pin_test_hook_permissions",
    requires = [
        g0_drop_count,
        g0_raise_error,
        g0_raise_panic,
        g0_raise_pg_error
    ]
);

thread_local! {
    static STORAGE_HOOK: Cell<Option<(u8, u32, bool)>> = const { Cell::new(None) };
}

// one-shot hooks are absent from production and restricted to disposable clusters.
#[pg_extern(volatile, parallel_unsafe)]
fn g2_inject(stage: i32, occurrence: i32, pause: bool) {
    // safety: pgrx enters this function on the backend main thread.
    if !unsafe { pg_sys::superuser() } {
        pgrx::ereport!(
            ERROR,
            PgSqlErrorCode::ERRCODE_INSUFFICIENT_PRIVILEGE,
            "Pin test injection requires a superuser"
        );
    }
    if (!(1..=16).contains(&stage) && !(32..=42).contains(&stage))
        || !(1..=4096).contains(&occurrence)
    {
        pgrx::ereport!(
            ERROR,
            PgSqlErrorCode::ERRCODE_INVALID_PARAMETER_VALUE,
            "invalid Pin test transition or occurrence"
        );
    }
    STORAGE_HOOK.with(|hook| hook.set(Some((stage as u8, occurrence as u32, pause))));
}

pub(crate) fn storage_event(stage: pin_core::mutable::Stage) {
    let transition = stage as u8;
    // safety: events run outside content locks; held pins and interlocks stay resource-owned.
    // the test-only worker hook uses a transaction-owned, fixed advisory lock.
    unsafe { crate::native::call(|| crate::native::pin_parallel_test_event(transition)) };

    let action = STORAGE_HOOK.with(|hook| {
        let (target, remaining, pause) = hook.get()?;
        if target != stage as u8 {
            return None;
        }
        if remaining > 1 {
            hook.set(Some((target, remaining - 1, pause)));
            return None;
        }
        hook.set(None);
        Some(pause)
    });
    match action {
        Some(false) => {
            pgrx::ereport!(
                ERROR,
                PgSqlErrorCode::ERRCODE_INTERNAL_ERROR,
                "Pin injected storage error"
            );
        }
        Some(true) => {
            // a test driver holds this fixed key and confirms the wait in pg_locks.
            // content locks are absent; a resource-owned count pin may remain.
            if let Err(error) = Spi::run("SELECT pg_catalog.pg_advisory_xact_lock(180006, 2)") {
                pgrx::error!("Pin test pause failed: {error}");
            }
        }
        None => {}
    }
}

pgrx::extension_sql!(
    "REVOKE ALL ON FUNCTION pin.g2_inject(integer, integer, boolean) FROM PUBLIC;",
    name = "pin_g2_hook_permissions",
    requires = [g2_inject]
);
