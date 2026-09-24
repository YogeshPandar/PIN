# Frontier validation provenance

These are observed validation results, not paired performance measurements.
`results.json` identifies the tested implementation commit, exact CI runs,
commands and raw-log hashes. The source change and tests were subsequently
formatted in `4db7410`; additional native SQL tests were added there. Neither
that later head nor later documentation/benchmark commits inherit a passed
check automatically. The results also record the observed Issue14, G6, G9 and
G0 pure passes on `4db7410`, and its completed native SQL/WAL-concurrency steps.
The final G2 recovery step subsequently passed as well. Artifact `10802616347`
from G0 run `35984784546` preserves the native logs for `4db7410`. Its SHA-256
is recorded in `results.json`. Consult the PR for subsequent-head CI.

The original Issue14 artifact is `10800888024` from run `35982647506`.
GitHub artifact retention is seven days. Copies of the raw archives are included
in the implementation handoff. The test matrix passed; its overall workflow failed
on formatting before the formatter correction. PostgreSQL job `107577954174`
passed in run `35982647514`, including actual grouped WAL recovery/concurrency
and G2 transactional/recovery qualification. This is not a claim of exhaustive
concurrency schedule coverage or independent unsafe review.

Local Python tests include compiled C-shim tests against doubles, source checks,
finite models and benchmark guards. No local Rust or PostgreSQL 18.6 execution
was possible. The new staged benchmark has not been run locally. Every absent
performance value is `null`, never zero or an estimated speedup.
