#!/usr/bin/env python3
"""Exercise campaign discovery and failure handling without compiling Rust."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

import nightly_properties as nightly


class CampaignRunnerTests(unittest.TestCase):
    def run_fake(self, mode):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binary = root / "fake-tests"
            names = nightly.CAMPAIGNS["assignment-scale"][1]
            binary.write_text("#!/usr/bin/env python3\n" + f"names = {names!r}\nmode = {mode!r}\n" + '''
import sys, time
if '--list' in sys.argv:
    for name in names if mode != 'missing' else names[:1]:
        print(name + ': test')
    raise SystemExit(0)
if mode == 'timeout':
    time.sleep(30)
for name in names:
    print('test ' + name + ' ... ' + ('ignored' if mode == 'ignored' else 'ok'))
raise SystemExit(1 if mode == 'failure' else 0)
''')
            binary.chmod(0o755)
            result = subprocess.run([sys.executable, str(nightly.ROOT / "scripts/nightly_properties.py"),
                                     "assignment-scale", str(binary), "--cases", "3", "--timeout", "1",
                                     "--output", str(root / "results")], capture_output=True, text=True)
            receipt = json.loads((root / "results/assignment-scale.json").read_text())
            return result.returncode, receipt

    def test_success_records_exact_budget_and_seed(self):
        code, receipt = self.run_fake('success')
        self.assertEqual(code, 0)
        self.assertEqual(receipt['status'], 'passed')
        self.assertEqual(receipt['cases_per_property'], 3)
        self.assertTrue(receipt['seed'])

    def test_missing_test_fails_discovery(self):
        code, receipt = self.run_fake('missing')
        self.assertNotEqual(code, 0)
        self.assertEqual(receipt['status'], 'discovery_failed')

    def test_ignored_tests_do_not_count_as_passed(self):
        code, receipt = self.run_fake('ignored')
        self.assertNotEqual(code, 0)
        self.assertEqual(receipt['status'], 'failed')

    def test_failure_exit_is_preserved(self):
        code, receipt = self.run_fake('failure')
        self.assertNotEqual(code, 0)
        self.assertEqual(receipt['returncode'], 1)

    def test_timeout_fails_and_writes_receipt(self):
        code, receipt = self.run_fake('timeout')
        self.assertNotEqual(code, 0)
        self.assertEqual(receipt['status'], 'timeout')
        self.assertEqual(len(receipt['incomplete']), 2)


if __name__ == '__main__':
    unittest.main()
