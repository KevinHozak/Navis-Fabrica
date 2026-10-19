#!/usr/bin/env python3
"""Build and verify an offline historical regression corpus."""
from __future__ import annotations
import argparse, hashlib, json, os, random, re, shutil, subprocess, sys, time
from pathlib import Path

SOURCE_PATH = "src-tauri/src/project_icon.rs"
CASES = [
    {"key":"svg_before","rev":"7372a11","family":"svg","bug":True,"split":"targets","tests":2,"method":"extract safe_svg() through the original test module; prepend MAX_ICON_BYTES"},
    {"key":"svg_after","rev":"bc2bbbe","family":"svg","bug":False,"split":"regressions","tests":2,"method":"extract safe_svg() through the original test module; prepend MAX_ICON_BYTES"},
    {"key":"nested_before","rev":"56a372f^","family":"nested","bug":True,"split":"targets","tests":2,"method":"retain production module; remove its original cfg(test) module"},
    {"key":"nested_after","rev":"56a372f","family":"nested","bug":False,"split":"regressions","tests":2,"method":"retain production module; remove its original cfg(test) module"},
    {"key":"store_before","rev":"ce37e47^","family":"store","bug":True,"split":"targets","tests":1,"method":"retain production module; remove its original cfg(test) module"},
    {"key":"store_after","rev":"ce37e47","family":"store","bug":False,"split":"regressions","tests":1,"method":"retain production module; remove its original cfg(test) module"},
    {"key":"encoded_before","rev":"7372a11","family":"encoded","bug":True,"split":"held_out","tests":2,"method":"extract safe_svg() and its helpers; prepend MAX_ICON_BYTES"},
    {"key":"encoded_after","rev":"bc2bbbe","family":"encoded","bug":False,"split":"held_out","tests":2,"method":"extract safe_svg() and its helpers; prepend MAX_ICON_BYTES"},
]
CONTRACTS = {
 "svg": "The complete contract is these four calls to safe_svg: return true for <svg><path d='M0 0'/></svg> and <svg><use href='#shape'/></svg>; return false for <svg><use href = \"https://example.invalid/x\"/></svg> and <svg><path fill='url( \"https://example.invalid/x\" )'/></svg>. Assess only these specimen inputs. Full sanitization, other syntax, styling and animation support are outside scope.",
 "nested": "The complete contract is these fixture scenarios, using PNG-signature bytes plus distinct marker payloads as the adapter's existing image_mime accepts. (1) A directory containing only assets/logo.png must select it. (2) A directory containing only frontend/public/icon-512.png must select it. (3) A canonical project directory whose basename is demo, containing only apps/frontend/public/demo-light-mark.png, must select it; project name here means that directory basename lowercased. Missing project roots and oversized assets/logo.png (>512KiB) must return None. Existing support for every other candidate location is expressly permitted; do NOT require an exclusive list or arbitrary recursive discovery. Assess only these fixture outcomes, not unrelated SVG validation or image decoding behavior.",
 "store": "The complete contract is two fixture scenarios, using PNG-signature bytes plus distinct marker payloads as existing image_mime accepts. (1) With only valid small assets/logo.png and assets/play-store-icon.png, select assets/logo.png. (2) With ONLY these three files: oversized assets/icon.png (>512KiB), valid small assets/play-store-icon.png and valid small public/icon-192.png, select the play-store icon's exact bytes. No other files are present in scenario 2. Missing project roots and oversized assets/logo.png alone must return None. All other existing candidate priorities and formats are permitted and OUTSIDE this contract; there is no native-over-all-logos requirement. Assess only these fixture outcomes, not unrelated SVG validation.",
 "encoded": "The complete contract is these four calls to safe_svg: return true for <svg><path d='M0 0'/></svg> and <svg xmlns:ref='http://www.w3.org/1999/xlink'><use ref:href='#shape'/></svg>; return false for <svg><image href=\"&#x68;ttps://example.invalid/x\"/></svg> and <svg xmlns:ref='http://www.w3.org/1999/xlink'><use ref:href = ' https://example.invalid/x'/></svg>. XML attribute entities must be resolved and an alias bound to the XLink namespace must not bypass the resource check. Assess only these four specimen inputs; arbitrary namespaces, full sanitization and other syntax are outside scope.",
}
DEPS='''[dependencies]\nbase64 = "=0.22.1"\nquick-xml = "=0.42.0"\n\n[dev-dependencies]\ntempfile = "3.27.0"\n'''

