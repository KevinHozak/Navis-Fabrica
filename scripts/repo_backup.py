from __future__ import annotations
import argparse, hashlib, json, os, re, subprocess, sys, zipfile
from datetime import datetime, timezone
from pathlib import Path

DEFAULT_EXCLUDES={"node_modules",".venv","venv","__pycache__","target","dist","build",".pytest_cache",".mypy_cache",".ruff_cache",".worktrees"}

def inside(child,parent):
 try: child.relative_to(parent); return True
 except ValueError: return False

def is_link(p):
 try: return p.is_symlink() or (hasattr(p,"is_junction") and p.is_junction()) or bool(getattr(p.stat(follow_symlinks=False),"st_file_attributes",0)&getattr(__import__("stat"),"FILE_ATTRIBUTE_REPARSE_POINT",0x400))
 except OSError as e: raise RuntimeError(f"cannot inspect filesystem entry {p}: {e}") from e

def github_remote_present(repo):
 env=os.environ.copy(); env["GIT_OPTIONAL_LOCKS"]="0"
 try:
  r=subprocess.run(["git","-c",f"safe.directory={repo}","-C",str(repo),"remote","-v"],capture_output=True,text=True,timeout=10,env=env)
 except (OSError,subprocess.TimeoutExpired) as e: raise RuntimeError(f"cannot inspect Git remotes for {repo.name}: {e}")
 if r.returncode: raise RuntimeError(f"cannot inspect Git remotes for {repo.name}")
 # Inspect values in memory; never print credentials or remote URLs.
 return any(re.search(r"(?:^|[@/:])github\.com(?:[:/]|$)",line.split("\t")[-1].lower()) for line in r.stdout.splitlines() if "\t" in line)

def discover(root,excludes):
 root=root.resolve(strict=True)
 if not root.is_dir(): raise ValueError("source_root must be a directory")
 found=[]
 try: entries=sorted(root.iterdir(),key=lambda p:p.name.casefold())
 except OSError as e: raise RuntimeError(f"cannot inspect source_root: {e}")
 for p in entries:
  if is_link(p) or not p.is_dir() or p.name.casefold() in excludes: continue
  if (p/".git").exists(): found.append((p.name,p))
 return found

def config_data(path):
 d=json.loads(Path(path).read_text(encoding="utf-8-sig"))
 if d.get("schema_version")!=1: raise ValueError("config requires schema_version 1")
 dest=Path(d.get("destination","")).expanduser()
 if not dest.is_absolute() or not dest.is_dir(): raise ValueError("destination must be an existing absolute directory")
 ex={str(x).casefold() for x in d.get("exclude_dirs",[])}|DEFAULT_EXCLUDES
 if d.get("source_root"):
  if d.get("selection") != "no-github-remote": raise ValueError("selection must be no-github-remote")
  root=Path(d["source_root"]).expanduser().resolve(strict=True)
  rows=discover(root,ex)
 else:
  rows=[]
  for r in d.get("repositories",[]):
   if not r.get("name") or not r.get("path"): raise ValueError("each repository requires name and path")
   rows.append((r["name"],Path(r["path"]).resolve(strict=True)))
 if not rows: raise ValueError("no Git repositories discovered/configured")
 seen=set()
 for n,p in rows:
  if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._ -]{0,79}",n) or n in (".",".."): raise ValueError(f"unsafe repository name: {n}")
  if n.casefold()=="backup-manifest.json": raise ValueError("repository name conflicts with reserved manifest")
  if n.casefold() in seen: raise ValueError(f"duplicate repository folder name: {n}")
  seen.add(n.casefold())
  if not p.is_dir(): raise ValueError(f"repository path is not a directory: {n}")
  if inside(dest.resolve(),p): raise ValueError(f"destination is inside source: {n}")
  gitdir=p/".git"
  if gitdir.is_file() or (gitdir/"commondir").exists(): raise ValueError(f"linked Git worktree/shared Git directory not supported: {n}")
  alternates=gitdir/"objects"/"info"/"alternates"
  if alternates.exists() and alternates.read_text(encoding="utf-8",errors="replace").strip(): raise ValueError(f"external Git object alternates are not self-contained: {n}")
 return dest.resolve(),rows,ex

def inventory(root,excludes):
 files=[]; skipped=[]
 def walk(d):
  try: entries=sorted(d.iterdir(),key=lambda p:p.name.casefold())
  except OSError as e: raise RuntimeError(f"cannot read {d}: {e}")
  for p in entries:
   rel=p.relative_to(root).as_posix()
   if is_link(p): skipped.append({"path":rel,"reason":"symlink/junction/reparse point not traversed"}); continue
   if p.is_dir():
    if p.name.casefold() in excludes: skipped.append({"path":rel,"reason":"configured generated/cache directory exclusion"}); continue
    if p != root and (p/".git").exists():
     if (p/".git").is_file() or (p/".git"/"commondir").exists():
      skipped.append({"path":rel,"reason":"nested linked/shared Git metadata is external"}); continue
     alternates=p/".git"/"objects"/"info"/"alternates"
     if alternates.exists() and alternates.read_text(encoding="utf-8",errors="replace").strip():
      skipped.append({"path":rel,"reason":"nested repository uses external Git object alternates"}); continue
     if github_remote_present(p):
      skipped.append({"path":rel,"reason":"nested GitHub-backed repository excluded"}); continue
    walk(p)
   elif p.is_file():
    if rel.casefold()=="backup-manifest.json": raise RuntimeError("source contains reserved root file BACKUP-MANIFEST.json")
    st=p.stat(); files.append((rel,p,st.st_size,st.st_mtime_ns))
   else: skipped.append({"path":rel,"reason":"unsupported filesystem entry"})
 walk(root); return files,skipped

