#!/usr/bin/env python3
"""Compare direct-TID build sealing with canonical postings in one PG backend."""

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
    parser.add_argument("--rows", type=int, default=16000)
    parser.add_argument("--terms", type=int, default=2000)
    parser.add_argument("--blocks", type=int, default=5)
    parser.add_argument("--queries", type=int, default=100)
    parser.add_argument("--grouped", action="store_true",
                        help="build and read grouped page masks on the PIN indexes")
    parser.add_argument("--packed", action="store_true",
                        help="also enable experimental two-owner dictionary packing")
    args = parser.parse_args()
    if args.rows < args.terms * 2 or args.terms < 100 or args.blocks < 2 or args.queries < 10:
        parser.error("rows >= terms * 2, terms >= 100, blocks >= 2, queries >= 10 required")
    args.output.mkdir(parents=True, exist_ok=False)
    conn = psycopg2.connect(dbname="postgres", host=os.environ["PGHOST"], port=os.environ["PGPORT"])
    conn.autocommit = True
    cur = conn.cursor()
    statements = []

    def execute(sql):
        statements.append(sql)
        cur.execute(sql)
        return cur.fetchall() if cur.description else []

    pid = execute("SELECT pg_backend_pid()")[0][0]
    revision = execute("SELECT pin.build_revision()")[0][0]
    settings = execute("SHOW ALL")
    (args.output / "settings.json").write_text(json.dumps(settings, indent=2, default=str))
    execute(f"SET pin.enable_packed_postings={'on' if args.packed else 'off'}")
    execute(f"SET pin.enable_grouped_storage={'on' if args.grouped else 'off'}")
    execute("SET pin.enable_direct_tid_segments=off")
    execute("SET pin.enable_count_fastpath=off")
    execute("SET pin.enable_grouped_count=off")
    execute(f"SET pin.enable_grouped_scan={'on' if args.grouped else 'off'}")
    execute("SET enable_seqscan=off")
    execute("SET enable_indexscan=off")
    builds, plans, checks, samples = [], {}, [], []
    data_dir = pathlib.Path(execute("SHOW data_directory")[0][0])
    cases = {"broad": ("echo", "echo"),
             "rare": ("word00001", "word00001"),
             "and": ("echo AND word00001", "echo & word00001")}
    try:
        for direct in (False, True):
            name = f"pin_direct_build_{'on' if direct else 'off'}"
            execute(f"CREATE TABLE {name}(id integer PRIMARY KEY, body text) WITH (autovacuum_enabled=false)")
            execute(f"INSERT INTO {name} SELECT i, 'echo word' || lpad((((i-1)%{args.terms})+1)::text,5,'0') FROM generate_series(1,{args.rows}) i")
            execute(f"SET pin.enable_direct_tid_build={'on' if direct else 'off'}")
            before_lsn = execute("SELECT pg_current_wal_insert_lsn()")[0][0]
            before = cpu_ns(pid)
            execute(f"CREATE INDEX {name}_pin ON {name} USING pin(body)")
            build_cpu = cpu_ns(pid) - before
            after_lsn = execute("SELECT pg_current_wal_insert_lsn()")[0][0]
            cur.execute("SELECT pg_wal_lsn_diff(%s,%s)", (after_lsn, before_lsn))
            wal_bytes = int(cur.fetchone()[0])
            execute(f"ANALYZE {name}")
            execute("CHECKPOINT")
            index_bytes = execute(f"SELECT pg_relation_size('{name}_pin')")[0][0]
            relpath = execute(f"SELECT pg_relation_filepath('{name}_pin')")[0][0]
            raw = (data_dir / relpath).read_bytes()
            if len(raw) % 8192:
                raise AssertionError("unaligned index relation")
            census = {}
            for offset in range(0, len(raw), 8192):
                image = raw[offset:offset + 8192]
                if image[24:28] != b"PIN2":
                    raise AssertionError(f"missing PIN2 at {offset // 8192}")
                key = str(image[30])
                census[key] = census.get(key, 0) + 1
            builds.append(dict(direct=direct, index_bytes=index_bytes, build_cpu_ns=build_cpu,
                               build_wal_bytes=wal_bytes, census=census))

        execute("CREATE TABLE pin_direct_build_gin(id integer PRIMARY KEY, body text, "
                "search_vector tsvector GENERATED ALWAYS AS (to_tsvector('simple',body)) STORED) "
                "WITH (autovacuum_enabled=false)")
        execute(f"INSERT INTO pin_direct_build_gin(id,body) SELECT i, 'echo word' || "
                f"lpad((((i-1)%{args.terms})+1)::text,5,'0') FROM generate_series(1,{args.rows}) i")
        execute("CREATE INDEX pin_direct_build_gin_idx ON pin_direct_build_gin USING gin(search_vector)")
        execute("ANALYZE pin_direct_build_gin")
        builds.append(dict(direct="gin_stored", index_bytes=execute(
            "SELECT pg_relation_size('pin_direct_build_gin_idx')")[0][0]))

        for case, (source, gin_source) in cases.items():
            identities = []
            for direct in (False, True, "gin_stored"):
                name = ("pin_direct_build_gin" if direct == "gin_stored" else
                        f"pin_direct_build_{'on' if direct else 'off'}")
                clause = (f"search_vector @@ to_tsquery('simple','{gin_source}')"
                          if direct == "gin_stored" else
                          f"body OPERATOR(pin.@@@) pin.parse_query('{source}')")
                sql = f"SELECT count(*) FROM {name} WHERE {clause}"
                row_sql = f"SELECT id FROM {name} WHERE {clause} ORDER BY id"
                plan = execute(f"EXPLAIN (ANALYZE,BUFFERS,TIMING OFF,FORMAT JSON) {sql}")[0][0]
                if "Bitmap Index Scan" not in json.dumps(plan):
                    raise AssertionError(f"no bitmap index scan: {case}/{direct}")
                plans[f"{case}/{direct}"] = plan
                indexed = execute(row_sql)
                execute("SET enable_seqscan=on")
                execute("SET enable_bitmapscan=off")
                oracle = execute(row_sql)
                execute("SET enable_seqscan=off")
                execute("SET enable_bitmapscan=on")
                if indexed != oracle:
                    raise AssertionError(f"oracle mismatch: {case}/{direct}")
                identities.append(indexed)
                checks.append(dict(case=case, direct=direct, rows=len(indexed)))
            if identities[0] != identities[1] or identities[0] != identities[2]:
                raise AssertionError(f"build-mode mismatch: {case}")
            for block in range(args.blocks):
                order = (False, True, "gin_stored") if block % 2 == 0 else ("gin_stored", True, False)
                for direct in order:
                    name = ("pin_direct_build_gin" if direct == "gin_stored" else
                            f"pin_direct_build_{'on' if direct else 'off'}")
                    clause = (f"search_vector @@ to_tsquery('simple','{gin_source}')"
                              if direct == "gin_stored" else
                              f"body OPERATOR(pin.@@@) pin.parse_query('{source}')")
                    sql = f"SELECT count(*) FROM {name} WHERE {clause}"
                    for _ in range(4):
                        execute(sql)
                    before = cpu_ns(pid)
                    for _ in range(args.queries):
                        answer = execute(sql)[0][0]
                        if answer != len(identities[0]):
                            raise AssertionError("unstable count")
                    used = cpu_ns(pid) - before
                    samples.append(dict(case=case, direct=direct, block=block,
                                        cpu_ns_per_query=used / args.queries))
                    (args.output / "samples.json").write_text(json.dumps(samples, indent=2))
    finally:
        for direct in (False, True):
            execute(f"DROP TABLE IF EXISTS pin_direct_build_{'on' if direct else 'off'}")
        execute("DROP TABLE IF EXISTS pin_direct_build_gin")
        (args.output / "statements.sql").write_text(";\n".join(statements) + ";\n")
        conn.close()
    summary = [dict(case=case, direct=direct,
                    median_cpu_ns_per_query=statistics.median(
                        s["cpu_ns_per_query"] for s in samples
                        if s["case"] == case and s["direct"] == direct))
               for case in cases for direct in (False, True, "gin_stored")]
    (args.output / "result.json").write_text(json.dumps(dict(revision=revision,
        rows=args.rows, terms=args.terms, blocks=args.blocks, queries=args.queries,
        grouped=args.grouped, packed=args.packed,
        builds=builds, plans=plans, checks=checks, summary=summary), indent=2) + "\n")


if __name__ == "__main__":
    main()