def sha(b: bytes) -> str: return hashlib.sha256(b).hexdigest()
def run(argv, cwd, timeout):
    return subprocess.run([str(x) for x in argv], cwd=cwd, capture_output=True, text=True, encoding="utf-8", timeout=timeout, shell=False)
def git(repo, *args):
    p=run(["git", "-C", repo, *args], repo, 30)
    if p.returncode: raise RuntimeError("git command failed: " + " ".join(args) + "\n" + p.stderr)
    return p.stdout.strip()
def strip_test_module(s: str) -> str:
    pat=re.compile(r"(?m)^#\[cfg\(test\)\]\s*\nmod\s+tests\s*\{")
    m=list(pat.finditer(s))
    if len(m)!=1: raise ValueError(f"expected exactly one original cfg(test) module, found {len(m)}")
    start=m[0].start(); brace=s.find("{",m[0].start()); depth=0; in_str=False; esc=False; line_comment=False; block=0; raw_hash=None; i=brace
    while i<len(s):
        c=s[i]; n=s[i+1] if i+1<len(s) else ""
        if line_comment:
            if c=="\n": line_comment=False
        elif block:
            if c=="/" and n=="*": block+=1; i+=1
            elif c=="*" and n=="/": block-=1; i+=1
        elif raw_hash is not None:
            end='"'+('#'*raw_hash)
            if s.startswith(end,i): i+=len(end)-1; raw_hash=None
        elif in_str:
            if esc: esc=False
            elif c=="\\": esc=True
            elif c=='"': in_str=False
        elif c=="/" and n=="/": line_comment=True; i+=1
        elif c=="/" and n=="*": block=1; i+=1
        elif c=="r":
            q=i+1
            while q<len(s) and s[q]=="#": q+=1
            if q<len(s) and s[q]=='"': raw_hash=q-i-1; i=q
        elif c=='"': in_str=True
        elif c=="{": depth+=1
        elif c=="}":
            depth-=1
            if depth==0:
                end=i+1
                return s[:start]+s[end:]
        i+=1
    raise ValueError("unterminated original test module")
def extract_svg(s: str) -> str:
    # Match the function boundary lexically by locating the first top-level next item.
    start=s.find("fn safe_svg(")
    if start<0: raise ValueError("safe_svg not found")
    # Keep helpers through the original test-module boundary.
    test=re.search(r"(?m)^#\[cfg\(test\)\]",s)
    if not test or test.start()<=start: raise ValueError("test module boundary missing")
    return "const MAX_ICON_BYTES: u64 = 512 * 1024;\n"+s[start:test.start()].strip()+"\n"
def write(path: Path, data: bytes): path.parent.mkdir(parents=True,exist_ok=True); path.write_bytes(data)
def load_frozen(state: Path):
    p=state/"manifest.json"
    if not p.is_file(): raise RuntimeError("manifest.json does not exist")
    raw=p.read_bytes(); m=json.loads(raw)
    return m,sha(raw)
