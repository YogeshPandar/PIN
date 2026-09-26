import json, os, subprocess, sys, time
from pathlib import Path
sys.path.insert(0, '/home/reing/projects/fts0/PIN/tools')
from g9_profile import Session
out = Path('/tmp/pin-reuse-final-profile')
out.mkdir(exist_ok=False)
with Session(Path('/usr/lib/postgresql/18/bin/psql'), out/'psql.stderr','pin_legacy') as s:
    s.execute("CREATE TABLE pin_selected_profile_docs AS SELECT i AS id, repeat('echo ',10000)||'alpha beta' AS body FROM generate_series(1,256)i;")
    try:
        s.execute('CREATE INDEX ON pin_selected_profile_docs USING pin(body);')
        s.execute('VACUUM ANALYZE pin_selected_profile_docs;')
        s.execute('SET pin.enable_phrase_positions=on;')
        pid=int(s.execute('SELECT pg_backend_pid();'))
        sql="SELECT count(*) FROM pin_selected_profile_docs WHERE body OPERATOR(pin.@@@) pin.parse_query('\"alpha beta\"');"
        s.execute(sql)
        pmu=subprocess.run(['sudo','-n','perf','stat','-e','cycles,instructions','--','true'],capture_output=True,text=True)
        (out/'pmu.txt').write_text(pmu.stdout+pmu.stderr)
        log=(out/'perf.stderr').open('w')
        proc=subprocess.Popen(['sudo','-n','perf','record','-e','cpu-clock','-F','199','-g','-p',str(pid),'-o',str(out/'perf.data'),'--','sleep','10'],stdout=log,stderr=log)
        deadline=time.monotonic()+10
        n=0
        while time.monotonic()<deadline and proc.poll() is None:
            if s.execute(sql) != '256': raise AssertionError('profile result differs')
            n+=1
        code=proc.wait(timeout=15)
        log.close()
        (out/'profile.json').write_text(json.dumps({'sql':sql,'queries':n,'returncode':code,'revision':s.execute('SELECT pin.build_revision();'),'scope':'one warm serial PIN backend; profiling perturbs timings'},indent=2)+'\n')
        subprocess.run(['sudo','-n','chown',str(os.getuid())+':'+str(os.getgid()),str(out/'perf.data')],check=True)
        with (out/'report.txt').open('w') as report:
            subprocess.run(['perf','report','--stdio','--no-children','-i',str(out/'perf.data')],stdout=report,stderr=subprocess.STDOUT,check=True)
    finally:
        s.execute('DROP TABLE pin_selected_profile_docs;')
