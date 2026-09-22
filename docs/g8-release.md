# G8 operational qualification

Base: `69c9ad745eec62b5e33188381466e20e2952c1db`.
Status: implementation and qualification in progress, not a production release.

G8 follows blueprint sections 21, 22, 23, 24 and 25. It adds executable operational
qualification, reproducible package assembly, compatibility and upgrade policy,
security review evidence, operator runbooks and release provenance. It does not
turn earlier experimental gates into supported features by changing their labels.

## Release boundaries

The selected target remains PostgreSQL 18.6, Rust 1.98.1, pgrx/cargo-pgrx/
pgrx-pg-sys 0.19.2, x86_64 GNU/Linux, UTF-8, upstream heap and 8192-byte pages.
The SQL version remains `0.0.0`; no released upgrade baseline exists. An invented
no-op update script would not prove compatibility between experimental snapshots.

Physical replay, post-promotion search and concurrent hot-standby search are
separate contracts. Pin index reads during recovery remain rejected. Ordinary
sequential predicate evaluation does not read Pin storage. Logical subscribers
must create their own compatible extension, schema and local Pin indexes.

SQL ranked execution is still absent. Experimental count, parallel maintenance
and retained-prefix compaction controls are not promoted by G8. Performance
parity requires measured workloads, durability settings and resource evidence.

## Implementation scope

- Package the normal shared library, authoritative generated SQL and control file
  with compatibility metadata, hashes, source identity and dependency inventory.
- Qualify clean install, dump/restore, physical backup, recovery targets, streaming
  replay, promotion, logical subscriber maintenance and supported DDL lifecycles.
- Check default-off controls, ordinary-role privileges, RLS fallback and absence
  of test-hook SQL from deployment artifacts.
- Publish support, upgrade/rebuild, incident-response and contribution policies.
- Keep release qualification separate from source checks and unexecuted tests.

## Blocking evidence

Compilation, host tests, independent storage/FFI/security review, controlled
performance measurements and sustained resource accounting must be attached to
the exact candidate commit. Unrun or failed checks are not passes. A package
checksum proves byte identity, not publisher authenticity or semantic safety.

No tag, public release, default-on experimental feature or production-readiness
claim is authorized by this implementation PR. The repository owner retains the
release decision after independent review and qualification.