def build(repoarg: Path, state: Path):
    if (state/"manifest.json").exists(): raise RuntimeError("refusing to overwrite preexisting manifest")
    root=Path(git(repoarg,"rev-parse","--show-toplevel")).resolve()
    state=state.resolve()
    if state==root or root in state.parents: raise RuntimeError("state must be outside repository")
    # Resolve ignore status for state path and reject a state directory inside the repository even if ignored.
    tracked=git(root,"ls-files","--error-unmatch",SOURCE_PATH)
    if not tracked: raise RuntimeError("production source is not tracked")
    shuffled=CASES[:]; random.Random(20261018).shuffle(shuffled)
    records=[]
    for idx,c in enumerate(shuffled):
        rev=git(root,"rev-parse",c["rev"])
        blob=run(["git","-C",root,"show",f"{rev}:{SOURCE_PATH}"],root,30)
        if blob.returncode: raise RuntimeError("cannot read historical source: "+rev)
        text=blob.stdout
        if c["family"] in ("svg", "encoded"): artifact=extract_svg(text)
        else: artifact=strip_test_module(text)
        aid=f"case_{idx+1:02d}"
        src=(state/"sources"/(aid+".rs")); write(src,artifact.encode("utf-8"))
        records.append({"id":aid,"family":c["family"],"bug":c["bug"],"split":c["split"],"expected_contract_failures":c["tests"] if c["bug"] else 0,"expected_contract_passes":0 if c["bug"] else c["tests"],"expected_invariant_count":2 if c["family"] in ("svg", "encoded") else 1,"source_sha256":sha(artifact.encode()),"source_path":str(src.relative_to(state)),"original_path":SOURCE_PATH,"original_revision":rev,"extraction_method":c["method"],"contract_criteria":CONTRACTS[c["family"]]})
    manifest={"format":1,"seed":20261018,"repo_root":str(root),"cases":records,"contract_text":CONTRACTS}
    harness(state,manifest)
    lock = run(["cargo", "generate-lockfile", "--offline"], state/"harness", 30)
    if lock.returncode:
        raise RuntimeError("Offline dependency lock failed: " + lock.stderr)
    manifest["harness_sha256"] = {str(p.relative_to(state)): sha(p.read_bytes())
        for p in sorted((state/"harness").rglob("*")) if p.is_file()}
    manifest["test_fixture_sha256"] = {p.name: sha(p.read_bytes())
        for p in sorted(Path(__file__).parent.glob("*_tests.rs"))}
    data=(json.dumps(manifest,ensure_ascii=False,indent=2)+"\n").encode("utf-8")
    write(state/"manifest.json",data)
    return manifest,sha(data)
def harness(state,manifest):
    h=state/"harness"; (h/"src").mkdir(parents=True,exist_ok=True)
    write(h/"Cargo.toml",("[package]\nname = \"reviewer-corpus\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n"+DEPS).encode())
    mods=[]
    testsdir=Path(__file__).resolve().parent
    for c in manifest["cases"]:
        aid=c["id"]; family=c["family"]
        test=(testsdir/(family+"_tests.rs")).read_text(encoding="utf-8")
        source=(state/c["source_path"]).read_text(encoding="utf-8")
        combined=source+"\n"+test+"\n"
        write(h/"src"/(aid+".rs"),combined.encode("utf-8"))
        mods.append(f"#[allow(dead_code)] mod {aid};")
    write(h/"src/lib.rs",("\n".join(mods)+"\n").encode())
