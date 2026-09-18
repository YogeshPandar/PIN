#!/usr/bin/env bash
# sourced by the G2 driver to reuse its disposable, fsync-enabled test cluster.
if [[ ${BASH_SOURCE[0]} == "$0" ]]; then
  echo 'Run this suite through tools/g2_qualification.sh.' >&2
  exit 2
fi

"${psql[@]}" -f "$root/tests/sql/g3_compaction.sql" >"$work/g3-compaction.log" 2>&1

wait_for_structure_waiter() {
  local app=$1
  for _ in $(seq 1 200); do
    if [[ $("${psql[@]}" -Atqc \
      "SELECT EXISTS (
         SELECT FROM pg_stat_activity a JOIN pg_locks l USING (pid)
         WHERE a.application_name = '$app' AND l.locktype = 'page'
           AND l.relation = 'public.g3_docs_pin'::regclass AND l.page = 1
           AND l.mode = 'ExclusiveLock' AND NOT l.granted
       )") == t ]]; then
      return
    fi
    sleep 0.05
  done
  echo "maintenance $app did not wait for the structural reader barrier" >&2
  exit 1
}

# maintenance cannot reclaim a page while a bitmap reader retains its source.
start_blocker pin-g3-reader-blocker 2
reader_expected=$("${psql[@]}" -Atqc \
  "SELECT count(*) FROM public.g3_docs WHERE body OPERATOR(pin.@@@) pin.parse_query('common')")
PGAPPNAME=pin-g3-pinned-reader PGOPTIONS='-c lock_timeout=0 -c statement_timeout=60s' \
  "${psql[@]}" -Atq \
  -c "SET enable_seqscan = off; SET enable_bitmapscan = on;
      SET enable_indexscan = off; SET enable_indexonlyscan = off;
      SELECT pin.g2_inject(12, 1, true);" \
  -c "SELECT count(*) FROM public.g3_docs WHERE body OPERATOR(pin.@@@) pin.parse_query('common')" \
  >"$work/g3-pinned-reader.log" 2>&1 &
reader_pid=$!
wait_for_advisory_waiter pin-g3-pinned-reader
PGAPPNAME=pin-g3-waiting-maintenance PGOPTIONS='-c lock_timeout=0 -c statement_timeout=60s' \
  "${psql[@]}" -c 'VACUUM (INDEX_CLEANUP ON) public.g3_docs;' \
  >"$work/g3-waiting-maintenance.log" 2>&1 &
maintenance_pid=$!
wait_for_structure_waiter pin-g3-waiting-maintenance

# maintenance waits for readers before acquiring the writer lock.
"${psql[@]}" -c "INSERT INTO public.g3_docs VALUES (7001, 'common alpha concurrent');" \
  >"$work/g3-concurrent-writer.log" 2>&1
terminate_blocker
wait "$reader_pid"
wait "$maintenance_pid"
if [[ $(tail -n 1 "$work/g3-pinned-reader.log") != "$reader_expected" ]]; then
  echo 'the pinned reader changed its MVCC result while an insertion committed' >&2
  exit 1
fi

# cancellation must release structural exclusion without releasing the blocker.
start_blocker pin-g3-cancel-blocker 2
PGAPPNAME=pin-g3-cancel-reader PGOPTIONS='-c lock_timeout=0 -c statement_timeout=60s' \
  "${psql[@]}" -c "SET enable_seqscan = off; SET enable_bitmapscan = on;
      SET enable_indexscan = off; SET enable_indexonlyscan = off;
      SELECT pin.g2_inject(12, 1, true);" \
  -c "SELECT count(*) FROM public.g3_docs WHERE body OPERATOR(pin.@@@) pin.parse_query('common')" \
  >"$work/g3-cancel-reader.log" 2>&1 &
reader_pid=$!
wait_for_advisory_waiter pin-g3-cancel-reader
PGAPPNAME=pin-g3-cancel-maintenance PGOPTIONS='-c lock_timeout=0 -c statement_timeout=60s' \
  "${psql[@]}" -c 'VACUUM (INDEX_CLEANUP ON) public.g3_docs;' \
  >"$work/g3-cancel-maintenance.log" 2>&1 &
maintenance_pid=$!
wait_for_structure_waiter pin-g3-cancel-maintenance
"${psql[@]}" -Atqc \
  "SELECT pg_cancel_backend(pid) FROM pg_stat_activity WHERE application_name = 'pin-g3-cancel-reader'" >/dev/null
if wait "$reader_pid"; then
  echo 'the cancelled bitmap reader unexpectedly succeeded' >&2
  exit 1
fi
wait "$maintenance_pid"
terminate_blocker

prepare_g3_fault_fixture() {
  "${psql[@]}" -c "
    DROP TABLE IF EXISTS public.g3_fault_docs;
    CREATE TABLE public.g3_fault_docs(id bigint PRIMARY KEY, body text)
      WITH (autovacuum_enabled = false);
    INSERT INTO public.g3_fault_docs SELECT i, 'alpha beta common' FROM generate_series(1, 3000) AS i;
    CREATE INDEX g3_fault_docs_pin ON public.g3_fault_docs USING pin(body pin.text_ops);
    ANALYZE public.g3_fault_docs;" >>"$work/g3-fault-fixtures.log" 2>&1
}

verify_g3_fault_fixture() {
  "${psql[@]}" -v expected_rows="$1" -f "$root/tests/sql/g3_fault_verify.sql" \
    >>"$work/g3-fault-verify.log" 2>&1
}

# ERROR after each publication phase leaves a recoverable journal and releases locks.
for stage in 9 10 11; do
  prepare_g3_fault_fixture
  if "${psql[@]}" -c "SELECT pin.g2_inject($stage, 1, false);" \
    -c 'VACUUM (INDEX_CLEANUP ON) public.g3_fault_docs;' >"$work/g3-error-$stage.log" 2>&1; then
    echo "G3 ERROR injection $stage did not fire" >&2
    exit 1
  fi
  grep -q 'Pin injected storage error' "$work/g3-error-$stage.log"
  verify_g3_fault_fixture 3000
  "${psql[@]}" -c 'VACUUM (INDEX_CLEANUP ON) public.g3_fault_docs;' >>"$work/g3-error-recovery.log" 2>&1
  verify_g3_fault_fixture 3000
done

# exercise partial output, publication, later terms, and partial retirement on disk.
for stage in 9 10 11; do
  for occurrence in 1 2; do
    prepare_g3_fault_fixture
    app="pin-g3-crash-$stage-$occurrence"
    start_blocker "$app-blocker" 2
    PGAPPNAME="$app" PGOPTIONS='-c lock_timeout=0 -c statement_timeout=60s' \
      "${psql[@]}" -c "SELECT pin.g2_inject($stage, $occurrence, true);" \
      -c 'VACUUM (INDEX_CLEANUP ON) public.g3_fault_docs;' >"$work/g3-crash-$stage-$occurrence.log" 2>&1 &
    maintenance_pid=$!
    wait_for_advisory_waiter "$app"
    "$bin/pg_ctl" -D "$work/data" -m immediate -w stop
    wait "$maintenance_pid" || true
    wait "$blocker_client_pid" || true
    "$bin/pg_ctl" -D "$work/data" -l "$work/postgres.log" -w start
    verify_g3_fault_fixture 3000
    # insertion before recovery can reuse already-reclaimed pages for fragments.
    "${psql[@]}" -c "INSERT INTO public.g3_fault_docs VALUES
      (3001, repeat('alpha beta common ', 5000));" >>"$work/g3-post-crash-writes.log" 2>&1
    verify_g3_fault_fixture 3001
    "${psql[@]}" -c 'VACUUM (INDEX_CLEANUP ON) public.g3_fault_docs;' >>"$work/g3-crash-recovery.log" 2>&1
    verify_g3_fault_fixture 3001
  done
done
