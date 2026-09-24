#!/usr/bin/env bash
# run only against a private postgres installation with no pre-existing pin library.
set -euo pipefail
if [[ $# -lt 3 || ! $1 =~ ^[0-9a-f]{40}$ || ! $3 =~ ^[01]$ ]]; then
  echo 'usage: issue14_isolated_run.sh REVISION OUTPUT ANCHORS_0_OR_1 [benchmark options]' >&2
  exit 2
fi
revision=$1
output=$(realpath -m -- "$2")
anchors=$3
shift 3
for option in "$@"; do
  case "$option" in
    --revision|--revision=*|--output|--output=*|--bindir|--bindir=*|--frontier-anchors)
      echo 'revision, output, bindir and activation are controlled by this runner' >&2
      exit 2 ;;
  esac
done
: "${PGRX_PG_CONFIG_PATH:?set a fresh /tmp/pin-g9-*/pg/bin/pg_config path}"
[[ $(id -u) != 0 ]] || { echo 'run as an unprivileged user' >&2; exit 2; }
config=$(realpath -- "$PGRX_PG_CONFIG_PATH")
python3 - "$config" <<'PY'
from pathlib import Path
import sys
p = Path(sys.argv[1])
if (len(p.parts) != 6 or p.parts[1] != 'tmp' or not p.parts[2].startswith('pin-g9-')
        or p.parts[3:] != ('pg', 'bin', 'pg_config')):
    raise SystemExit('a fresh private /tmp/pin-g9-*/pg installation is required')
PY
[[ $("$config" --version) == 'PostgreSQL 18.6' ]]
[[ $(rustc --version) == 'rustc 1.98.1 '* ]]
[[ $(cargo pgrx --version) =~ (^|[[:space:]])0\.19\.2$ ]]
[[ -z ${RUSTFLAGS:-}${CARGO_ENCODED_RUSTFLAGS:-} ]] || exit 2
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
bin=$("$config" --bindir)
lib=$("$config" --pkglibdir)/pin.so
[[ ! -e $lib ]] || { echo 'use a new private postgres prefix for each candidate' >&2; exit 2; }
[[ $(git -C "$root" rev-parse "$revision^{commit}") == "$revision" ]]
mkdir -- "$output"
mkdir "$output/evidence"
cluster=''
cleanup() {
  status=$?
  trap - EXIT
  if [[ -n $cluster ]]; then
    "$bin/pg_ctl" -D "$cluster" -m immediate -w stop >> "$output/evidence/stop.log" 2>&1 || true
  fi
  printf '%s\n' "$status" > "$output/evidence/exit-status.txt"
  # finish log writes before hashing evidence.
  exec 1>&3 2>&4
  exec 3>&- 4>&-
  # keep failed builds, profiles and clusters available for inspection.
  (cd "$output/evidence" && find . -type f ! -name SHA256SUMS -print0 | sort -z |
    xargs -0 -r sha256sum > SHA256SUMS)
  exit "$status"
}
exec 3>&1 4>&2
trap cleanup EXIT
exec > "$output/evidence/run.log" 2>&1
printf '%s\n' "$revision" > "$output/evidence/revision.txt"
git -C "$root" worktree add --detach "$output/source" "$revision"
git -C "$output/source" rev-parse HEAD 'HEAD^{tree}' > "$output/evidence/source-identity.txt"
git -C "$output/source" archive --format=tar.gz -o "$output/evidence/source.tar.gz" HEAD
cp "$output/source/Cargo.lock" "$output/evidence/Cargo.lock"
mkdir "$output/target" "$output/build"
export CARGO_TARGET_DIR="$output/target" CARGO_BUILD_BUILD_DIR="$output/build"
export PIN_BUILD_REVISION="$revision" PIN_RELEASE_PACKAGE=1 PGRX_PG_CONFIG_PATH="$config"
# empty values also disable wrappers configured outside the repository.
export RUSTC_WRAPPER='' RUSTC_WORKSPACE_WRAPPER='' CARGO_INCREMENTAL=0
export RUSTFLAGS='' CARGO_ENCODED_RUSTFLAGS=''
unset CARGO_BUILD_RUSTC_WRAPPER CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER
{
  env | grep -E '^(CARGO_TARGET_DIR|CARGO_BUILD_BUILD_DIR|PIN_BUILD_REVISION|PIN_RELEASE_PACKAGE|PGRX_PG_CONFIG_PATH)='
  rustc -Vv
  cargo -V
  cargo pgrx --version
  "$config" --configure
} > "$output/evidence/build-environment.txt"
(
  cd "$output/source/crates/pin-pg"
  cargo build --locked --release -p pin-pg
  # pgrx 0.19.2 install has no locked option; reject any subsequent lockfile change.
  cargo pgrx install --release --pg-config "$config"
) > "$output/evidence/build.log" 2>&1
cmp "$output/source/Cargo.lock" "$output/evidence/Cargo.lock"
cp "$lib" "$output/evidence/pin.so"
for file in pin.control pin--0.0.0.sql; do
  cp "$("$config" --sharedir)/extension/$file" "$output/evidence/$file"
done
sha256sum "$lib" "$output/evidence/pin.so" > "$output/evidence/library-identity.txt"
unset PGHOST PGHOSTADDR PGPORT PGDATABASE PGUSER PGSERVICE PGSERVICEFILE PGPASSFILE PGOPTIONS PGAPPNAME
cluster=$(mktemp -d /tmp/pin-g9-run-XXXXXXXX)
printf '%s\n' "$cluster" > "$output/evidence/cluster-directory.txt"
"$bin/initdb" -D "$cluster" --encoding=UTF8 --locale=C --auth-local=trust --auth-host=reject
mkdir "$cluster/socket"
cat >> "$cluster/postgresql.conf" <<CONF
shared_preload_libraries = 'pin'
listen_addresses = ''
unix_socket_directories = '$cluster/socket'
port = 55491
fsync = on
full_page_writes = on
synchronous_commit = on
shared_buffers = '128MB'
work_mem = '64MB'
maintenance_work_mem = '64MB'
autovacuum = off
CONF
cp "$cluster/postgresql.conf" "$output/evidence/postgresql.conf"
"$bin/pg_ctl" -D "$cluster" -l "$output/evidence/postgres.log" -w start
export PGHOST="$cluster/socket" PGPORT=55491 PGDATABASE=postgres
"$bin/psql" -X -v ON_ERROR_STOP=1 -c 'CREATE EXTENSION pin;'
flags=()
if [[ $anchors == 1 ]]; then flags+=(--frontier-anchors); fi
# the same current driver tests either binary; nested git provenance uses its worktree.
cp "$root/tools/"*.py "$output/evidence/"
(
  cd "$output/source"
  python3 "$root/tools/issue14_frontier_bench.py" --disposable --bindir "$bin" \
    "$@" --revision "$revision" --output "$output/evidence/measurements" "${flags[@]}"
)
