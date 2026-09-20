#!/usr/bin/env bash
# sourced only by the disposable G2 driver after its G3 helper definitions.
if [[ ${BASH_SOURCE[0]} == "$0" ]]; then
  echo 'Run this suite through tools/g2_qualification.sh.' >&2
  exit 2
fi

prepare_g7_fault_fixture() {
  prepare_g3_fault_fixture
  "${psql[@]}" -c "INSERT INTO public.g3_fault_docs
    SELECT i, 'alpha beta common' FROM generate_series(3001, 8500) AS i;" \
    -c 'SET pin.enable_compact_reuse = off;' \
    -c 'VACUUM (INDEX_CLEANUP ON) public.g3_fault_docs;' \
    -c "INSERT INTO public.g3_fault_docs
      SELECT i, 'alpha beta common' FROM generate_series(8501, 8800) AS i;" \
    -c 'CHECKPOINT;' >>"$work/g7-fault-fixtures.log" 2>&1
}

# these phases cover unpublished output, the three-page swap, and retired suffixes.
for stage in 9 10 11; do
  prepare_g7_fault_fixture
  if "${psql[@]}" -c "SET pin.enable_compact_reuse = on; SELECT pin.g2_inject($stage, 1, false);" \
    -c 'VACUUM (INDEX_CLEANUP ON) public.g3_fault_docs;' >"$work/g7-error-$stage.log" 2>&1; then
    echo "G7 ERROR injection $stage did not fire" >&2
    exit 1
  fi
  grep -q 'Pin injected storage error' "$work/g7-error-$stage.log"
  verify_g3_fault_fixture 8800
  "${psql[@]}" -c 'SET pin.enable_compact_reuse = on;' \
    -c 'VACUUM (INDEX_CLEANUP ON) public.g3_fault_docs;' >>"$work/g7-error-recovery.log" 2>&1
  verify_g3_fault_fixture 8800

  prepare_g7_fault_fixture
  app="pin-g7-crash-$stage"
  start_blocker "$app-blocker" 2
  PGAPPNAME="$app" PGOPTIONS='-c lock_timeout=0 -c statement_timeout=60s' \
    "${psql[@]}" -c "SET pin.enable_compact_reuse = on; SELECT pin.g2_inject($stage, 1, true);" \
    -c 'VACUUM (INDEX_CLEANUP ON) public.g3_fault_docs;' >"$work/g7-crash-$stage.log" 2>&1 &
  maintenance_pid=$!
  wait_for_advisory_waiter "$app"
  "$bin/pg_ctl" -D "$work/data" -m immediate -w stop
  wait "$maintenance_pid" || true
  wait "$blocker_client_pid" || true
  "$bin/pg_ctl" -D "$work/data" -l "$work/postgres.log" -w start
  verify_g3_fault_fixture 8800
  # accepted writes after restart may consume reclaimed pages, never retained ones.
  "${psql[@]}" -c "INSERT INTO public.g3_fault_docs VALUES
    (8801, repeat('alpha beta common ', 5000));" >>"$work/g7-post-crash-writes.log" 2>&1
  verify_g3_fault_fixture 8801
  "${psql[@]}" -c 'SET pin.enable_compact_reuse = on;' \
    -c 'VACUUM (INDEX_CLEANUP ON) public.g3_fault_docs;' >>"$work/g7-crash-recovery.log" 2>&1
  verify_g3_fault_fixture 8801
done
