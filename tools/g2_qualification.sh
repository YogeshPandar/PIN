#!/usr/bin/env bash
set -euo pipefail

: "${PGRX_PG_CONFIG_PATH:?set the PostgreSQL 18.6 pg_config path}"
if [[ $(id -u) == 0 ]]; then
  echo 'Run G2 qualification as an unprivileged user.' >&2
  exit 2
fi

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
bin=$("$PGRX_PG_CONFIG_PATH" --bindir)
if [[ $("$PGRX_PG_CONFIG_PATH" --version) != 'PostgreSQL 18.6' ]]; then
  echo 'G2 qualification requires PostgreSQL 18.6.' >&2
  exit 2
fi

work=$(mktemp -d /tmp/pin-g2.XXXXXXXX)
artifacts="$root/.artifacts/g2"
port=55482

cleanup() {
  "$bin/pg_ctl" -D "$work/data" -m immediate stop >/dev/null 2>&1 || true
  mkdir -p "$artifacts"
  cp "$work"/*.log "$artifacts/" 2>/dev/null || true
  rm -rf -- "$work"
}
trap cleanup EXIT

mkdir -p "$artifacts" "$work/socket"
unset PGHOST PGHOSTADDR PGPORT PGDATABASE PGUSER PGSERVICE PGSERVICEFILE PGPASSFILE PGOPTIONS PGAPPNAME

"$bin/initdb" -D "$work/data" --encoding=UTF8 --locale=C --auth-local=trust --auth-host=reject >/dev/null
cat >> "$work/data/postgresql.conf" <<CONF
shared_preload_libraries = 'pin'
listen_addresses = ''
unix_socket_directories = '$work/socket'
port = $port
fsync = on
full_page_writes = on
synchronous_commit = on
statement_timeout = '30s'
lock_timeout = '10s'
CONF

"$bin/pg_ctl" -D "$work/data" -l "$work/postgres.log" -w start
psql=("$bin/psql" -X -h "$work/socket" -p "$port" -d postgres -v ON_ERROR_STOP=1)

"${psql[@]}" -c 'CREATE EXTENSION pin;'
if [[ $("${psql[@]}" -Atqc "SELECT to_regprocedure('pin.g2_inject(integer,integer,boolean)') IS NOT NULL") != t ]]; then
  echo 'G2 qualification requires the test-hooks build.' >&2
  exit 1
fi

"${psql[@]}" -f "$root/tests/sql/g2_transactions.sql" | tee "$work/transactions.log"
"${psql[@]}" -f "$root/tests/sql/g4_queries.sql" | tee "$work/g4-queries.log"

start_blocker() {
  local app=$1
  local key=$2
  PGAPPNAME="$app" "${psql[@]}" \
    -c "SELECT pg_advisory_lock(180006, $key); SELECT pg_sleep(600);" \
    >"$work/$app.log" 2>&1 &
  blocker_client_pid=$!

  blocker_backend_pid=
  for _ in $(seq 1 200); do
    blocker_backend_pid=$("${psql[@]}" -Atqc \
      "SELECT a.pid FROM pg_stat_activity a JOIN pg_locks l USING (pid)
       WHERE a.application_name = '$app' AND l.locktype = 'advisory' AND l.granted
       LIMIT 1")
    [[ -n $blocker_backend_pid ]] && return
    sleep 0.05
  done

  echo "blocker $app did not acquire its advisory lock" >&2
  exit 1
}

wait_for_advisory_waiter() {
  local app=$1
  for _ in $(seq 1 200); do
    if [[ $("${psql[@]}" -Atqc \
      "SELECT EXISTS (
         SELECT FROM pg_stat_activity a JOIN pg_locks l USING (pid)
         WHERE a.application_name = '$app'
           AND l.locktype = 'advisory'
           AND NOT l.granted
       )") == t ]]; then
      return
    fi
    sleep 0.05
  done

  echo "backend $app did not reach the injected wait" >&2
  exit 1
}

terminate_blocker() {
  "${psql[@]}" -Atqc "SELECT pg_terminate_backend($blocker_backend_pid)" >/dev/null
  wait "$blocker_client_pid" || true
}

# fix one repeatable-read snapshot while writes commit.
start_blocker pin-g2-concurrency-blocker 3
PGAPPNAME=pin-g2-concurrency-reader "${psql[@]}" \
  -f "$root/tests/sql/g2_concurrent_reader.sql" >"$work/concurrent-reader.log" 2>&1 &
reader_pid=$!
wait_for_advisory_waiter pin-g2-concurrency-reader

writer_sql="$work/concurrent-writer.sql"
: >"$writer_sql"
for i in $(seq 1 64); do
  printf "INSERT INTO public.g2_concurrent(id, body) VALUES (%d, 'concurrent alpha writer'); SELECT pg_sleep(0.01);\n" \
    "$((21000 + i))" >>"$writer_sql"
done
PGAPPNAME=pin-g2-concurrency-writer "${psql[@]}" -f "$writer_sql" \
  >"$work/concurrent-writer.log" 2>&1 &
writer_pid=$!

visible=0
for _ in $(seq 1 200); do
  visible=$("${psql[@]}" -Atqc 'SELECT count(*) FROM public.g2_concurrent')
  (( visible > 8 )) && break
  sleep 0.05
done
if (( visible <= 8 )); then
  echo 'concurrent writer did not commit while the reader snapshot was pinned' >&2
  exit 1
fi

terminate_blocker
wait "$reader_pid"
wait "$writer_pid"

"$bin/pg_ctl" -D "$work/data" -m fast -w restart -l "$work/postgres.log"
"${psql[@]}" -f "$root/tests/sql/g2_post_restart.sql" | tee "$work/post-restart.log"

check_count() {
  local term=$1
  local expected=$2
  [[ $term =~ ^[a-z]+$ ]] || {
    echo "unsafe test term: $term" >&2
    exit 1
  }

  local sequential indexed plan
  sequential=$(PGOPTIONS='-c enable_seqscan=on -c enable_bitmapscan=off -c enable_indexscan=off -c enable_indexonlyscan=off' \
    "${psql[@]}" -Atq -c \
    "SELECT count(*) FROM public.g2_crash_docs WHERE body OPERATOR(pin.@@@) pin.parse_query('$term')")
  indexed=$(PGOPTIONS='-c enable_seqscan=off -c enable_bitmapscan=on -c enable_indexscan=off -c enable_indexonlyscan=off' \
    "${psql[@]}" -Atq -c \
    "SELECT count(*) FROM public.g2_crash_docs WHERE body OPERATOR(pin.@@@) pin.parse_query('$term')")
  plan=$(PGOPTIONS='-c enable_seqscan=off -c enable_bitmapscan=on -c enable_indexscan=off -c enable_indexonlyscan=off' \
    "${psql[@]}" -Atq -c \
    "EXPLAIN (FORMAT JSON) SELECT id FROM public.g2_crash_docs WHERE body OPERATOR(pin.@@@) pin.parse_query('$term')")

  if [[ $plan != *'Bitmap Index Scan'* ]]; then
    echo "crash recovery did not produce a bitmap index plan for $term" >&2
    exit 1
  fi
  if [[ $sequential != "$expected" || $indexed != "$expected" ]]; then
    echo "crash recovery mismatch for $term: expected $expected, sequential $sequential, indexed $indexed" >&2
    exit 1
  fi
}

terms=(crashone crashtwo crashthree crashfour crashfive crashsix)
for stage in $(seq 1 6); do
  term=${terms[$((stage - 1))]}
  blocker_app="pin-g2-crash-blocker-$stage"
  inserter_app="pin-g2-crash-$stage"

  start_blocker "$blocker_app" 2
  PGAPPNAME="$inserter_app" "${psql[@]}" -c \
    "SELECT pin.g2_inject($stage, 1, true);
     INSERT INTO public.g2_crash_docs(id, body)
     VALUES ($((30000 + stage)), repeat('$term alpha beta gamma ', 5000));" \
    >"$work/crash-$stage.log" 2>&1 &
  inserter_pid=$!

  wait_for_advisory_waiter "$inserter_app"

  # immediate stop simulates a postmaster crash at the durable boundary.
  "$bin/pg_ctl" -D "$work/data" -m immediate -w stop
  wait "$inserter_pid" || true
  wait "$blocker_client_pid" || true

  "$bin/pg_ctl" -D "$work/data" -l "$work/postgres.log" -w start
  check_count stable 1
  check_count "$term" 0

  "${psql[@]}" -c 'VACUUM (INDEX_CLEANUP ON) public.g2_crash_docs;'
  check_count "$term" 0
done

source "$root/tools/g3_qualification.sh"
source "$root/tools/g5_qualification.sh"

"${psql[@]}" -f "$root/tests/sql/g2_post_restart.sql" | tee "$work/post-crash.log"
"$bin/pg_ctl" -D "$work/data" -m fast -w stop
