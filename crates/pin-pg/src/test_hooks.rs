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
    requires = [g0_drop_count, g0_raise_error, g0_raise_panic, g0_raise_pg_error]
);
