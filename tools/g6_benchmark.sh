#!/usr/bin/env bash
set -euo pipefail

: "${PGRX_PG_CONFIG_PATH:?set the PostgreSQL 18.6 pg_config path}"
: "${PGDATABASE:?use a dedicated benchmark database}"

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
bin=$("$PGRX_PG_CONFIG_PATH" --bindir)
psql=("$bin/psql" -X -v ON_ERROR_STOP=1)
pgbench="$bin/pgbench"

if [[ $("$PGRX_PG_CONFIG_PATH" --version) != 'PostgreSQL 18.6' ]]; then
  echo 'G6 benchmark requires PostgreSQL 18.6.' >&2
  exit 2
fi

clients=${PIN_G6_CLIENTS:-8}
threads=${PIN_G6_THREADS:-4}
seconds=${PIN_G6_SECONDS:-30}
warmup=${PIN_G6_WARMUP_SECONDS:-5}
seed=${PIN_G6_SEED:-6001}
mode=${PIN_G6_MODE:-core}
setup=${PIN_G6_SETUP:-1}
rate=${PIN_G6_RATE:-}
latency_limit=${PIN_G6_LATENCY_LIMIT_MS:-250}
label=${PIN_G6_LABEL:-run}

for value in "$clients" "$threads" "$seconds" "$warmup" "$seed" "$latency_limit"; do
  [[ $value =~ ^[0-9]+$ ]] || {
    echo "invalid numeric benchmark setting: $value" >&2
    exit 2
  }
done
if (( clients == 0 || threads == 0 || threads > clients || seconds == 0 )); then
  echo 'invalid G6 client, thread or duration setting.' >&2
  exit 2
fi
if [[ -n $rate && ! $rate =~ ^[0-9]+([.][0-9]+)?$ ]]; then
  echo 'PIN_G6_RATE must be a positive numeric rate.' >&2
  exit 2
fi

server_version=$("${psql[@]}" -Atqc 'SHOW server_version_num')
if [[ $server_version != 180006 ]]; then
  echo "server version mismatch: $server_version" >&2
  exit 2
fi
if [[ $("${psql[@]}" -Atqc "SELECT count(*) FROM pg_catalog.pg_extension WHERE extname = 'pin'") != 1 ]]; then
  echo 'install and create extension pin before benchmarking.' >&2
  exit 2
fi

case "$mode" in
  core)
    pin_options='-c pin.enable_count_fastpath=off -c enable_seqscan=off -c enable_bitmapscan=on -c enable_indexscan=off -c enable_indexonlyscan=off'
    ;;
  count-oracle)
    pin_options='-c pin.enable_count_fastpath=on -c pin.enable_count_vm=off -c pin.enable_count_recheck=off -c enable_seqscan=off -c enable_bitmapscan=on'
    ;;
  count-stream)
    pin_options='-c pin.enable_count_fastpath=on -c pin.enable_count_vm=off -c pin.enable_count_recheck=on -c enable_seqscan=off -c enable_bitmapscan=on'
    ;;
  *)
    echo "unsupported PIN_G6_MODE: $mode" >&2
    exit 2
    ;;
esac

head=$(git -C "$root" rev-parse HEAD)
short=${head:0:12}
output=${PIN_G6_OUTPUT:-"$root/.artifacts/g6/${label}-${mode}-${short}"}
if [[ -e $output ]]; then
  echo "benchmark output already exists: $output" >&2
  exit 2
fi
mkdir -p "$output"

{
  echo "commit=$head"
  echo "label=$label"
  echo "mode=$mode"
  echo "clients=$clients"
  echo "threads=$threads"
  echo "seconds=$seconds"
  echo "warmup_seconds=$warmup"
  echo "seed=$seed"
  echo "rate=${rate:-unthrottled}"
  "$PGRX_PG_CONFIG_PATH" --version
  "$PGRX_PG_CONFIG_PATH" --configure
  "$pgbench" --version
  uname -a
  command -v lscpu >/dev/null && lscpu || true
  command -v rustc >/dev/null && rustc -Vv || true
  sha256sum "$root/Cargo.lock"
} >"$output/environment.txt"

"${psql[@]}" -Atqc "
SELECT name || '=' || setting
FROM pg_catalog.pg_settings
WHERE name = ANY (ARRAY[
  'shared_buffers','work_mem','maintenance_work_mem','effective_cache_size',
  'effective_io_concurrency','maintenance_io_concurrency','random_page_cost',
  'seq_page_cost','cpu_tuple_cost','cpu_index_tuple_cost','cpu_operator_cost',
  'fsync','full_page_writes','synchronous_commit','autovacuum'
])
ORDER BY name;
" >"$output/settings.txt"

