#!/usr/bin/env python3
"""Paired native CPU/page/WAL qualification on disposable PIN tables."""

import argparse
import json
import os
import pathlib
import statistics
import time

import psycopg2


def cpu_ns(pid):
    return int(pathlib.Path(f"/proc/{pid}/schedstat").read_text().split()[0])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--disposable", action="store_true", required=True)
    parser.add_argument("--output", type=pathlib.Path, required=True)
    parser.add_argument("--terms", type=int, default=2000)
    parser.add_argument("--blocks", type=int, default=5)
    parser.add_argument("--queries", type=int, default=100)
    args = parser.parse_args()
    if args.terms < 100 or args.blocks < 2 or args.queries < 10:
        parser.error("terms >= 100, blocks >= 2 and queries >= 10 are required")
    args.output.mkdir(parents=True, exist_ok=False)
    conn = psycopg2.connect(dbname="postgres", host=os.environ["PGHOST"], port=os.environ["PGPORT"])
    conn.autocommit = True
    cur = conn.cursor()
    statements = []

    def execute(sql, fetch=True):
        statements.append(sql)
        cur.execute(sql)
        return cur.fetchall() if fetch and cur.description else []

    pid = execute("SELECT pg_backend_pid()")[0][0]
    revision = execute("SELECT pin.build_revision()")[0][0]
    server = execute("SELECT version()")[0][0]
    settings = execute("SHOW ALL")
    (args.output / "settings.json").write_text(json.dumps(settings, indent=2, default=str))
    execute("SET pin.enable_count_fastpath=off")
    execute("SET pin.enable_grouped_count=off")
    execute("SET pin.enable_grouped_scan=off")
    execute("SET enable_seqscan=off")
    execute("SET enable_indexscan=off")
    samples, builds, plans, checks = [], [], {}, []
    data_dir = pathlib.Path(execute("SHOW data_directory")[0][0])
    try:
        for corpus in ("single", "two", "dense"):
            for packed in (False, True):
                name = f"pin_pc_{corpus}_{'on' if packed else 'off'}"
                execute(f"CREATE TABLE {name}(id integer PRIMARY KEY, body text) WITH (autovacuum_enabled=false)", False)
                rows = args.terms if corpus == "single" else args.terms * 2
                if corpus == "single":
                    body = "'word' || lpad(i::text, 5, '0')"
                elif corpus == "two":
                    body = f"'word' || lpad((((i-1)%{args.terms})+1)::text, 5, '0')"
                else:
                    body = f"'echo word' || lpad((((i-1)%{args.terms})+1)::text, 5, '0')"
                execute(f"INSERT INTO {name} SELECT i, {body} FROM generate_series(1,{rows}) i", False)
                execute(f"SET pin.enable_packed_postings={'on' if packed else 'off'}", False)
                before_cpu = cpu_ns(pid)
                before_lsn = execute("SELECT pg_current_wal_insert_lsn()")[0][0]
                start = time.perf_counter_ns()
                execute(f"CREATE INDEX {name}_pin ON {name} USING pin(body)", False)
                elapsed_ns = time.perf_counter_ns() - start
                after_cpu = cpu_ns(pid)
                after_lsn = execute("SELECT pg_current_wal_insert_lsn()")[0][0]
                cur.execute("SELECT pg_wal_lsn_diff(%s, %s)", (after_lsn, before_lsn))
                wal_bytes = int(cur.fetchone()[0])
                index_bytes = execute(f"SELECT pg_relation_size('{name}_pin')")[0][0]
                builds.append(dict(corpus=corpus, packed=packed, rows=rows, index_bytes=index_bytes,
                                   cpu_ns=after_cpu - before_cpu, elapsed_ns=elapsed_ns,
                                   wal_bytes=wal_bytes))
                execute(f"VACUUM (ANALYZE, PARALLEL 0) {name}", False)
                execute("CHECKPOINT", False)
                relpath = execute(f"SELECT pg_relation_filepath('{name}_pin')")[0][0]
                raw = (data_dir / relpath).read_bytes()
                if len(raw) % 8192:
                    raise AssertionError("unaligned index relation")
                census = {}
                for page in range(0, len(raw), 8192):
                    image = raw[page:page + 8192]
                    if image[24:28] != b"PIN2":
                        raise AssertionError(f"missing PIN2 page at {page // 8192}")
                    kind = image[30]
                    key = str(kind)
                    census[key] = census.get(key, 0) + 1
                builds[-1]["post_vacuum_census"] = census
                builds[-1]["post_vacuum_bytes"] = len(raw)

            word = "echo" if corpus == "dense" else "word00001"
            for packed in (False, True):
                name = f"pin_pc_{corpus}_{'on' if packed else 'off'}"
                sql = f"SELECT count(*) FROM {name} WHERE body OPERATOR(pin.@@@) pin.parse_query('{word}')"
                row_sql = f"SELECT id FROM {name} WHERE body OPERATOR(pin.@@@) pin.parse_query('{word}') ORDER BY id"
                plan = execute(f"EXPLAIN (ANALYZE, BUFFERS, TIMING OFF, FORMAT JSON) {sql}")[0][0]
                plans[name] = plan
                if "Bitmap Index Scan" not in json.dumps(plan):
                    raise AssertionError(f"{name} did not use PIN index")
                indexed = execute(row_sql)
                execute("SET enable_seqscan=on", False)
                execute("SET enable_bitmapscan=off", False)
                oracle = execute(row_sql)
                execute("SET enable_seqscan=off", False)
                execute("SET enable_bitmapscan=on", False)
                if indexed != oracle:
                    raise AssertionError(f"{name} disagrees with sequential oracle")
                checks.append(dict(corpus=corpus, packed=packed, matched=len(indexed)))

            for block in range(args.blocks):
                for packed in ((False, True) if block % 2 == 0 else (True, False)):
                    name = f"pin_pc_{corpus}_{'on' if packed else 'off'}"
                    sql = f"SELECT count(*) FROM {name} WHERE body OPERATOR(pin.@@@) pin.parse_query('{word}')"
                    for _ in range(3):
                        execute(sql)
                    before = cpu_ns(pid)
                    latency = []
                    answers = []
                    for _ in range(args.queries):
                        start = time.perf_counter_ns()
                        answers.append(execute(sql)[0][0])
                        latency.append(time.perf_counter_ns() - start)
                    used = cpu_ns(pid) - before
                    if len(set(answers)) != 1:
                        raise AssertionError("unstable count")
                    samples.append(dict(corpus=corpus, packed=packed, block=block, answers=answers[0],
                                        cpu_ns_per_query=used / args.queries,
                                        latency_ns=latency))
                    (args.output / "samples.json").write_text(json.dumps(samples, indent=2))
    finally:
        for corpus in ("single", "two", "dense"):
            for packed in (False, True):
                name = f"pin_pc_{corpus}_{'on' if packed else 'off'}"
                execute(f"DROP TABLE IF EXISTS {name}", False)
        (args.output / "statements.sql").write_text(";\n".join(statements) + ";\n")
        conn.close()
    summary = [dict(corpus=corpus, packed=packed,
                    median_cpu_ns_per_query=statistics.median(s["cpu_ns_per_query"] for s in samples
                                                       if s["corpus"] == corpus and s["packed"] == packed))
               for corpus in ("single", "two", "dense") for packed in (False, True)]
    (args.output / "result.json").write_text(json.dumps(dict(revision=revision, server=server,
        terms=args.terms, blocks=args.blocks, queries=args.queries, builds=builds,
        summary=summary, checks=checks, plans=plans), indent=2, default=str) + "\n")


if __name__ == "__main__":
    main()
