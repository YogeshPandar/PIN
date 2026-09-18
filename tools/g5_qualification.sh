#!/usr/bin/env bash
# sourced by G2 to use its disposable PostgreSQL 18.6 test-hook cluster.
if [[ ${BASH_SOURCE[0]} == "$0" ]]; then
  echo 'Run this suite through tools/g2_qualification.sh.' >&2
  exit 2
fi

"${psql[@]}" -f "$root/tests/sql/g5_counts.sql" >"$work/g5-counts.log" 2>&1

g5_options='-c pin.enable_count_fastpath=on -c pin.enable_count_vm=on -c enable_seqscan=off -c enable_bitmapscan=on -c enable_indexscan=off -c enable_indexonlyscan=off -c max_parallel_workers_per_gather=0 -c lock_timeout=0 -c statement_timeout=60s'
g5_query="SELECT count(*) FROM ONLY public.g5_docs WHERE body OPERATOR(pin.@@@) pin.parse_query('alpha')"

g5_assert_count() {
  local expected=$1
  local baseline counted plan
  baseline=$(PGOPTIONS='-c pin.enable_count_fastpath=off -c enable_seqscan=on -c enable_bitmapscan=off -c enable_indexscan=off -c enable_indexonlyscan=off' \
    "${psql[@]}" -Atqc "$g5_query")
  counted=$(PGOPTIONS="$g5_options" "${psql[@]}" -Atqc "$g5_query")
  plan=$(PGOPTIONS="$g5_options" "${psql[@]}" -Atqc "EXPLAIN (ANALYZE, FORMAT JSON) $g5_query")
  printf '%s\n' "$plan" >>"$work/g5-explain.log"
  if [[ $baseline != "$expected" || $counted != "$expected" || $plan != *'PinCount'* ]]; then
    echo "G5 count/plan mismatch: expected=$expected baseline=$baseline counted=$counted" >&2
    exit 1
  fi
}

