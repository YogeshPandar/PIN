# G0 unsafe boundary register

Review state: implementation self-review performed; independent reviewers are
not assigned. None of these entries is independently approved. G0 remains open.
See the identically named entries in [api-evidence.md](api-evidence.md) for pinned
upstream contracts and exact local obligations.

| ID | Operations | Safety argument | Required validation |
|---|---|---|---|
| ABI01 | Two non-throwing C probe declarations/calls | Fixed-width scalar ABI; any key accepted; no input pointers or host state | C compile, all 51 offsets, runtime `abi_check` |
| AM01 | `palloc`, pointer cast, `write`, Datum conversion | Zero-argument runtime handler; one fresh aligned context allocation, complete initialization before exposure, no Rust allocator adoption | SQL AM registration and catalog lifecycle |
| AM01 | Installed `extern "C-unwind"` callbacks | Selected signature and pgrx guard; unused opaque inputs never dereferenced; no storage acquired | Rejected index/opclass creation; callback error Drop count |
| COMPAT01 | `_PG_init`, preload flag read | PostgreSQL-controlled single-thread startup or guarded late-load attempt | Startup, restart, repeated rejected late loads |
| COMPAT01 | `GetConfigOption`, `CStr::from_ptr` | Fixed preset name; initialized GUCs; null checked; terminated borrowed bytes consumed before another host call | Exact server version/page-size checks, incompatible build gate |
| COMPAT01 | `GetDatabaseEncoding` | Only guarded entries in a connected backend | UTF-8 success, LATIN1 install rejection |

No raw shared-buffer slices, unchecked decoders, pointer arithmetic, atomics,
SIMD, or manual heap-visibility checks are added. The pure crate and build script
forbid unsafe code. There are no buffer pins, relation handles, generation
registrations, or ResourceOwner resources to release in this slice. Their future
introduction requires a new audit and real ERROR/abort/backend-death testing.

The test-only Drop probe makes no host calls and uses no unsafe code. It tests
stack unwinding, not the safety of a future resource wrapper. Source inventory
checks are drift detection, not type checking or a memory-safety proof.
