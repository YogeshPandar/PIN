# Issue 14 local evidence, 2026-09-24

This record concerns the locally prepared candidate based on upstream
`1d80b58e0eb17b003326283f8050ac3556f80776`. It is not a PostgreSQL benchmark.
The extracted archive's restored tree equals upstream
`3419b5c52df53689cc5cbe5e75ce51f9c6fca387`.

## Observed locally

| Command/check | Result | Scope |
| --- | --- | --- |
| `python3 -m unittest discover -s tests -p 'test_*.py' -v` | 90 methods passed | Includes the ten new model/accounting tests and existing C test-double compilation/UBSan/bounds fixtures |
| `python3 -m unittest discover -s tests -p 'test_issue14_*.py' -v` | 10 methods passed | Finite-set/phrase models and measurement parsing/accounting |
| Python byte compilation of the new tools | Passed | Syntax/import compilation, not live database execution |
| YAML parsing of the new workflow | Passed | YAML syntax, not GitHub Actions execution |
| `git diff --check` | Passed | Whitespace/conflict-marker check, not Rust formatting |
| `git diff --exit-code -- Cargo.lock Cargo.toml` | Unchanged | No dependency or lockfile rewrite |

The model checks 57,344 four-group/three-term expression assignments. The sparse
and selective native page-read assertions were prepared separately from the remote
PR's newer grouped-scan implementation. No local throughput, PostgreSQL CPU,
hardware-counter or live allocation result is claimed here.

## Not executed locally

Rust compilation, rustfmt, Clippy, Rust unit/integration tests, the phrase release
ablation, PostgreSQL normal/fault/concurrency/recovery suites and the live benchmark
driver were not executed in the development environment. Existing or subsequent CI
results must be evaluated at their exact commit SHA.

## Publishing

The compatible parts of the local candidate were merged into pull request
[#15](https://github.com/YogeshPandar/PIN/pull/15) on
`perf/issue-14-adaptive-grouped-scans`. The PR already contained a newer grouped
scan implementation, so overlapping old grouped-scan files were not used to
overwrite that work. The bounded phrase recheck, phrase tests/ablation, issue-14
measurement tooling and supporting evidence were added on top of the PR head.

Issue 14 remains open and no 10x GIN claim is made by this record.

## Acceptance

Keep the issue open. Native correctness/current-head CI, independent review,
isolated repeatable profiles, cold/cache/corpus variation, sustained-write/delta
and maintenance/recovery qualification remain required. See the main
[implementation and measurement report](../../issue-14-performance.md).