def digest(f):
 h=hashlib.sha256()
 for b in iter(lambda:f.read(1024*1024),b""): h.update(b)
 return h.hexdigest()

def archive_one(name,root,dest,excludes,mode):
 files,skipped=inventory(root,excludes); total=sum(x[2] for x in files)
 stamp=datetime.now().astimezone().strftime("%Y-%m-%d"); final=dest/f"{name}_{stamp}.zip"; partial=dest/(final.name+".partial")
 print(f"{mode.upper()} {name}: {len(files)} files, {total:,} bytes -> {final.name}",flush=True)
 if mode=="plan":
  if skipped: print(f"  {len(skipped)} link/cache/unsupported entries will be listed in the manifest",flush=True)
  return
 if final.exists(): raise RuntimeError(f"refusing to overwrite existing archive: {final.name}")
 if partial.exists(): raise RuntimeError(f"partial archive already exists: {partial.name}")
 manifest={"schema_version":1,"repository":name,"source":str(root),"created_utc":datetime.now(timezone.utc).isoformat(),"files":[],"skipped":skipped,"exclusions":sorted(excludes)}
 try:
  with zipfile.ZipFile(partial,"x",compression=zipfile.ZIP_DEFLATED,compresslevel=6,allowZip64=True) as z:
   for rel,p,size,mtime in files:
    if (p.stat().st_size,p.stat().st_mtime_ns)!=(size,mtime): raise RuntimeError(f"source changed before read: {rel}")
    h=hashlib.sha256()
    with p.open("rb") as src,z.open(f"{name}/{rel}","w",force_zip64=True) as out:
     for block in iter(lambda:src.read(1024*1024),b""): h.update(block); out.write(block)
    if (p.stat().st_size,p.stat().st_mtime_ns)!=(size,mtime): raise RuntimeError(f"source changed while reading: {rel}")
    manifest["files"].append({"path":rel,"size":size,"sha256":h.hexdigest()})
   z.writestr(f"{name}/BACKUP-MANIFEST.json",json.dumps(manifest,ensure_ascii=False,indent=2).encode("utf-8"))
  with zipfile.ZipFile(partial) as z:
   if z.testzip(): raise RuntimeError("ZIP CRC check failed")
   for rec in manifest["files"]:
    with z.open(f"{name}/{rec['path']}") as f:
     if digest(f)!=rec["sha256"]: raise RuntimeError(f"archive hash mismatch: {rec['path']}")
  now,_=inventory(root,excludes)
  if [(a,s,m) for a,_,s,m in files]!=[(a,s,m) for a,_,s,m in now]: raise RuntimeError("source file inventory changed during backup")
  # Windows rename is atomic and refuses an existing destination on the target platform.
  # A hard-link fallback is used only for non-Windows development/test hosts.
  if os.name == "nt": os.rename(partial,final)
  else: os.link(partial,final); partial.unlink()
  print(f"  VERIFIED {final.name} ({final.stat().st_size:,} bytes)")
 except Exception: raise

def main(argv=None):
 ap=argparse.ArgumentParser(description="Plan or create verified ZIP snapshots of local Git repositories.")
 ap.add_argument("--config",required=True,help="external JSON configuration")
 ap.add_argument("--repo",help="limit to one repository by folder/name")
 ap.add_argument("mode",choices=("plan","backup"),help="plan is read-only; backup creates archives")
 a=ap.parse_args(argv)
 try:
  dest,rows,ex=config_data(a.config)
  selected=[]; failures=[]
  for n,p in rows:
   if a.repo and n.casefold()!=a.repo.casefold(): continue
   try:
    print(f"CHECK {n}: Git remotes",flush=True)
    if github_remote_present(p): print(f"SKIP {n}: GitHub remote configured")
    else: selected.append((n,p))
   except Exception as e:
    print(f"FAILED {n}: remote inspection: {e}",file=sys.stderr); failures.append(n)
  if a.repo and not any(n.casefold()==a.repo.casefold() for n,_ in rows): raise ValueError(f"repository not found: {a.repo}")
  if not selected: print("No eligible repositories were selected.")
  for n,p in selected:
   try: archive_one(n,p,dest,ex,a.mode)
   except Exception as e: print(f"FAILED {n}: {e}",file=sys.stderr); failures.append(n)
  if failures: print("Failed repositories: " + ", ".join(failures),file=sys.stderr); return 1
  return 0
 except Exception as e: print(f"ERROR: {e}",file=sys.stderr); return 1
if __name__=="__main__": raise SystemExit(main())
