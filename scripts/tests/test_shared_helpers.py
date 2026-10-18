"""CLI compatibility checks; optional caller shims are supplied by the test operator."""
import importlib.util
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from urllib.parse import unquote

ROOT=Path(__file__).resolve().parents[2]

class SharedHelpers(unittest.TestCase):
    def test_canonical_cli_entry_points(self):
        for name in ['workflow_evidence.py','create_project_harness.py','skill_regression.py','model_usage_stats.py']:
            result=subprocess.run([sys.executable,str(ROOT/'scripts'/name),'--help'],capture_output=True)
            self.assertEqual(result.returncode,0,result.stderr.decode('utf-8','replace'))
    @unittest.skipUnless(os.environ.get('FABRICA_COMPAT_ROOT'),'caller compatibility root not supplied')
    def test_original_caller_cli_and_harness(self):
        scripts=Path(os.environ['FABRICA_COMPAT_ROOT'])/'scripts'
        for name in ['workflow_evidence.py','create_project_harness.py','skill_regression.py','model_usage_stats.py']:
            result=subprocess.run([sys.executable,str(scripts/name),'--help'],capture_output=True)
            self.assertEqual(result.returncode,0,result.stderr.decode('utf-8','replace'))
        with tempfile.TemporaryDirectory() as tmp:
            output=Path(tmp)/'fixture with spaces'
            subprocess.run([sys.executable,str(scripts/'create_project_harness.py'),'--target',str(output),'--project-name','Shared Fixture'],check=True,capture_output=True)
            text=(output/'AGENTS.md').read_text(encoding='utf-8-sig')
            self.assertIn('Shared Fixture',text)
            self.assertIn('issue-delivery-contract.md',text)
            self.assertNotIn('[FABRICA ROOT]',text)
            self.assertIn(ROOT.as_posix(),unquote(text))
            before=(output/'AGENTS.md').read_bytes()
            subprocess.run([sys.executable,str(scripts/'create_project_harness.py'),'--target',str(output),'--project-name','Other'],check=True,capture_output=True)
            self.assertEqual((output/'AGENTS.md').read_bytes(),before)

if __name__=='__main__':unittest.main()
