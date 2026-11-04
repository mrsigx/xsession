"""2.3 regressions: parallel scan equivalence, prefix checks, fallback and inventory paths."""
import json
import os
from pathlib import Path
import unittest
from unittest.mock import patch

import test_xsession as base
import xsession

A, B, C = base.A, base.B, base.C


class ParallelScanTests(unittest.TestCase):
    setUp = base.RetrievalTests.setUp
    tearDown = base.RetrievalTests.tearDown
    save_config = base.RetrievalTests.save_config
    write = base.RetrievalTests.write
    claude_file = base.RetrievalTests.claude_file
    codex_file = base.RetrievalTests.codex_file
    run_cli = base.RetrievalTests.run_cli

    def mixed_corpus(self):
        """Hits of every provenance class plus every per-record diagnostic."""
        self.claude_file(A, 'needle from the operator')
        self.write(self.claude, 'project/' + B + '.jsonl', [
            {'type': 'user', 'sessionId': A, 'uuid': 'old', 'message': {'role': 'user', 'content': 'needle inherited'}},
            {'type': 'assistant', 'sessionId': B, 'uuid': 'tool', 'message': {'role': 'assistant', 'content': [
                {'type': 'tool_use', 'input': {'command': 'grep NEEDLE N:\\x'}}]}}])
        self.write(self.claude, 'project/' + A + '/subagents/agent-1.jsonl', [
            {'type': 'user', 'sessionId': A, 'message': {'role': 'user', 'content': 'needle delegated'}}])
        p = self.codex_file('rollout-' + C + '.jsonl', text='codex needle')
        with p.open('ab') as f:
            f.write(json.dumps({'type': 'response_item', 'payload': {'type': 'function_call',
                    'arguments': json.dumps({'cmd': 'echo needle \u00df'})}}).encode() + b'\n')
            f.write(b'{broken}\n')
            f.write(json.dumps({'type': 'response_item', 'payload': 'unexpected'}).encode() + b'\n')
            f.write(b'{"type":')
        for n in range(4):
            self.codex_file('2026/10/%02d/rollout-%d.jsonl' % (n + 1, n), sid=C[:-1] + str(n), text='needle %d' % n,
                            root=self.archive)

    def comparable(self, completed):
        out = json.loads(completed.stdout)
        for key in ('elapsed_seconds', 'snapshot', 'pagination'):
            out.pop(key, None)
        out['coverage'].pop('scan', None)
        return out

    def test_parallel_output_matches_serial_byte_for_byte(self):
        self.mixed_corpus()
        self.env['XSESSION_PARALLEL_MIN_BYTES'] = '0'
        self.env.pop('XSESSION_SNAPSHOT_DIR')
        serial = self.run_cli('find', 'needle', '--json', '--limit', '1000', '--jobs', '1')
        parallel = self.run_cli('find', 'needle', '--json', '--limit', '1000', '--jobs', '3')
        self.assertEqual(serial.returncode, 4, serial.stderr)
        self.assertEqual(parallel.returncode, serial.returncode, parallel.stderr)
        self.assertEqual(json.loads(parallel.stdout)['coverage']['scan'], {'jobs': 3, 'prefix_check': 'crc32'})
        self.assertEqual(json.loads(serial.stdout)['coverage']['scan']['jobs'], 1)
        self.assertEqual(json.dumps(self.comparable(serial)), json.dumps(self.comparable(parallel)))
        out = self.comparable(parallel)
        self.assertGreaterEqual(out['total_matches'], 9)
        self.assertEqual((out['coverage']['malformed_records'], out['coverage']['unsupported_records'],
                          out['coverage']['unfinished_records']), (1, 1, 1))
        self.assertIn('resumed_history', {r['provenance'] for r in out['results']})
        self.assertIn('tool_input', {r['provenance'] for r in out['results']})

    def test_parallel_snapshot_paging_pins_sha256(self):
        self.mixed_corpus()
        self.env['XSESSION_PARALLEL_MIN_BYTES'] = '0'
        first = self.run_cli('find', 'needle', '--json', '--limit', '2', '--jobs', '2')
        page = json.loads(first.stdout)
        self.assertEqual(page['coverage']['scan'], {'jobs': 2, 'prefix_check': 'sha256'})
        self.assertEqual(page['pagination']['status'], 'ready')
        second = self.run_cli('find', 'needle', '--json', '--limit', '2', '--jobs', '2', '--cursor', page['next_cursor'])
        self.assertEqual(second.returncode, 4, second.stderr)
        later = json.loads(second.stdout)
        self.assertEqual(later['offset'], 2)
        self.assertEqual(later['total_matches'], page['total_matches'])

    @base.python_only('Rust: tests::prefix_check_crc_detects_in_place_change')
    def test_prefix_check_crc_detects_in_place_change(self):
        p = self.claude_file(A, 'stable text')
        size = p.stat().st_size
        crc = xsession.prefix_check(p, size, False)
        self.assertTrue(crc.startswith('crc32:'))
        self.assertEqual(xsession.prefix_check(p, size, True), xsession.prefix_digest(p, size))
        data = bytearray(p.read_bytes())
        data[data.index(b'stable')] = ord('S')
        p.write_bytes(bytes(data))
        self.assertEqual(p.stat().st_size, size)
        self.assertNotEqual(xsession.prefix_check(p, size, False), crc)

    @base.python_only('Rust: tests::only_strong_reads_pin_the_prefix')
    def test_only_strong_reads_pin_the_prefix(self):
        self.claude_file(A, 'needle')
        config = json.loads(self.cfg.read_text(encoding='utf-8'))
        rows = xsession.inventory(config, xsession.initial_coverage(config))
        crc_row, sha_row = dict(rows[0]), dict(rows[0])
        list(xsession.records(crc_row, xsession.initial_coverage(config), 'needle', strong=False))
        list(xsession.records(sha_row, xsession.initial_coverage(config), 'needle', strong=True))
        self.assertNotIn('_prefix_sha256', crc_row)
        self.assertEqual(sha_row['_prefix_sha256'], xsession.prefix_digest(sha_row['path'], sha_row['bytes']))

    @base.python_only('process-pool fallback is Python-only; threads have no fallback path (parallel == serial is tested)')
    def test_unavailable_workers_fall_back_to_identical_serial_scan(self):
        self.mixed_corpus()
        config = json.loads(self.cfg.read_text(encoding='utf-8'))

        def scan(jobs):
            coverage = xsession.initial_coverage(config)
            rows = xsession.inventory(config, coverage)
            hits = xsession.search(rows, 'needle', coverage, {}, 100, jobs=jobs, strong=False)
            return hits, coverage

        serial_hits, serial_coverage = scan(1)
        with patch.dict(os.environ, {'XSESSION_PARALLEL_MIN_BYTES': '0'}), \
                patch.object(xsession, 'ProcessPoolExecutor', side_effect=OSError('process creation blocked')):
            hits, coverage = scan(4)
        self.assertEqual(json.dumps(hits), json.dumps(serial_hits))
        self.assertEqual(coverage['scan']['jobs'], 1)
        self.assertEqual([n['kind'] for n in coverage['notes'] if n['kind'] == 'parallel_scan_unavailable'],
                         ['parallel_scan_unavailable'])
        self.assertEqual(coverage['errors'], serial_coverage['errors'])

    def test_jobs_must_be_in_range(self):
        self.claude_file(A, 'needle')
        for value in ('0', '65'):
            r = self.run_cli('find', 'needle', '--jobs', value, '--json')
            self.assertEqual(r.returncode, 2, r.stderr)

    @base.python_only('CLI: test_binary_equivalents.test_inventory_paths_equal_resolution; Rust: tests::guards_equal_capture')
    def test_inventory_paths_equal_per_file_resolution(self):
        self.mixed_corpus()
        config = json.loads(self.cfg.read_text(encoding='utf-8'))
        rows = xsession.inventory(config, xsession.initial_coverage(config))
        self.assertGreaterEqual(len(rows), 8)
        walked = sorted(str(p.resolve()) for root in (self.claude, self.codex, self.archive) for p in root.rglob('*.jsonl'))
        self.assertEqual(sorted(r['path'] for r in rows), walked)
        for row in rows:
            self.assertEqual(row['_guards'], xsession.capture_guards(Path(row['path']), row['bytes']))


if __name__ == '__main__':
    unittest.main()