wait_for_g5_owner_waiter() {
  local app=$1
  for _ in $(seq 1 200); do
    if [[ $("${psql[@]}" -Atqc \
      "SELECT EXISTS (SELECT FROM pg_stat_activity WHERE application_name = '$app'
         AND wait_event_type = 'BufferPin')") == t ]]; then
      return
    fi
    sleep 0.05
  done
  echo "G5 VACUUM $app did not wait for the canonical owner pin" >&2
  exit 1
}

g5_assert_count 1918

# a retained repeatable-read snapshot must survive HOT/non-HOT changes and VACUUM.
start_blocker pin-g5-old-snapshot-blocker 3
cat >"$work/g5-old-snapshot.sql" <<SQL
BEGIN ISOLATION LEVEL REPEATABLE READ;
SELECT count(*) AS expected FROM ONLY public.g5_docs
WHERE body OPERATOR(pin.@@@) pin.parse_query('alpha') \gset
SELECT pg_advisory_xact_lock(180006, 3);
SELECT count(*) = :expected AS same_count FROM ONLY public.g5_docs
WHERE body OPERATOR(pin.@@@) pin.parse_query('alpha');
COMMIT;
SQL
PGAPPNAME=pin-g5-old-snapshot PGOPTIONS="$g5_options" "${psql[@]}" -Atq \
  -f "$work/g5-old-snapshot.sql" >"$work/g5-old-snapshot.log" 2>&1 &
reader_pid=$!
wait_for_advisory_waiter pin-g5-old-snapshot
"${psql[@]}" -c "INSERT INTO public.g5_docs(id, body) VALUES (9000, 'alpha');
    UPDATE public.g5_docs SET body = 'beta' WHERE id = 3;
    DELETE FROM public.g5_docs WHERE id = 4;" >"$work/g5-snapshot-writer.log" 2>&1
"${psql[@]}" -c 'VACUUM (INDEX_CLEANUP ON) public.g5_docs;' >>"$work/g5-snapshot-writer.log" 2>&1
terminate_blocker
wait "$reader_pid"
if [[ $(tail -n 1 "$work/g5-old-snapshot.log") != t ]]; then
  echo 'G5 repeatable-read count changed after concurrent commits' >&2
  exit 1
fi
"${psql[@]}" -c 'VACUUM (INDEX_CLEANUP ON) public.g5_docs;' >>"$work/g5-snapshot-writer.log" 2>&1
g5_assert_count 1917

# delete before the reader snapshot so VACUUM has a removable owner on its first page.
"${psql[@]}" -c 'DELETE FROM public.g5_docs WHERE id = 6;' >"$work/g5-owner-delete.log" 2>&1
start_blocker pin-g5-owner-blocker 2
PGAPPNAME=pin-g5-owner-reader PGOPTIONS="$g5_options" "${psql[@]}" -Atq \
  -c 'SELECT pin.g2_inject(14, 1, true);' -c "$g5_query" >"$work/g5-owner-reader.log" 2>&1 &
reader_pid=$!
wait_for_advisory_waiter pin-g5-owner-reader
PGAPPNAME=pin-g5-owner-vacuum PGOPTIONS='-c lock_timeout=0 -c statement_timeout=60s' \
  "${psql[@]}" -c 'VACUUM (INDEX_CLEANUP ON) public.g5_docs;' >"$work/g5-owner-vacuum.log" 2>&1 &
maintenance_pid=$!
wait_for_g5_owner_waiter pin-g5-owner-vacuum
terminate_blocker
wait "$reader_pid"
wait "$maintenance_pid"
if [[ $(tail -n 1 "$work/g5-owner-reader.log") != 1916 ]]; then
  echo 'G5 pinned-owner count changed during VACUUM' >&2
  exit 1
fi
g5_assert_count 1916

# release owner, heap and structural resources on both ERROR and backend death.
expected=1916
for mode in cancel terminate; do
  if [[ $mode == cancel ]]; then
    removed=7
    stage=13
  else
    removed=8
    stage=15
  fi
  expected=$((expected - 1))
  "${psql[@]}" -c "DELETE FROM public.g5_docs WHERE id = $removed;" >>"$work/g5-owner-delete.log" 2>&1
  start_blocker "pin-g5-$mode-blocker" 2
  PGAPPNAME="pin-g5-$mode-reader" PGOPTIONS="$g5_options" "${psql[@]}" -Atq \
    -c "SELECT pin.g2_inject($stage, 1, true);" -c "$g5_query" >"$work/g5-$mode-reader.log" 2>&1 &
  reader_pid=$!
  wait_for_advisory_waiter "pin-g5-$mode-reader"
  PGAPPNAME="pin-g5-$mode-vacuum" PGOPTIONS='-c lock_timeout=0 -c statement_timeout=60s' \
    "${psql[@]}" -c 'VACUUM (INDEX_CLEANUP ON) public.g5_docs;' >"$work/g5-$mode-vacuum.log" 2>&1 &
  maintenance_pid=$!
  wait_for_g5_owner_waiter "pin-g5-$mode-vacuum"
  "${psql[@]}" -Atqc "SELECT pg_${mode}_backend(pid) FROM pg_stat_activity
      WHERE application_name = 'pin-g5-$mode-reader'" >/dev/null
  if wait "$reader_pid"; then
    echo "G5 $mode reader unexpectedly returned success" >&2
    exit 1
  fi
  # the blocker remains held: cleanup must not depend on releasing the pause lock.
  wait "$maintenance_pid"
  terminate_blocker
  g5_assert_count "$expected"
done

# a linked but unpublished insertion must remain uncounted before and after crash.
start_blocker pin-g5-publication-blocker 2
PGAPPNAME=pin-g5-unpublished PGOPTIONS='-c lock_timeout=0 -c statement_timeout=60s' \
  "${psql[@]}" -c "SELECT pin.g2_inject(5, 1, true);
    INSERT INTO public.g5_docs(id, body) VALUES (9999, 'alpha');" >"$work/g5-unpublished.log" 2>&1 &
inserter_pid=$!
wait_for_advisory_waiter pin-g5-unpublished
g5_assert_count "$expected"
"$bin/pg_ctl" -D "$work/data" -m immediate -w stop
wait "$inserter_pid" || true
wait "$blocker_client_pid" || true
"$bin/pg_ctl" -D "$work/data" -l "$work/postgres.log" -w start
g5_assert_count "$expected"
"${psql[@]}" -c 'VACUUM (INDEX_CLEANUP ON) public.g5_docs;' >"$work/g5-recovery-vacuum.log" 2>&1
g5_assert_count "$expected"
