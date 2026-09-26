import subprocess,os
from pathlib import Path
repo=Path('/home/reing/projects/fts0/PIN')
runs=[('base-long',False,256,10000),('reuse-long',True,256,10000),('base-short',False,2048,32),('reuse-short',True,2048,32),('reuse-repeat',True,256,10000),('base-repeat',False,256,10000)]
for name,reuse,rows,tokens in runs:
 stem='pin-reuse-final' if reuse else 'pin-dense'
 env=dict(os.environ,PGHOST='/tmp/'+stem+'-socket',PGPORT='55498' if reuse else '55497',PGDATABASE='postgres')
 prefix='/tmp/page-reuse-'+name
 with open(prefix+'.log','w') as out:
  subprocess.run(['python3','tools/fragment_phrase_bench.py','--disposable','--stored-control','--rows',str(rows),'--tokens',str(tokens),'--blocks','6','--queries','100','--output',prefix],cwd=repo,env=env,stdout=out,stderr=subprocess.STDOUT,check=True)
 print(name+' complete',flush=True)
with open('/tmp/pin-reuse-final-profile.log','w') as out:
 subprocess.run(['python3','/tmp/pin-reuse-final-profile.py'],cwd=repo,env=dict(os.environ,PGHOST='/tmp/pin-reuse-final-socket',PGPORT='55498',PGDATABASE='postgres'),stdout=out,stderr=subprocess.STDOUT,check=True)
