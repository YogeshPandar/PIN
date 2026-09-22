//! Default-off compaction policy; no shared state or new storage ownership.
//! The AM retains the existing exclusive structure/writer barriers.
//! Contracts: docs/g7-selective.md and the G7 API evidence entry.

use pgrx::guc::{GucContext, GucFlags, GucRegistry, GucSetting};
use pin_core::mutable::{CompactMode, CompactStats};

static ENABLE_DIRECT_TID: GucSetting<bool> = GucSetting::<bool>::new(false);

static ENABLE_COMPACT_REUSE: GucSetting<bool> = GucSetting::<bool>::new(false);

// called during guarded postmaster preload, before any backend uses the policy.
pub(crate) fn initialize() {
    GucRegistry::define_bool_guc(
        c"pin.enable_direct_tid_segments",
        c"Create experimental direct-TID sealed pages during VACUUM.",
        c"Changes disk format; rebuild indexes before downgrading this binary.",
        &ENABLE_DIRECT_TID,
        GucContext::Suset,
        GucFlags::default(),
    );
    GucRegistry::define_bool_guc(
        c"pin.enable_compact_reuse",
        c"Enable experimental live sealed-prefix retention during compaction.",
        c"Off retains the copying reference; canonical liveness and WAL are unchanged.",
        &ENABLE_COMPACT_REUSE,
        GucContext::Suset,
        GucFlags::default(),
    );
}

// freeze the backend's selected policy once per maintenance operation.
pub(crate) fn mode() -> CompactMode {
    if ENABLE_DIRECT_TID.get() {
        CompactMode::DirectTid
    } else if ENABLE_COMPACT_REUSE.get() {
        CompactMode::RetainSealedPrefix
    } else {
        CompactMode::Copy
    }
}

// called after maintenance barriers are released; no per-posting logging.
pub(crate) fn report(stats: CompactStats) {
    pgrx::pg_sys::debug1!(
        "Pin compaction: retained_pages={} written_pages={} reclaimed_pages={} reused_pages={}",
        stats.retained_pages,
        stats.written_pages,
        stats.reclaimed_pages,
        stats.reused_pages,
    );
}
