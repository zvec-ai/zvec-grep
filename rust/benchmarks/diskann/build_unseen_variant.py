"""Build and archive a local zvec candidate without replacing the baseline SDK."""
import argparse,hashlib,json,os,shlex,shutil,subprocess
from pathlib import Path

def main():
    p=argparse.ArgumentParser();p.add_argument('--base',type=Path,required=True);p.add_argument('--variant',required=True)
    a=p.parse_args();out=a.base/a.variant;out.mkdir(exist_ok=False)
    source=Path('/Users/xingrui/Documents/workspace/zvec/zvec_grep_diskann/zvec')
    build=Path('/tmp/zvec-local-cohere-20261104/build')
    env=dict(os.environ,PATH='/opt/homebrew/bin:/usr/bin:/bin')
    with (out/'build.txt').open('w') as log:
        subprocess.run(['/opt/homebrew/bin/ninja','-C',str(build),'lib/libzvec_core.a'],env=env,stdout=log,stderr=log,check=True)
        meta=json.loads(Path('/tmp/zvec-node-cohere-20261104/build.json').read_text())
        cmd=shlex.split(meta['direct_link_command'].split(' && ')[1]);cmd[cmd.index('-o')+1]=str(out/'libzvec_c_api.dylib')
        subprocess.run(cmd,cwd=build,env=env,stdout=log,stderr=log,check=True)
        subprocess.run(['/usr/bin/strip','-x',str(out/'libzvec_c_api.dylib')],stdout=log,stderr=log,check=True)
    (out/'source.patch').write_text(subprocess.check_output(['git','diff','7d3a413e'],cwd=source,text=True))
    lib=out/'libzvec_c_api.dylib'
    (out/'build.json').write_text(json.dumps(dict(link_command=cmd,source_base='7d3a413e',library_sha256=hashlib.sha256(lib.read_bytes()).hexdigest()),indent=2))
    current=a.base/'rust-target/release/libzvec_c_api.dylib'
    shutil.copy2(lib,current.with_suffix('.next'));os.replace(current.with_suffix('.next'),current)
    print('BUILT',a.variant,flush=True)

if __name__=='__main__':main()