def verify(state: Path, only=False):
    manifest,mhash=load_frozen(state); h=state/"harness"; receipts=[]; allok=True
    for name, expected in manifest["harness_sha256"].items():
        if sha((state/name).read_bytes()) != expected:
            raise RuntimeError("Frozen harness changed: " + name)
    for c in manifest["cases"]:
        if sha((state/c["source_path"]).read_bytes()) != c["source_sha256"]:
            raise RuntimeError("Frozen source changed: " + c["id"])
    rust=run(["rustc","--version"],h,15)
    compile_cmd=["cargo","test","--offline","--locked","--no-run","--message-format=json"]
    started=time.monotonic()
    try: cp=run(compile_cmd,h,180); duration=time.monotonic()-started
    except subprocess.TimeoutExpired as e: cp=None; duration=180; compile_err="timeout"
    logs=state/"logs"; logs.mkdir(parents=True,exist_ok=True)
    if cp:
        write(logs/"compile.stdout",cp.stdout.encode()); write(logs/"compile.stderr",cp.stderr.encode())
        artifacts=[]
        for line in cp.stdout.splitlines():
            try: x=json.loads(line)
            except Exception: continue
            if x.get("reason")=="compiler-artifact" and x.get("profile",{}).get("test"): artifacts.append(x.get("executable"))
        exe=Path(artifacts[-1]).resolve() if artifacts else None
        compile_ok=cp.returncode==0 and exe is not None and h.resolve() in exe.parents and exe.is_file()
    else:
        write(logs/"compile.stdout",b""); write(logs/"compile.stderr",str(compile_err).encode()); compile_ok=False; exe=None
    allok &= compile_ok
    for c in manifest["cases"]:
        src=state/c["source_path"]; before=sha(src.read_bytes()); caseok=compile_ok
        for kind in ("invariant","contract"):
            cmd=[str(exe),f"{c['id']}::corpus_tests::{kind}"] if exe else []
            t0=time.monotonic()
            try: p=run(cmd,h,15) if exe else None; elapsed=time.monotonic()-t0
            except subprocess.TimeoutExpired as e: p=None; elapsed=15
            out=p.stdout if p else ""; err=p.stderr if p else "timeout or compile failure"
            passed=failed=None
            m=re.search(r"test result: \w+\.\s+(\d+) passed;\s+(\d+) failed",out)
            if m: passed,failed=map(int,m.groups())
            rec={"case":c["id"],"filter":kind,"command":cmd,"returncode":p.returncode if p else None,"passed":passed,"failed":failed,"stdout_sha256":sha(out.encode()),"stderr_sha256":sha(err.encode()),"stdout_path":str((logs/(c["id"]+"_"+kind+".stdout")).relative_to(state)),"stderr_path":str((logs/(c["id"]+"_"+kind+".stderr")).relative_to(state)),"duration_seconds":elapsed}
            write(state/rec["stdout_path"],out.encode()); write(state/rec["stderr_path"],err.encode())
            if kind=="invariant": ok=bool(p and p.returncode==0 and passed==c["expected_invariant_count"] and failed==0)
            elif c["bug"]: ok=bool(p and p.returncode==101 and failed==c["expected_contract_failures"] and passed==0)
            else: ok=bool(p and p.returncode==0 and passed==c["expected_contract_passes"] and failed==0)
            rec["expected_outcome_met"]=ok; caseok &= ok; receipts.append(rec)
        after=sha(src.read_bytes()); unchanged=before==after==c["source_sha256"]; caseok &= unchanged; allok &= caseok
    verification={"verdict":"verified" if allok else "UNVERIFIED","manifest_sha256":mhash,"rustc_version":rust.stdout.strip() if rust.returncode==0 else None,"compile":{"command":compile_cmd,"returncode":cp.returncode if cp else None,"stdout_sha256":sha((cp.stdout if cp else "").encode()),"stderr_sha256":sha((cp.stderr if cp else str(compile_err)).encode()),"stdout_path":"logs/compile.stdout","stderr_path":"logs/compile.stderr","duration_seconds":duration,"executable":str(exe) if exe else None,"ok":compile_ok},"runs":receipts}
    for name, expected in manifest["harness_sha256"].items():
        if sha((state/name).read_bytes()) != expected:
            raise RuntimeError("Frozen harness changed during verification: " + name)
    if sha((state/"manifest.json").read_bytes()) != mhash:
        raise RuntimeError("Frozen manifest changed during verification")
    write(state/"verification.json",(json.dumps(verification,indent=2)+"\n").encode())
    print(json.dumps({"verdict":verification["verdict"],"cases":len(manifest["cases"]),"runs":len(receipts),"manifest_sha256":mhash},separators=(",",":")))
    return 0 if allok else 2
def main():
    ap=argparse.ArgumentParser(); ap.add_argument("--repo",type=Path,required=True); ap.add_argument("--state",type=Path,required=True); ap.add_argument("--verify-only",action="store_true"); a=ap.parse_args()
    try:
        root = Path(git(a.repo, "rev-parse", "--show-toplevel")).resolve()
        state = a.state.resolve()
        if state == root or root in state.parents:
            raise RuntimeError("state must be outside repository")
        if a.verify_only: return verify(a.state.resolve(),True)
        m,d=build(a.repo,a.state); print(json.dumps({"manifest_sha256":d,"cases":len(m["cases"]),"status":"frozen; verifying"},separators=(",",":")))
        return verify(a.state.resolve())
    except Exception as e:
        print(json.dumps({"verdict":"UNVERIFIED","error":str(e)},separators=(",",":")),file=sys.stderr); return 2
if __name__=="__main__": raise SystemExit(main())
