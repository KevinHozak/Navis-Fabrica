"""Exercise the production native-pipe reader with large simultaneous output."""
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest

SOURCE = Path(__file__).resolve().parents[1] / 'src-tauri/scripts/commit-reviewed.ps1'

class NativeOutputTests(unittest.TestCase):
    def test_large_output_drains_both_pipes_without_per_line_waits(self):
        source = SOURCE.read_text(encoding='utf-8')
        functions = source[source.index('function Native-Path'):source.index('$RequestPath = Native-Path')]
        functions += source[source.index('function Quote-Argument'):source.index('function Find-PullRequest')]
        with tempfile.TemporaryDirectory(prefix='fabrica-pipes-') as folder:
            folder = Path(folder)
            producer = folder / 'producer.py'
            producer.write_text("import sys\nfor i in range(3000):\n print('output-'+str(i), flush=True)\n print('error-'+str(i), file=sys.stderr, flush=True)\nsys.exit(int(sys.argv[1]))\n", encoding='utf-8')
            quote = lambda value: "'" + str(value).replace("'", "''") + "'"
            for stream in (False, True):
                harness = folder / 'reader.ps1'
                invocation = f"Run-Native -Program {quote(sys.executable)} -Arguments @({quote(producer)}, '0')" + (' -Stream' if stream else '')
                harness.write_text("$ErrorActionPreference='Stop'\n" + functions + '\n' + invocation, encoding='utf-8')
                started = time.monotonic()
                result = subprocess.run(['powershell.exe', '-NoProfile', '-NonInteractive', '-File', str(harness)], capture_output=True, text=True, timeout=20)
                elapsed = time.monotonic() - started
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn('output-2999', result.stdout)
                if stream:
                    self.assertIn('error-2999', result.stdout)
                self.assertLess(elapsed, 12, f'stream={stream}: {elapsed:.2f}s')
                print(f'3000 lines per pipe, stream={stream}: {elapsed:.2f}s')
            harness.write_text("$ErrorActionPreference='Stop'\n" + functions + f"\nRun-Native -Program {quote(sys.executable)} -Arguments @({quote(producer)}, '7')", encoding='utf-8')
            failed = subprocess.run(['powershell.exe', '-NoProfile', '-NonInteractive', '-File', str(harness)], capture_output=True, text=True, timeout=20)
            self.assertNotEqual(failed.returncode, 0)
            self.assertIn('error-2999', failed.stderr)

if __name__ == '__main__':
    unittest.main()
