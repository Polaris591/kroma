#!/usr/bin/env python3
"""Round17 historical Rust issue #7 itoa: local pre-fix/post-fix controls.
Uses actual upstream base and fixed GitHub blobs, no synthetic bug insertion.
Do not execute agent until controls pass.
"""
from pathlib import Path
import subprocess,os,hashlib
ROOT=Path(__file__).resolve().parent
LAB=Path("D:/Kroma/experiments/kroma-round17-real-rust-itoa-v2")
PUBLIC=r'''use std::io::Write;
#[test]
fn formats_signed_and_unsigned_numbers() {
    let mut s=Vec::new();
    itoa::write(&mut s,-12i32).unwrap();
    itoa::write(&mut s,345u64).unwrap();
    assert_eq!(s,b"-12345");
}
'''
HIDDEN=r'''use std::io::{self,Write};
#[test]
fn returns_correct_bytes_written() {
    for (value,expected) in [(0i64,1usize),(-1,2),(-123456789,10), (i64::MIN,20)] {
        let mut buf=Vec::new();
        let count=itoa::write(&mut buf,value).unwrap();
        assert_eq!(count,expected);
        assert_eq!(count,buf.len());
    }
}
#[test]
fn propagates_partial_sink_failure() {
    struct Failed { count:usize }
    impl Write for Failed {
        fn write(&mut self,data:&[u8])->io::Result<usize> {
            if self.count>0 { return Err(io::Error::other("sink failed")); }
            let n=data.len().min(2);
            self.count+=n;
            Ok(n)
        }
        fn flush(&mut self)->io::Result<()> {Ok(())}
    }
    let mut writer=Failed{count:0};
    assert!(itoa::write(&mut writer,12345i32).is_err());
}
'''
def setup(version):
    proj=LAB/version
    proj.mkdir(parents=True,exist_ok=False)
    p=ROOT/("round17_itoa_"+version)
    for source,dest in [("Cargo.toml","Cargo.toml"),("src__lib.rs","src/lib.rs"),("tests__test.rs","tests/test.rs")]:
        path=proj/dest
        path.parent.mkdir(parents=True,exist_ok=True)
        path.write_bytes((p/source).read_bytes())
    (proj/"tests/public.rs").write_text(PUBLIC,encoding="utf-8",newline="\n")
    return proj
def run(version,hidden):
    p=LAB/version
    if hidden:(p/"tests/holdout.rs").write_text(HIDDEN,encoding="utf-8",newline="\n")
    env=os.environ.copy()
    env["CARGO_TARGET_DIR"]=str(LAB/"targets"/version)
    env["CARGO_INCREMENTAL"]="0"
    r=subprocess.run(["cargo","test","--offline","--quiet"],cwd=p,env=env,
        capture_output=True,encoding="utf-8",errors="replace",timeout=90)
    print(version,"hidden" if hidden else "public","status",r.returncode,"tail",ascii((r.stdout+r.stderr)[-750:]),flush=True)
    return r.returncode==0
def main():
    setup("base")
    assert run("base",False),"invalid baseline public"
    assert not run("base",True),"invalid baseline hidden"
    setup("fixed")
    assert run("fixed",False),"invalid fixed public"
    assert run("fixed",True),"invalid upstream positive control"
    print("RUST_ITOA_HISTORICAL_NEGATIVE_POSITIVE_CONTROLS_PASS",flush=True)
if __name__=="__main__":main()
