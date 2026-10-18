"""Real filesystem backup checks on Windows; no user profiles or live secrets."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

PS = shutil.which("pwsh") or shutil.which("powershell")
ROOT = Path(__file__).resolve().parents[2]

@unittest.skipUnless(os.name == "nt" and PS, "Windows junction checks")
class BackupTests(unittest.TestCase):
    def test_shared_junction_backup_and_untrusted_link_refusal(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            skills, shared, discovery = [root / name for name in ["skills", "shared", "discovery"]]
            for folder in [skills / "local", shared / "reusable", discovery]:
                folder.mkdir(parents=True)
            (skills / "local/SKILL.md").write_text("local fixture", encoding="utf-8")
            (shared / "reusable/SKILL.md").write_text("shared fixture", encoding="utf-8")
            quote = lambda p: "'" + str(p).replace("'", "''") + "'"
            command = f"New-Item -ItemType Junction -Path {quote(skills/'reusable')} -Target {quote(shared/'reusable')} | Out-Null; New-Item -ItemType Junction -Path {quote(discovery/'reusable')} -Target {quote(skills/'reusable')} | Out-Null"
            subprocess.run([PS,"-NoProfile","-Command",command],check=True,capture_output=True)
            args=[PS,"-NoProfile","-File",str(ROOT/"scripts/backup-codex-skills.ps1"),"-SkillRoot",str(skills),"-SharedSkillRoot",str(shared),"-DiscoveryRoot",str(discovery)]
            subprocess.run(args+["-BackupRoot",str(root/"backups")],check=True,capture_output=True)
            manifest=json.loads(next((root/"backups").glob("*/manifest.json")).read_text(encoding="utf-8-sig"))
            self.assertTrue(manifest["complete"])
            self.assertEqual(manifest["skillCount"],2)
            self.assertEqual(len(manifest["sourceLinks"]),1)
            self.assertEqual(len(manifest["discoveryLinks"]),1)
            copied=next((root/"backups").glob("*/shared/agent-skills/reusable/SKILL.md"))
            self.assertEqual(copied.read_text(),"shared fixture")
            outside=root/"untrusted";outside.mkdir()
            subprocess.run([PS,"-NoProfile","-Command",f"New-Item -ItemType Junction -Path {quote(skills/'unknown')} -Target {quote(outside)} | Out-Null"],check=True,capture_output=True)
            failed=subprocess.run(args+["-BackupRoot",str(root/"refused")],capture_output=True)
            self.assertNotEqual(failed.returncode,0)
            self.assertFalse(list((root/"refused").glob("*/manifest.json")))

if __name__ == "__main__":unittest.main()
