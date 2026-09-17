//! header/binding comparison before any postgresql-owned am node is constructed.
//! c probes return scalar constants only; no pointer, allocation, error or shared state.
//! contracts: pg access/amapi.h, rust offset_of!/size_of/align_of and abi01 in the evidence ledger.

use pgrx::pg_sys;
use std::mem::{align_of, offset_of, size_of};

// safety: both declarations match pin_abi.h; uint32_t/uint64_t map to rust u32/u64.
unsafe extern "C" {
    fn pin_abi_constant(key: u32) -> u64;
    fn pin_abi_am_offset(key: u32) -> u64;
}

pub(crate) fn constant(key: u32) -> u64 {
    // safety: the shim accepts every u32, dereferences no input and cannot unwind.
    unsafe { pin_abi_constant(key) }
}

fn am_offset(key: u32) -> u64 {
    // safety: invalid keys return a sentinel; this c function never raises error.
    unsafe { pin_abi_am_offset(key) }
}

pub(crate) fn validate() -> Result<(), &'static str> {
    if align_of::<pg_sys::IndexAmRoutine>() > 8 {
        return Err("Pin IndexAmRoutine alignment exceeds PostgreSQL palloc alignment");
    }
    let expected = [
        (0, 180006),
        (1, pg_sys::BLCKSZ as u64),
        (2, size_of::<*const ()>() as u64),
        (3, size_of::<pg_sys::Datum>() as u64),
        (4, size_of::<pg_sys::IndexAmRoutine>() as u64),
        (5, align_of::<pg_sys::IndexAmRoutine>() as u64),
        (6, pg_sys::NodeTag::T_IndexAmRoutine as u64),
        (7, size_of::<pg_sys::ItemPointerData>() as u64),
        (8, align_of::<pg_sys::ItemPointerData>() as u64),
    ];
    for (key, value) in expected {
        if constant(key) != value {
            return Err("Pin C headers and pgrx ABI constants disagree");
        }
    }
    if constant(1) != 8192 || constant(9) == 0 || constant(9) > 512 {
        return Err("Pin requires the audited 8 KiB heap offset domain");
    }
    if constant(10) == 0 || constant(10) == u64::MAX {
        return Err("Pin could not read the generic WAL registration limit");
    }
    for (key, rust_offset) in AM_OFFSETS.iter().enumerate() {
        if am_offset(key as u32) != *rust_offset as u64 {
            return Err("Pin IndexAmRoutine field offsets disagree with PostgreSQL headers");
        }
    }
    if am_offset(AM_OFFSETS.len() as u32) != u64::MAX {
        return Err("Pin C and Rust AM field inventories disagree");
    }
    Ok(())
}

// keep independent inventories to detect one-sided abi edits.
const AM_OFFSETS: &[usize] = &[
    offset_of!(pg_sys::IndexAmRoutine, type_),
    offset_of!(pg_sys::IndexAmRoutine, amstrategies),
    offset_of!(pg_sys::IndexAmRoutine, amsupport),
    offset_of!(pg_sys::IndexAmRoutine, amoptsprocnum),
    offset_of!(pg_sys::IndexAmRoutine, amcanorder),
    offset_of!(pg_sys::IndexAmRoutine, amcanorderbyop),
    offset_of!(pg_sys::IndexAmRoutine, amcanhash),
    offset_of!(pg_sys::IndexAmRoutine, amconsistentequality),
    offset_of!(pg_sys::IndexAmRoutine, amconsistentordering),
    offset_of!(pg_sys::IndexAmRoutine, amcanbackward),
    offset_of!(pg_sys::IndexAmRoutine, amcanunique),
    offset_of!(pg_sys::IndexAmRoutine, amcanmulticol),
    offset_of!(pg_sys::IndexAmRoutine, amoptionalkey),
    offset_of!(pg_sys::IndexAmRoutine, amsearcharray),
    offset_of!(pg_sys::IndexAmRoutine, amsearchnulls),
    offset_of!(pg_sys::IndexAmRoutine, amstorage),
    offset_of!(pg_sys::IndexAmRoutine, amclusterable),
    offset_of!(pg_sys::IndexAmRoutine, ampredlocks),
    offset_of!(pg_sys::IndexAmRoutine, amcanparallel),
    offset_of!(pg_sys::IndexAmRoutine, amcanbuildparallel),
    offset_of!(pg_sys::IndexAmRoutine, amcaninclude),
    offset_of!(pg_sys::IndexAmRoutine, amusemaintenanceworkmem),
    offset_of!(pg_sys::IndexAmRoutine, amsummarizing),
    offset_of!(pg_sys::IndexAmRoutine, amparallelvacuumoptions),
    offset_of!(pg_sys::IndexAmRoutine, amkeytype),
    offset_of!(pg_sys::IndexAmRoutine, ambuild),
    offset_of!(pg_sys::IndexAmRoutine, ambuildempty),
    offset_of!(pg_sys::IndexAmRoutine, aminsert),
    offset_of!(pg_sys::IndexAmRoutine, aminsertcleanup),
    offset_of!(pg_sys::IndexAmRoutine, ambulkdelete),
    offset_of!(pg_sys::IndexAmRoutine, amvacuumcleanup),
    offset_of!(pg_sys::IndexAmRoutine, amcanreturn),
    offset_of!(pg_sys::IndexAmRoutine, amcostestimate),
    offset_of!(pg_sys::IndexAmRoutine, amgettreeheight),
    offset_of!(pg_sys::IndexAmRoutine, amoptions),
    offset_of!(pg_sys::IndexAmRoutine, amproperty),
    offset_of!(pg_sys::IndexAmRoutine, ambuildphasename),
    offset_of!(pg_sys::IndexAmRoutine, amvalidate),
    offset_of!(pg_sys::IndexAmRoutine, amadjustmembers),
    offset_of!(pg_sys::IndexAmRoutine, ambeginscan),
    offset_of!(pg_sys::IndexAmRoutine, amrescan),
    offset_of!(pg_sys::IndexAmRoutine, amgettuple),
    offset_of!(pg_sys::IndexAmRoutine, amgetbitmap),
    offset_of!(pg_sys::IndexAmRoutine, amendscan),
    offset_of!(pg_sys::IndexAmRoutine, ammarkpos),
    offset_of!(pg_sys::IndexAmRoutine, amrestrpos),
    offset_of!(pg_sys::IndexAmRoutine, amestimateparallelscan),
    offset_of!(pg_sys::IndexAmRoutine, aminitparallelscan),
    offset_of!(pg_sys::IndexAmRoutine, amparallelrescan),
    offset_of!(pg_sys::IndexAmRoutine, amtranslatestrategy),
    offset_of!(pg_sys::IndexAmRoutine, amtranslatecmptype),
];
