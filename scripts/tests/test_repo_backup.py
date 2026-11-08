import contextlib, importlib.util, io, json, os, sqlite3, subprocess, tempfile, threading, time, unittest, uuid, zipfile
from unittest.mock import patch
from pathlib import Path
SCRIPT=Path(__file__).parents[1]/"repo_backup.py"
spec=importlib.util.spec_from_file_location("repo_backup",SCRIPT); rb=importlib.util.module_from_spec(spec); spec.loader.exec_module(rb)

class RepoBackupTests(unittest.TestCase):
 def test_backup_restores_dirty_untracked_and_git_history(self):
  with tempfile.TemporaryDirectory() as td:
   base=Path(td); src=base/"repo"; dest=base/"backups"; src.mkdir(); dest.mkdir()
   subprocess.run(["git","init","-q",str(src)],check=True)
   (src/"tracked.txt").write_text("first\n",encoding="utf8")
   subprocess.run(["git","-C",str(src),"-c","user.name=Test","-c","user.email=test@example.invalid","add","tracked.txt"],check=True)
   subprocess.run(["git","-C",str(src),"-c","user.name=Test","-c","user.email=test@example.invalid","commit","-qm","seed"],check=True)
   (src/"tracked.txt").write_text("dirty\n",encoding="utf8"); (src/"untracked.txt").write_text("new\n",encoding="utf8")
   (src/"node_modules").mkdir(); (src/"node_modules"/"omit").write_text("cache")
   rb.archive_one("sample",src,dest,set(rb.DEFAULT_EXCLUDES),"backup")
   zpath=next(dest.glob("*.zip"))
   with zipfile.ZipFile(zpath) as z:
    self.assertEqual(z.testzip(),None); m=json.loads(z.read("sample/BACKUP-MANIFEST.json"))
    self.assertIn("sample/tracked.txt",z.namelist()); self.assertIn("sample/untracked.txt",z.namelist()); self.assertTrue(any(x["path"]==".git/config" for x in m["files"]))
    self.assertNotIn("sample/node_modules/omit",z.namelist()); self.assertTrue(any(x["path"]=="node_modules" for x in m["skipped"]))
    restored=base/"restored"; z.extractall(restored)
   self.assertEqual((restored/"sample"/"tracked.txt").read_text(),"dirty\n")
   self.assertEqual(subprocess.check_output(["git","-C",str(restored/"sample"),"log","-1","--format=%s"],text=True).strip(),"seed")
   with self.assertRaisesRegex(RuntimeError,"refusing to overwrite"):
    rb.archive_one("sample",src,dest,set(rb.DEFAULT_EXCLUDES),"backup")
 def test_discovery_is_immediate_and_github_check_is_local(self):
  with tempfile.TemporaryDirectory() as td:
   root=Path(td); (root/"top").mkdir(); (root/"nested").mkdir(); (root/"top"/".git").mkdir(); (root/"nested"/".git").mkdir()
   self.assertEqual([n for n,_ in rb.discover(root,set())],["nested","top"])
   subprocess.run(["git","-C",str(root/"top"),"init","-q"],check=True)
   subprocess.run(["git","-C",str(root/"top"),"remote","add","origin","https://github.com/example/private.git"],check=True)
   self.assertTrue(rb.github_remote_present(root/"top"))
   subprocess.run(["git","-C",str(root/"nested"),"init","-q"],check=True)
   self.assertFalse(rb.github_remote_present(root/"nested"))
 def test_configured_repository_exclusion_precedes_repo_selection(self):
  with tempfile.TemporaryDirectory() as td:
   base=Path(td); repo=base/"GP-Krita"; repo.mkdir(); dest=base/"backups"; dest.mkdir()
   cfg=base/"config.json"; cfg.write_text(json.dumps({"schema_version":1,"destination":str(dest),"repositories":[{"name":"GP-Krita","path":str(repo)}],"exclude_repositories":["gp-krita"]}))
   output=io.StringIO()
   with patch.object(rb,"github_remote_present",side_effect=AssertionError("excluded repo must not be inspected")),contextlib.redirect_stdout(output):
    self.assertEqual(rb.main(["--config",str(cfg),"--repo","GP-Krita","plan"]),0)
   self.assertIn("SKIP GP-Krita: excluded by configuration",output.getvalue())
 def test_nested_github_repo_is_skipped_and_local_repo_is_kept(self):
  with tempfile.TemporaryDirectory() as td:
   root=Path(td)/"parent"; root.mkdir()
   local=root/"local"; github=root/"github"
   for child in (local,github):
    child.mkdir(); subprocess.run(["git","init","-q",str(child)],check=True); (child/"file.txt").write_text(child.name)
   subprocess.run(["git","-C",str(github),"remote","add","origin","git@github.com:example/remote.git"],check=True)
   files,skipped=rb.inventory(root,set())
   paths={x[0] for x in files}
   self.assertIn("local/file.txt",paths)
   self.assertIn("local/.git/config",paths)
   self.assertNotIn("github/file.txt",paths)
   self.assertTrue(any(x["path"]=="github" and "GitHub-backed" in x["reason"] for x in skipped))
 def test_rejects_actual_linked_worktree(self):
  with tempfile.TemporaryDirectory() as td:
   base=Path(td); repo=base/"repo"; repo.mkdir(); dest=base/"backups"; dest.mkdir()
   subprocess.run(["git","init","-q",str(repo)],check=True)
   (repo/"seed").write_text("x")
   subprocess.run(["git","-C",str(repo),"-c","user.name=Test","-c","user.email=test@example.invalid","add","seed"],check=True)
   subprocess.run(["git","-C",str(repo),"-c","user.name=Test","-c","user.email=test@example.invalid","commit","-qm","seed"],check=True)
   linked=base/"linked"; subprocess.run(["git","-C",str(repo),"worktree","add","--detach",str(linked),"HEAD"],check=True,capture_output=True)
   cfg=base/"c.json"; cfg.write_text(json.dumps({"schema_version":1,"destination":str(dest),"repositories":[{"name":"linked","path":str(linked)}]}))
   with self.assertRaisesRegex(ValueError,"linked Git worktree/shared Git directory"):
    rb.config_data(cfg)
 def test_links_are_manifested_and_never_traversed_when_supported(self):
  with tempfile.TemporaryDirectory() as td:
   root=Path(td)/"repo"; root.mkdir(); target=Path(td)/"outside"; target.mkdir(); (target/"secret.txt").write_text("outside")
   junction=root/"link"
   try: os.symlink(target,junction,target_is_directory=True)
   except (OSError,NotImplementedError): self.skipTest("directory link creation is unavailable")
   files,skipped=rb.inventory(root,set())
   self.assertFalse(files); self.assertTrue(any(x["path"]=="link" for x in skipped))
 def test_filesystem_inspection_error_fails_closed(self):
  with tempfile.TemporaryDirectory() as td:
   root=Path(td)/"repo"; root.mkdir(); (root/"x").write_text("x")
   with patch.object(rb,"is_link",side_effect=RuntimeError("cannot inspect filesystem entry")):
    with self.assertRaisesRegex(RuntimeError,"cannot inspect filesystem entry"): rb.inventory(root,set())
 def test_live_json_and_sqlite_snapshots_archive_consistently(self):
  with tempfile.TemporaryDirectory() as td:
   base=Path(td); src=base/"repo"; dest=base/"out"; src.mkdir(); dest.mkdir()
   json_file=src/"private"/"usage"/"latest.json"; json_file.parent.mkdir(parents=True); json_file.write_text('{"sequence":0}')
   db_file=json_file.parent/"usage.sqlite3"
   db=sqlite3.connect(db_file)
   try:
    db.execute("PRAGMA journal_mode=WAL"); db.execute("CREATE TABLE events (n INTEGER PRIMARY KEY)"); db.execute("INSERT INTO events VALUES (0)")
   finally: db.close()
   stable_tmp=json_file.parent/f"latest.{uuid.uuid4()}.tmp"; stable_tmp.write_text("temporary replacement")
   stop=threading.Event(); ready=threading.Event()
   def writers():
    conn=sqlite3.connect(db_file,timeout=5)
    conn.execute("INSERT INTO events VALUES (999)"); conn.commit(); ready.set()
    n=1
    try:
     while not stop.is_set() and n<120:
      conn.execute("INSERT INTO events VALUES (?)",(n,)); conn.commit()
      tmp=json_file.parent/f"latest.{uuid.uuid4()}.tmp"; tmp.write_text(json.dumps({"sequence":n}))
      try: os.replace(tmp,json_file)
      except PermissionError:
       try: tmp.unlink()
       except OSError: pass
      n+=1; time.sleep(.002)
    finally: conn.close()
   thread=threading.Thread(target=writers); thread.start(); self.assertTrue(ready.wait(5))
   try: rb.archive_one("sample",src,dest,set(),"backup",["private/usage/latest.json","private/usage/usage.sqlite3"])
   finally: stop.set(); thread.join(timeout=10)
   self.assertFalse(thread.is_alive())
   zpath=next(dest.glob("*.zip"))
   with zipfile.ZipFile(zpath) as z:
    m=json.loads(z.read("sample/BACKUP-MANIFEST.json")); snapshots={x["path"]:x for x in m["snapshots"]}
    self.assertEqual(snapshots["private/usage/latest.json"]["method"],"stable-json-copy")
    self.assertEqual(snapshots["private/usage/usage.sqlite3"]["method"],"sqlite-online-backup")
    parsed=json.loads(z.read("sample/private/usage/latest.json")); self.assertIsInstance(parsed["sequence"],int)
    staged=base/"snapshot.sqlite3"; staged.write_bytes(z.read("sample/private/usage/usage.sqlite3"))
    db=sqlite3.connect(staged)
    try:
     self.assertEqual(db.execute("PRAGMA quick_check").fetchone()[0],"ok")
     self.assertGreaterEqual(db.execute("SELECT COUNT(*) FROM events").fetchone()[0],1)
    finally: db.close()
    self.assertTrue(any("transient atomic JSON" in x["reason"] for x in m["skipped"]))
    self.assertTrue(any("SQLite online snapshot" in x["reason"] for x in m["skipped"]))
   self.assertTrue(json.loads(json_file.read_text())["sequence"]>=0)
   stable_tmp.unlink(missing_ok=True)
   db=sqlite3.connect(db_file)
   try: self.assertEqual(db.execute("PRAGMA quick_check").fetchone()[0],"ok")
   finally: db.close()
 def test_source_mutation_does_not_publish_success_archive(self):
  with tempfile.TemporaryDirectory() as td:
   root=Path(td); src=root/"repo"; dest=root/"out"; src.mkdir(); dest.mkdir(); f=src/"f"; f.write_text("before")
   original=rb.digest
   def mutate(stream):
    result=original(stream); f.write_text("changed-content-longer"); return result
   rb.digest=mutate
   try:
    with self.assertRaisesRegex(RuntimeError,"source changed while reading|inventory changed"): rb.archive_one("r",src,dest,set(),"backup")
   finally: rb.digest=original
   self.assertFalse(list(dest.glob("*.zip"))); self.assertTrue(list(dest.glob("*.partial")))
if __name__=="__main__": unittest.main()
