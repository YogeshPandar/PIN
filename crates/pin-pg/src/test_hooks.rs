//! fixed error probes for disposable test clusters only.
//! no host calls run during drop; production builds omit every function here.

use pgrx::prelude::*;
use std::cell::Cell;

use pin_core::error::{Error, Result};
use pin_core::mutable::PageStore;
use pin_core::mutable::page::{CAPACITY, Page, PageKind};

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

fn v2_hook_superuser() {
    // safety: this test-only SQL entry runs on PostgreSQL's backend thread.
    if !unsafe { pg_sys::superuser() } {
        pgrx::ereport!(
            ERROR,
            PgSqlErrorCode::ERRCODE_INSUFFICIENT_PRIVILEGE,
            "Pin v2 extent probes require a superuser"
        );
    }
}

/// exercises the pg store fast path against a fresh wal initialized page.
#[pg_extern(volatile, parallel_unsafe)]
fn v2_primary_extent_roundtrip(index_oid: pg_sys::Oid) -> bool {
    crate::compatibility::database();
    v2_hook_superuser();
    let is_pin = Spi::get_one::<bool>(&format!(
        "SELECT c.relam = a.oid FROM pg_catalog.pg_class c \
         JOIN pg_catalog.pg_am a ON a.amname = 'pin' WHERE c.oid = {}",
        index_oid.to_u32()
    ))
    .ok()
    .flatten()
    .unwrap_or(false);
    if !is_pin {
        pgrx::ereport!(
            ERROR,
            PgSqlErrorCode::ERRCODE_INVALID_PARAMETER_VALUE,
            "v2 extent probe requires a PIN index"
        );
    }

    // index_open acquires the relation lock; index_close releases it on success.
    // pgrx guards the throwing c call.
    // safety: the OID was resolved as an extant PIN index immediately above.
    let index = unsafe {
        crate::native::call(|| {
            pg_sys::index_open(index_oid, pg_sys::AccessExclusiveLock as pg_sys::LOCKMODE)
        })
    };
    // safety: the relation stays open and exclusively locked for the complete store operation.
    let result = unsafe {
        crate::storage::with_writer(index, |store| -> Result<bool> {
            let offset = 32u16;
            let expected: Vec<u8> = (0..CAPACITY - 16)
                .map(|n| (n as u8).wrapping_mul(37) ^ 0x6d)
                .collect();
            let block = store.extend()?;
            let page = Page::primary(block, &expected)?;
            store.commit(&[&page])?;

            let full = store.read(block)?;
            if full.kind() != PageKind::Primary || full.primary_payload()? != expected {
                return Ok(false);
            }
            let length = 13usize;
            let mut guarded = [0xa5u8; 17];
            store.read_primary_extent(block, offset, &mut guarded[2..2 + length])?;
            let selected = full.primary_extent(offset, length as u16)?;
            Ok(guarded[..2] == [0xa5, 0xa5]
                && guarded[2 + length..] == [0xa5, 0xa5]
                && &guarded[2..2 + length] == selected)
        })
    };
    // safety: this exact index_open acquisition is paired once on ordinary return.
    unsafe {
        crate::native::call(|| {
            pg_sys::index_close(index, pg_sys::AccessExclusiveLock as pg_sys::LOCKMODE)
        })
    };
    match result {
        Ok(true) => true,
        Ok(false) => pgrx::error!("v2 extent copy did not match the full private page"),
        Err(error) => pgrx::error!("v2 extent probe failed: {error}"),
    }
}

/// triggers the fast reader's wrong-page-type check on an ordinary pin page.
#[pg_extern(volatile, parallel_unsafe)]
fn v2_primary_extent_wrong_tag(index_oid: pg_sys::Oid) {
    crate::compatibility::database();
    v2_hook_superuser();
    // safety: the disposable harness supplies an existing PIN index OID.
    let index = unsafe {
        crate::native::call(|| {
            pg_sys::index_open(index_oid, pg_sys::AccessExclusiveLock as pg_sys::LOCKMODE)
        })
    };
    // safety: the relation remains open and locked during the intentional ERROR.
    let result = unsafe {
        crate::storage::with_writer(index, |store| -> Result<()> {
            let blocks = store.blocks()?;
            let mut candidate = None;
            for block in 1..blocks {
                let image = store.read(block)?;
                if image.kind() != PageKind::Primary {
                    candidate = Some(block);
                    break;
                }
            }
            let block = candidate.ok_or(Error::InvalidState)?;
            let mut output = [0u8; 1];
            store.read_primary_extent(block, 16, &mut output)
        })
    };
    // successful return means the negative control missed the c error.
    match result {
        Ok(()) => pgrx::error!("wrong-tag extent probe unexpectedly succeeded"),
        Err(error) => {
            pgrx::error!("wrong-tag extent probe failed before the native error: {error}")
        }
    }
}

pgrx::extension_sql!(
    "REVOKE ALL ON FUNCTION pin.v2_primary_extent_roundtrip(oid), \
     pin.v2_primary_extent_wrong_tag(oid) FROM PUBLIC;",
    name = "pin_v2_extent_hook_permissions",
    requires = [v2_primary_extent_roundtrip, v2_primary_extent_wrong_tag]
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
