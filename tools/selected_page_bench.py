#!/usr/bin/env python3
"""Measure grouped selective-page reads against an equivalent stored-vector GIN."""

import argparse
import json
import os
from pathlib import Path
import statistics

import psycopg2


def cpu_ns(pid):
    return int(Path(f"/proc/{pid}/schedstat").read_text().split()[0])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--disposable", action="store_true", required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--blocks", type=int, default=5)
    parser.add_argument("--queries", type=int, default=100)
    args = parser.parse_args()
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
    execute("SET pin.enable_grouped_storage=on")
    execute("SET pin.enable_grouped_scan=on")
    execute("SET pin.enable_count_fastpath=off")
    execute("SET pin.enable_grouped_count=off")
    execute("SET pin.enable_exact_bitmap=on")
    execute("SET enable_seqscan=off")
    execute("SET enable_indexscan=off")
    checks, plans, samples = [], {}, []
    cases = {
        "broad": ("wide", "wide"),
        "rare": ("rare", "rare"),
        "and": ("wide AND rare", "wide & rare"),
    }
    try:
        execute("CREATE TABLE pin_selected_page(body text) "
                "WITH (autovacuum_enabled=false,fillfactor=100)")
        execute("INSERT INTO pin_selected_page "
                "SELECT CASE WHEN i=1 THEN 'wide rare' ELSE 'wide' END "
                "FROM generate_series(1,100000) i")
        heap_pages = execute("SELECT pg_relation_size('pin_selected_page') / 8192")[0][0]
        execute("CREATE INDEX pin_selected_page_pin ON pin_selected_page USING pin(body)")
        execute("CREATE INDEX pin_selected_page_gin ON pin_selected_page "
                "USING gin(to_tsvector('simple',body))")
        execute("ANALYZE pin_selected_page")
        execute("CHECKPOINT")
        for case, (source, gin) in cases.items():
            answers = []
            for engine in ("pin", "gin"):
                clause = (f"body OPERATOR(pin.@@@) pin.parse_query('{source}')" if engine == "pin"
                          else f"to_tsvector('simple',body) @@ to_tsquery('simple','{gin}')")
                sql = f"SELECT count(*) FROM pin_selected_page WHERE {clause}"
                row_sql = f"SELECT ctid FROM pin_selected_page WHERE {clause} ORDER BY ctid"
                plan = execute(f"EXPLAIN (ANALYZE,BUFFERS,TIMING OFF,FORMAT JSON) {sql}")[0][0]
                if "Bitmap Index Scan" not in json.dumps(plan):
                    raise AssertionError(f"no bitmap index scan for {case}/{engine}")
                plans[f"{case}/{engine}"] = plan
                indexed = execute(row_sql)
                execute("SET enable_seqscan=on")
                execute("SET enable_bitmapscan=off")
                oracle = execute(row_sql)
                execute("SET enable_seqscan=off")
                execute("SET enable_bitmapscan=on")
                if indexed != oracle:
                    raise AssertionError(f"oracle mismatch for {case}/{engine}")
                answers.append(indexed)
                checks.append(dict(case=case, engine=engine, rows=len(indexed)))
            if answers[0] != answers[1]:
                raise AssertionError(f"PIN/GIN mismatch for {case}")
            for block in range(args.blocks):
                for engine in (("pin", "gin") if block % 2 == 0 else ("gin", "pin")):
                    source, gin = cases[case]
                    clause = (f"body OPERATOR(pin.@@@) pin.parse_query('{source}')" if engine == "pin"
                              else f"to_tsvector('simple',body) @@ to_tsquery('simple','{gin}')")
                    sql = f"SELECT count(*) FROM pin_selected_page WHERE {clause}"
                    for _ in range(4):
                        execute(sql)
                    before = cpu_ns(pid)
                    for _ in range(args.queries):
                        if execute(sql)[0][0] != len(answers[0]):
                            raise AssertionError("unstable count")
                    samples.append(dict(case=case, engine=engine, block=block,
                                        cpu_ns_per_query=(cpu_ns(pid) - before) / args.queries))
                    (args.output / "samples.json").write_text(json.dumps(samples, indent=2))
    finally:
        execute("DROP TABLE IF EXISTS pin_selected_page")
        (args.output / "statements.sql").write_text(";\n".join(statements) + ";\n")
        conn.close()
    summary = [dict(case=case, engine=engine, median_cpu_ns_per_query=statistics.median(
        sample["cpu_ns_per_query"] for sample in samples
        if sample["case"] == case and sample["engine"] == engine))
        for case in cases for engine in ("pin", "gin")]
    (args.output / "result.json").write_text(json.dumps(dict(
        revision=revision, heap_pages=heap_pages, blocks=args.blocks, queries=args.queries,
        checks=checks, plans=plans, summary=summary), indent=2) + "\n")


if __name__ == "__main__":
    main()
