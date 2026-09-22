#!/usr/bin/env bash
# run native operational tests against a private normal installation.
set -euo pipefail
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
: "${PGRX_PG_CONFIG_PATH:?set the pinned pg_config path}"
: "${PIN_PG_SOURCE:?set the pinned PostgreSQL source checkout}"
: "${PIN_BUILD_REVISION:?set the candidate source commit}"
[[ $(id -u) != 0 ]] || { echo 'run qualification as a non-root user' >&2; exit 1; }
[[ $("$PGRX_PG_CONFIG_PATH" --version) == 'PostgreSQL 18.6' ]]
[[ $(git -C "$PIN_PG_SOURCE" rev-parse HEAD) == 724edf9bde9d356724ad384a2e196edc3c9f80f7 ]]
[[ $PIN_BUILD_REVISION =~ ^[0-9a-f]{40}$ ]]
export PATH="$("$PGRX_PG_CONFIG_PATH" --bindir):$PATH"
export PERL5LIB="$PIN_PG_SOURCE/src/test/perl"
export PG_REGRESS="$PIN_PG_SOURCE/src/test/regress/pg_regress"
export PG_TEST_TIMEOUT_DEFAULT=120
work=$(mktemp -d "${TMPDIR:-/tmp}/pin-g8.XXXXXXXX")
artifacts="$root/.artifacts/g8"
mkdir -p "$artifacts"
cleanup() {
    status=$?
    trap - EXIT
    if [[ -d "$work/log" ]]; then cp -a "$work/log/." "$artifacts/"; fi
    # PostgreSQL::Test::Cluster owns process termination before prove returns.
    rm -rf -- "$work"
    exit "$status"
}
trap cleanup EXIT
export TESTDATADIR="$work/data"
export TESTLOGDIR="$work/log"
[[ -x $PG_REGRESS ]]
cd "$work"
timeout --signal=TERM --kill-after=30s 15m prove --verbose "$root/tests/tap/001_operational.pl" "$root/tests/tap/002_logical.pl" 2>&1 | tee "$artifacts/tap.txt"
