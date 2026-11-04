"""CLI-level equivalents of the 2.3.0 tests that call the Python API in-process. They run against
Python 2.3.0 and against the binary named by XSESSION_BINARY alike."""
import json
from pathlib import Path
import unittest

import test_xsession as base

A, B, C = base.A, base.B, base.C


class BinaryEquivalentTests(unittest.TestCase):
    setUp = base.RetrievalTests.setUp
    tearDown = base.RetrievalTests.tearDown
    save_config = base.RetrievalTests.save_config
    write = base.RetrievalTests.write
    claude_file = base.RetrievalTests.claude_file
    codex_file = base.RetrievalTests.codex_file
    run_cli = base.RetrievalTests.run_cli

    def test_operational_error_is_json_exit_six(self):
        # Replaces test_operational_exception_has_json_and_exit_six (patched inventory).
        r = self.run_cli('find', 'needle', '--sources', str(self.root / 'absent.json'), '--json')
        self.assertEqual(r.returncode, 6, r.stdout + r.stderr)
        out = json.loads(r.stdout)
        self.assertEqual(out['status'], 'operational_error')
        self.assertEqual(out['error']['type'], 'FileNotFoundError')
        self.assertEqual(out['results'], [])

    def test_default_page_writes_no_snapshot(self):
        # Replaces test_default_page_does_not_attempt_persistent_storage (patched save_snapshot).
        self.claude_file(A, 'needle one')
        self.claude_file(B, 'needle two')
        self.env.pop('XSESSION_SNAPSHOT_DIR')
        before = sorted(str(p) for p in self.root.rglob('*'))
        r = self.run_cli('find', 'needle', '--limit', '1', '--json')
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        data = json.loads(r.stdout)
        self.assertEqual(len(data['results']), 1)
        self.assertTrue(data['truncated'])
        self.assertIsNone(data['next_cursor'])
        self.assertEqual(data['pagination']['status'], 'disabled')
        self.assertEqual(sorted(str(p) for p in self.root.rglob('*')), before)

    def test_inventory_paths_equal_resolution(self):
        # Replaces the path half of test_inventory_paths_equal_per_file_resolution.
        self.claude_file(A, 'needle from the operator')
        self.write(self.claude, 'project/' + A + '/subagents/agent-1.jsonl', [
            {'type': 'user', 'sessionId': A, 'message': {'role': 'user', 'content': 'needle delegated'}}])
        self.codex_file('rollout-' + C + '.jsonl', text='codex needle')
        for n in range(4):
            self.codex_file('2026/10/%02d/rollout-%d.jsonl' % (n + 1, n), sid=C[:-1] + str(n), text='needle %d' % n,
                            root=self.archive)
        r = self.run_cli('list', '1000', '--json')
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        listed = sorted(row['path'] for row in json.loads(r.stdout)['results'])
        walked = sorted(str(p.resolve()) for root in (self.claude, self.codex, self.archive) for p in root.rglob('*.jsonl'))
        self.assertEqual(listed, walked)


if __name__ == '__main__':
    unittest.main()
