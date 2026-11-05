"""Build the DB, core, and turbo archives, then isolate the candidate's absolute dylib."""
import argparse,hashlib,json,os,shlex,shutil,subprocess
from pathlib import Path

def sha(p):
    with Path(p).open('rb') as f:return hashlib.file_digest(f,'sha256').hexdigest()
def main():
    p=argparse.ArgumentParser();p.add_argument('--base',type=Path,required=True);p.add_argument('--variant',required=True);a=p.parse_args()
    out=a.base/a.variant;out.mkdir(exist_ok=False);lib=out/'libzvec_c_api.dylib';exe=out/'memory-bench'
    src=Path('/Users/xingrui/Documents/workspace/zvec/zvec_grep_diskann/zvec');build=Path('/tmp/zvec-local-cohere-20261104/build')
    env=dict(os.environ,PATH='/opt/homebrew/bin:/usr/bin:/bin')
    with (out/'build.log').open('w') as log:
        subprocess.run(['/opt/homebrew/bin/ninja','-C',str(build),'lib/libzvec.a','lib/libzvec_core.a','lib/libzvec_turbo.a'],env=env,stdout=log,stderr=log,check=True)
        meta=json.loads(Path('/tmp/zvec-node-cohere-20261104/build.json').read_text());cmd=shlex.split(meta['direct_link_command'].split(' && ')[1]);cmd[cmd.index('-o')+1]=str(lib);cmd[cmd.index('-install_name')+1]=str(lib)
        subprocess.run(cmd,cwd=build,env=env,stdout=log,stderr=log,check=True)
        rust_env=dict(os.environ,ZVEC_LIB_DIR=str(out),ZVEC_AUTO_BUILD='0',CARGO_TARGET_DIR=str(a.base/'rust-target'),RUSTFLAGS='-C link-arg=-Wl,-headerpad_max_install_names')
        subprocess.run(['/Users/xingrui/.cargo/bin/cargo','build','--release','--manifest-path',str(Path(__file__).resolve().parent/'harness/Cargo.toml'),'--features','experimental-pq','--bin','memory-bench'],env=rust_env,stdout=log,stderr=log,check=True)
        shutil.copy2(a.base/'rust-target/release/memory-bench',exe)
        assert str(lib) in subprocess.check_output(['otool','-L',str(exe)],text=True)
        for f in [exe,lib]:subprocess.run(['codesign','--verify',str(f)],stdout=log,stderr=log,check=True)
    paths=['src/core/algorithm/diskann/diskann_indexer.h','src/core/algorithm/diskann/diskann_dist_calculator.h','src/core/algorithm/diskann/diskann_indexer.cc','src/core/algorithm/diskann/diskann_file_reader.h','src/core/algorithm/diskann/diskann_file_reader.cc','src/core/algorithm/diskann/diskann_read_executor.h','src/core/algorithm/diskann/diskann_read_cache.h','src/turbo/quantizer/pq_int8_quantizer/pq_int8_quantizer.h','src/turbo/quantizer/pq_int8_quantizer/pq_int8_quantizer.cc']
    (out/'source.patch').write_text(subprocess.check_output(['git','diff','6473f807'],cwd=src,text=True))
    (out/'build.json').write_text(json.dumps(dict(source_base='6473f807',source_commit=subprocess.check_output(['git','rev-parse','HEAD'],cwd=src,text=True).strip(),source_sha256={f:sha(src/f) for f in paths},library_sha256=sha(lib),binary_sha256=sha(exe),link_command=cmd,otool=subprocess.check_output(['otool','-L',str(exe)],text=True)),indent=2))
    print(out,flush=True)
if __name__=='__main__':main()
