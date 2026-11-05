"""Build independent benchmark executables with verified absolute dylib paths."""
import argparse,hashlib,json,os,shutil,subprocess
from pathlib import Path

HERE=Path(__file__).resolve().parent
def main():
    p=argparse.ArgumentParser();p.add_argument('--base',type=Path,required=True)
    p.add_argument('--baseline-library',type=Path,default=Path('/tmp/zvec-unseen-opt-20261105/final/libzvec_c_api.dylib'))
    a=p.parse_args();b=a.base.resolve();library=b/'final/libzvec_c_api.dylib';baseline=a.baseline_library.resolve()
    assert library.is_file() and baseline.is_file()
    env=dict(os.environ,ZVEC_LIB_DIR=str(library.parent),ZVEC_AUTO_BUILD='0',CARGO_TARGET_DIR=str(b/'rust-target'),RUSTFLAGS='-C link-arg=-Wl,-headerpad_max_install_names')
    cmd=['cargo','build','--release','--manifest-path',str(HERE/'harness/Cargo.toml'),'--features','experimental-pq','--bin','memory-bench']
    with (b/'rust-build.log').open('w') as log:subprocess.run(cmd,env=env,stdout=log,stderr=log,check=True)
    candidate=b/'rust-target/release/memory-bench';old=b/'baseline/memory-bench';old.parent.mkdir(exist_ok=True)
    shutil.copy2(candidate,old)
    records={}
    for name,exe,lib in [('baseline',old,baseline),('candidate',candidate,library)]:
        otool=subprocess.check_output(['otool','-L',str(exe)],text=True)
        loaded=next(line.strip().split(' (compatibility version')[0] for line in otool.splitlines()[1:] if 'libzvec_c_api.dylib' in line)
        if loaded!=str(lib):subprocess.run(['install_name_tool','-change',loaded,str(lib),str(exe)],check=True)
        subprocess.run(['codesign','--force','--sign','-',str(exe)],check=True)
        subprocess.run(['codesign','--verify',str(exe)],check=True)
        records[name]=dict(binary=str(exe),binary_sha256=hashlib.sha256(exe.read_bytes()).hexdigest(),library=str(lib),otool=subprocess.check_output(['otool','-L',str(exe)],text=True))
    (b/'loader-isolation.json').write_text(json.dumps(records,indent=2)+'\n')

if __name__=='__main__':main()