if (( setup == 1 )); then
  if [[ $("${psql[@]}" -Atqc "SELECT to_regclass('public.pin_g6_bench') IS NULL") != t ]]; then
    echo 'public.pin_g6_bench already exists; use a fresh database or PIN_G6_SETUP=0.' >&2
    exit 2
  fi
  "${psql[@]}" -f "$root/benches/g6/setup.sql" >"$output/setup.log"
elif [[ $("${psql[@]}" -Atqc "SELECT to_regclass('public.pin_g6_bench') IS NOT NULL") != t ]]; then
  echo 'PIN_G6_SETUP=0 requires an existing public.pin_g6_bench fixture.' >&2
  exit 2
fi

query="SELECT count(*) FROM ONLY public.pin_g6_bench WHERE body OPERATOR(pin.@@@) pin.parse_query('alpha')"
sequential=$(PGOPTIONS='-c pin.enable_count_fastpath=off -c enable_seqscan=on -c enable_bitmapscan=off -c enable_indexscan=off -c enable_indexonlyscan=off'   "${psql[@]}" -Atqc "$query")
indexed=$(PGOPTIONS='-c pin.enable_count_fastpath=off -c enable_seqscan=off -c enable_bitmapscan=on -c enable_indexscan=off -c enable_indexonlyscan=off'   "${psql[@]}" -Atqc "$query")
if [[ $sequential != "$indexed" ]]; then
  echo "result mismatch before timing: sequential=$sequential indexed=$indexed" >&2
  exit 1
fi
printf 'sequential=%s\nindexed=%s\n' "$sequential" "$indexed" >"$output/correctness.txt"

PGOPTIONS="$pin_options" "${psql[@]}" -Atqc "EXPLAIN (ANALYZE, BUFFERS, WAL, FORMAT JSON) $query"   >"$output/plan.json"

snapshot() {
  local name=$1
  "${psql[@]}" -AtF $'\t' -c "
SELECT clock_timestamp(),
       wal_bytes,
       pg_relation_size('public.pin_g6_bench'),
       pg_indexes_size('public.pin_g6_bench'),
       n_tup_ins,
       n_tup_upd,
       n_tup_del,
       n_dead_tup,
       vacuum_count,
       autovacuum_count
FROM pg_catalog.pg_stat_wal,
     pg_catalog.pg_stat_user_tables
WHERE relid = 'public.pin_g6_bench'::regclass;
" >"$output/$name.tsv"
}

common=(
  -n
  -M prepared
  -c "$clients"
  -j "$threads"
  -T "$seconds"
  -P 5
  -r
  -l
  --failures-detailed
  --random-seed="$seed"
)
scheduled=()
if [[ -n $rate ]]; then
  common+=(-R "$rate" -L "$latency_limit")
  scheduled=(--scheduled)
fi

run_workload() {
  local name=$1
  shift
  local prefix="$output/$name-log"
  snapshot "$name-before"
  (
    cd "$output"
    PGOPTIONS="$pin_options" "$pgbench" "${common[@]}" --log-prefix="$prefix" "$@" "$PGDATABASE"
  ) >"$output/$name.txt" 2>&1
  snapshot "$name-after"
  python3 "$root/tools/g6_latency.py" "${scheduled[@]}" "$prefix".* >"$output/$name-latency.json"
}

if (( warmup > 0 )); then
  PGOPTIONS="$pin_options" "$pgbench" -n -M prepared -c "$clients" -j "$threads"     -T "$warmup" --random-seed="$seed" -f "$root/benches/g6/count.sql" "$PGDATABASE"     >"$output/warmup.txt" 2>&1
fi

run_workload read -f "$root/benches/g6/count.sql"
run_workload write -f "$root/benches/g6/write.sql"
run_workload mixed   -f "$root/benches/g6/count.sql@95"   -f "$root/benches/g6/write.sql@5"

"${psql[@]}" -Atqc "
SELECT pg_size_pretty(pg_relation_size('public.pin_g6_bench')),
       pg_size_pretty(pg_indexes_size('public.pin_g6_bench')),
       n_live_tup,
       n_dead_tup,
       vacuum_count,
       autovacuum_count
FROM pg_catalog.pg_stat_user_tables
WHERE relid = 'public.pin_g6_bench'::regclass;
" >"$output/final-state.txt"

printf '%s\n' "$output"
