# PR #19 follow-up validation record

This is a local source/driver validation record, **not a backend performance
run or native PostgreSQL qualification**. Code under test:
`0bf376fd766b7c9312941f26edcbb18ec04d6d28`, following the initial PR #19 head
`2ab08406e7bb8670aa2faf4375ef4f93a668bb53` and the exact CI formatter checkpoint
`65d179c`. Documentation and this record are added afterward.

`provenance.json` records the ZIP/tree, original PR head/tree, Cargo.lock and
review-artifact hashes, code commit and observed checks. `local-validation.tar.gz`
retains the successful 117-test Python runs, previous failures and retries, source
contract output, the historical archive extraction, and the failed push log.
The first Python run found a package import error which was fixed. Several subsequent
attempts timed out in an existing fake-psql pipe test; later isolated and complete
runs passed. The timeout cause was not established and its logs are not discarded. The
positive-response test now allows five seconds for child startup; its separate
50 ms no-response timeout test and the production timeout are unchanged. Six
isolated diagnostic runs and the final two complete suites passed.

Commands observed locally:

```sh
python3 -m unittest discover -s tests -v
python3 tools/check_contracts.py
python3 -m py_compile tools/frontier_options.py tools/g9_profile.py \
  tools/g9_cpu_profile.py tools/issue14_frontier_bench.py \
  tools/g9_qualification.py tests/test_issue14_anchors.py
bash -n tools/g9_qualification.sh tools/issue14_isolated_run.sh
git diff --check
```

The source contracts, Python compilation and shell parsing passed. The new Rust
and SQL tests have not executed here. Rust was not installed, as required by
AGENTS.md. The available host PostgreSQL is not the pinned 18.6 server. Earlier
PR-head CI success is not transferred to this changed code. Python source and
mocked-backend checks do not prove actual anchor selection in PostgreSQL.

`verified-historical-baseline.json` contains the independently rechecked archive
hashes, 552 validated inner manifest entries per archive, environment/status,
accepted PR #17 sample medians and write records. Its source is the adjacent
[PR #17 run](../2026-09-24-pr17-paired-cpu/README.md), whose original raw archives
remain unchanged. The shared-target run remains rejected. None of the historical
measurements is relabeled as an exact-ZIP or new-head measurement.

The original PR head's review artifact is GitHub run `36008246964`, artifact
`10811472043`, SHA-256
`c22cbf03262df54e10e06e477baeb606d22c031703eef9dd73674c92aba02e75`.
Its merge-preview source tree equals the recorded PR head tree. Its formatter
patch was applied verbatim. G0 run `36008246679`, pure job `107661929870`, reported
nine passing initial core anchor tests, Clippy/docs success and formatting failure.
Its PostgreSQL job `107661930195` passed the then-existing default-off lifecycle;
it did not run the newly added anchored matrix. The [architecture review](../../issue-14-anchors.md)
records the observed core work counts separately from backend results.

A normal Git push of the local branch was attempted and failed with
`Could not resolve host: github.com`. The connected GitHub actions available in
this session support reading the repository but expose no commit/push action.
Remote PR #19 remained a draft at its original head on recheck. Local commits
and the handoff patch are not claimed to have been added to GitHub.

Verify this record with `sha256sum -c SHA256SUMS`. Native correctness, replay,
standby, downgrade, profile attribution, independent review, latency and all
new-head performance targets remain pending. The [run procedure](../../issue-14-anchors.md)
includes isolated build/cluster commands and retains failed measurement outputs.
