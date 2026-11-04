"""Compressed transcripts (.jsonl.zst), added in 2.4.0 / 3.1.0. A compressed rollout must read
exactly like its plain twin (lines, byte offsets, record digests, list windows, paging), and
every stream this tool does not read must surface as partial coverage with a fixed reason.
The same tests drive the Python core, or the binary named by XSESSION_BINARY.

Fixtures are built with the standard library's compression.zstd (Python 3.14+), which is also
the oracle decoder; hand-built frames cover the header cases an encoder would not emit."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

from compression import zstd

BASE = Path(__file__).resolve().parent
PY = BASE.parent / 'python'  # the Python implementation under test
A = '11111111-1111-4111-8111-111111111111'
C = '22222222-1111-4111-8111-111111111111'
D = '33333333-1111-4111-8111-111111111111'
BINARY = os.environ.get('XSESSION_BINARY')
MAGIC = b'\x28\xb5\x2f\xfd'


def python_only(reason):
    return unittest.skipIf(BINARY, 'in-process Python API; ' + reason)


def line(obj):
    return (json.dumps(obj) + '\n').encode()


def rollout(sid, texts, timestamp='2026-09-21T00:00:00Z'):
    out = [line({'type': 'session_meta', 'payload': {'id': sid, 'timestamp': timestamp, 'source': 'cli'}})]
    for i, text in enumerate(texts):
        out.append(line({'type': 'response_item', 'timestamp': '2026-09-21T00:%02d:00Z' % (i + 1),
                         'payload': {'type': 'message', 'role': 'user', 'content': [{'type': 'input_text', 'text': text}]}}))
    return b''.join(out)


def raw_frame(blocks, content_size):
    """A single-segment frame with a 1-byte content size; blocks are (type, payload, regenerated)."""
    out = bytearray(MAGIC + bytes([0x20, content_size]))
    for index, (kind, payload, regenerated) in enumerate(blocks):
        size = regenerated if kind == 1 else len(payload)
        header = (size << 3) | (kind << 1) | (1 if index == len(blocks) - 1 else 0)
        out += header.to_bytes(3, 'little') + payload
    return bytes(out)


def strip_volatile(obj):
    """Fields that legitimately differ between a plain file and its compressed twin."""
    if isinstance(obj, dict):
        return {k: (strip_volatile(v) if k not in ('path', 'mtime_ns', 'source') else None)
                for k, v in obj.items() if k not in ('version', 'elapsed_seconds', 'snapshot', 'next_cursor')}
    if isinstance(obj, list):
        return [strip_volatile(x) for x in obj]
    return obj


class CompressedTranscriptTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(dir=BASE)
        self.root = Path(self.tmp.name)
        self.plain = self.root / 'plain'
        self.packed = self.root / 'packed'
        self.claude = self.root / 'claude'
        for p in (self.plain, self.packed, self.claude):
            p.mkdir()
        self.cfg = self.root / 'sources.json'
        self.cfg.write_text(json.dumps({'sources': [
            {'name': 'plain', 'kind': 'codex', 'path': str(self.plain)},
            {'name': 'packed', 'kind': 'codex', 'path': str(self.packed)},
            {'name': 'claude', 'kind': 'claude', 'path': str(self.claude)}]}), encoding='utf-8')
        self.env = dict(os.environ, XSESSION_CONFIG=str(self.cfg), PYTHONIOENCODING='utf-8',
                        XSESSION_SNAPSHOT_DIR=str(self.root / 'snapshots'))
        self.env.pop('CODEX_THREAD_ID', None)
        self.env.pop('CLAUDE_CODE_SESSION_ID', None)

    def tearDown(self):
        self.tmp.cleanup()

    def put(self, root, name, data):
        p = root / name
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_bytes(data)
        return p

    def twins(self, name, data, **options):
        """The same transcript as plain .jsonl in one store and .jsonl.zst in the other."""
        self.put(self.plain, name, data)
        return self.put(self.packed, name + '.zst', zstd.compress(data, **options))

    def run_cli(self, *args):
        if BINARY:
            command = [BINARY, *args]
        else:
            command = [sys.executable, '-B', str(PY / 'xsession.py'), *args]
        return subprocess.run(command, env=self.env, capture_output=True, text=True, encoding='utf-8', timeout=60)

    def json_cli(self, *args):
        r = self.run_cli(*args, '--json')
        return r.returncode, json.loads(r.stdout) if r.stdout.strip() else None, r.stderr

    def assert_partial(self, kind, detail, path_name):
        code, out, err = self.json_cli('find', 'needle', '--source', 'packed')
        self.assertEqual(code, 4, err)
        self.assertEqual(out['status'], 'partial_coverage')
        errors = [e for e in out['coverage']['errors'] if Path(e['path']).name == path_name]
        self.assertEqual([(e['kind'], e['detail']) for e in errors], [(kind, detail)], out['coverage']['errors'])

    # --------------------------------------------------------------- equivalence with plain twins

    def test_find_hits_match_the_plain_twin(self):
        data = rollout(C, ['first needle', 'padding ' + 'x' * 5000, 'second needle', 'third NEEDLE'])
        self.twins('rollout-2026-09-21-' + C + '.jsonl', data)
        plain = self.json_cli('find', 'needle', '--source', 'plain')
        packed = self.json_cli('find', 'needle', '--source', 'packed')
        self.assertEqual(plain[0], 0, plain[2])
        self.assertEqual(packed[0], 0, packed[2])
        self.assertEqual(len(packed[1]['results']), 3)
        self.assertEqual(strip_volatile(packed[1]['results']), strip_volatile(plain[1]['results']))
        for hit in packed[1]['results']:
            self.assertTrue(hit['path'].endswith('.jsonl.zst'))
            self.assertEqual(hit['bytes'], len(data))
            start = hit['byte_offset']
            raw = data[start:data.index(b'\n', start) + 1]
            self.assertEqual(hit['record_sha256'], hashlib.sha256(raw).hexdigest())

    def test_list_rows_match_the_plain_twin(self):
        texts = ['opening request %d' % i for i in range(3)] + ['y' * 70000, 'closing needle']
        data = rollout(C, texts)
        self.twins('rollout-' + C + '.jsonl', data)
        plain = self.json_cli('list', '10', '--source', 'plain')
        packed = self.json_cli('list', '10', '--source', 'packed')
        self.assertEqual((plain[0], packed[0]), (0, 0), packed[2])
        self.assertEqual(strip_volatile(packed[1]['results']), strip_volatile(plain[1]['results']))
        self.assertGreater(len(data), 65536)
        self.assertEqual(packed[1]['results'][0]['inspected_bytes'], len(data))

    def test_read_line_checks_the_decompressed_record(self):
        data = rollout(C, ['alpha', 'needle beta', 'gamma'])
        self.twins('rollout-' + C + '.jsonl', data)
        code, out, err = self.json_cli('find', 'needle beta', '--source', 'packed')
        self.assertEqual(code, 0, err)
        hit = out['results'][0]
        code, read, err = self.json_cli('read', C, '--line', str(hit['line']), '--context', '1',
                                        '--record-sha256', hit['record_sha256'], '--source', 'packed')
        self.assertEqual(code, 0, err)
        self.assertEqual([r['line'] for r in read['results']], [2, 3, 4])
        code, read, err = self.json_cli('read', C, '--line', '3', '--record-sha256', '0' * 64, '--source', 'packed')
        self.assertEqual((code, read['status']), (3, 'record_changed'), err)

    def test_paging_reopens_compressed_records(self):
        data = rollout(C, ['needle %d' % i for i in range(5)])
        self.twins('rollout-' + C + '.jsonl', data)
        code, first, err = self.json_cli('find', 'needle', '--limit', '2', '--source', 'packed')
        self.assertEqual(code, 0, err)
        self.assertTrue(first['next_cursor'])
        seen = [r['line'] for r in first['results']]
        cursor = first['next_cursor']
        while cursor:
            code, page, err = self.json_cli('find', 'needle', '--limit', '2', '--source', 'packed', '--cursor', cursor)
            self.assertEqual(code, 0, err)
            seen += [r['line'] for r in page['results']]
            cursor = page['next_cursor']
        self.assertEqual(sorted(seen), [2, 3, 4, 5, 6])

    def test_legacy_path_reports_decompressed_lines_and_size(self):
        data = rollout(C, ['a', 'b']) + b'{"type": "tail without newline"}'
        self.twins('rollout-' + C + '.jsonl', data)
        r = self.run_cli('path', C, '--legacy', '--source', 'packed')
        self.assertEqual(r.returncode, 0, r.stderr)
        fields = dict(l.split(': ', 1) for l in r.stdout.splitlines())
        self.assertEqual(fields['lines'], '4')
        self.assertEqual(fields['size'], str(len(data)))
        self.assertTrue(fields['transcript'].endswith('.jsonl.zst'))

    def test_unfinished_tail_is_excluded_like_plain(self):
        data = rollout(C, ['needle complete']) + b'{"type": "response_item", "payload": "needle cut'
        self.twins('rollout-' + C + '.jsonl', data)
        plain = self.json_cli('find', 'needle', '--source', 'plain')
        packed = self.json_cli('find', 'needle', '--source', 'packed')
        self.assertEqual((plain[0], packed[0]), (0, 0), packed[2])
        self.assertEqual(strip_volatile(packed[1]['results']), strip_volatile(plain[1]['results']))
        self.assertEqual(strip_volatile(packed[1]['coverage']['notes']), strip_volatile(plain[1]['coverage']['notes']))
        self.assertEqual(packed[1]['coverage']['unfinished_records'], 1)

    def test_claude_transcript_identity_ignores_the_storage_suffix(self):
        data = line({'type': 'user', 'uuid': 'u1', 'sessionId': A, 'timestamp': '2026-09-20T00:00:00Z',
                     'message': {'role': 'user', 'content': [{'type': 'text', 'text': 'needle in claude'}]}})
        self.put(self.claude, 'project/' + A + '.jsonl.zst', zstd.compress(data))
        code, out, err = self.json_cli('resolve', A, '--source', 'claude')
        self.assertEqual(code, 0, err)
        self.assertEqual(out['results'][0]['id'], A)
        self.assertIsNone(out['results'][0]['segments'][0]['resumed_from'])

    def test_sources_count_compressed_files(self):
        self.twins('rollout-' + C + '.jsonl', rollout(C, ['x']))
        self.put(self.packed, 'rollout-' + D + '.jsonl', rollout(D, ['y']))
        code, out, err = self.json_cli('sources')
        self.assertEqual(code, 0, err)
        self.assertEqual({s['name']: s['files'] for s in out['results']}, {'plain': 1, 'packed': 2, 'claude': 0})

    # ------------------------------------------------------------------------ frame structure

    def test_multiple_frames_and_skippable_frames(self):
        first, second = rollout(C, ['needle one']), line({'type': 'response_item', 'payload': {
            'type': 'message', 'role': 'user', 'content': [{'type': 'input_text', 'text': 'needle two'}]}})
        skippable = (0x184D2A53).to_bytes(4, 'little') + (5).to_bytes(4, 'little') + b'skip!'
        self.put(self.plain, 'rollout-' + C + '.jsonl', first + second)
        self.put(self.packed, 'rollout-' + C + '.jsonl.zst',
                 skippable + zstd.compress(first) + skippable + zstd.compress(second, options={zstd.CompressionParameter.checksum_flag: 1}))
        plain = self.json_cli('find', 'needle', '--source', 'plain')
        packed = self.json_cli('find', 'needle', '--source', 'packed')
        self.assertEqual(packed[0], 0, packed[2])
        self.assertEqual(strip_volatile(packed[1]['results']), strip_volatile(plain[1]['results']))
        self.assertEqual(sorted(r['line'] for r in packed[1]['results']), [2, 3])

    def test_raw_and_rle_blocks(self):
        content = b'{"type": "session_meta", "payload": {"id": "%s"}}\n' % C.encode()
        frame = raw_frame([(0, content, None), (1, b'\n', 3)], len(content) + 3)
        self.put(self.packed, 'rollout-' + C + '.jsonl.zst', frame)
        code, out, err = self.json_cli('resolve', C, '--source', 'packed')
        self.assertEqual(code, 0, err)
        self.assertEqual(out['results'][0]['segments'][0]['bytes'], len(content) + 3)

    def test_empty_files(self):
        self.put(self.packed, 'rollout-empty.jsonl.zst', b'')
        self.put(self.packed, 'rollout-empty-frame.jsonl.zst', zstd.compress(b''))
        code, out, err = self.json_cli('find', 'needle', '--source', 'packed')
        self.assertEqual(code, 4, err)
        self.assertEqual(sorted((Path(e['path']).name, e['kind']) for e in out['coverage']['errors']),
                         [('rollout-empty-frame.jsonl.zst', 'identity_unavailable'),
                          ('rollout-empty.jsonl.zst', 'identity_unavailable')])

    # ------------------------------------------------------------- streams this tool refuses

    def test_frame_without_content_size(self):
        self.put(self.packed, 'rollout-nosize.jsonl.zst',
                 zstd.compress(rollout(C, ['needle']), options={zstd.CompressionParameter.content_size_flag: 0}))
        self.assert_partial('read_error', 'zstd: frame without content size', 'rollout-nosize.jsonl.zst')

    def test_dictionary_frame(self):
        self.put(self.packed, 'rollout-dict.jsonl.zst', MAGIC + bytes([0x21, 5, 1]) + (1 | 8).to_bytes(3, 'little') + b'x')
        self.assert_partial('read_error', 'zstd: frame needs a dictionary', 'rollout-dict.jsonl.zst')

    def test_reserved_header_bit(self):
        self.put(self.packed, 'rollout-reserved.jsonl.zst', MAGIC + bytes([0x28, 1]) + (1 | 8).to_bytes(3, 'little') + b'x')
        self.assert_partial('read_error', 'zstd: reserved frame header bit set', 'rollout-reserved.jsonl.zst')

    def test_reserved_block_type(self):
        self.put(self.packed, 'rollout-block.jsonl.zst', MAGIC + bytes([0x20, 1]) + (1 | 6 | 8).to_bytes(3, 'little') + b'x')
        self.assert_partial('read_error', 'zstd: reserved block type', 'rollout-block.jsonl.zst')

    def test_truncated_frame(self):
        self.put(self.packed, 'rollout-cut.jsonl.zst', zstd.compress(rollout(C, ['needle ' * 400]))[:-9])
        self.assert_partial('read_error', 'zstd: truncated frame', 'rollout-cut.jsonl.zst')

    def test_bytes_that_are_not_a_frame(self):
        self.put(self.packed, 'rollout-text.jsonl.zst', rollout(C, ['needle']))
        self.assert_partial('read_error', 'zstd: not a zstd frame', 'rollout-text.jsonl.zst')

    def test_trailing_bytes_after_the_last_frame(self):
        self.put(self.packed, 'rollout-trailing.jsonl.zst', zstd.compress(rollout(C, ['needle'])) + b'\0\0\0\0junk')
        self.assert_partial('read_error', 'zstd: not a zstd frame', 'rollout-trailing.jsonl.zst')

    def test_corrupt_block_data(self):
        data = rollout(C, ['needle ' + ' '.join(str(i) for i in range(4000))])
        packed = bytearray(zstd.compress(data))
        for i in range(40, len(packed) - 8, 97):
            packed[i] ^= 0x5A
        self.put(self.packed, 'rollout-corrupt.jsonl.zst', bytes(packed))
        self.assert_partial('read_error', 'zstd: data could not be decoded', 'rollout-corrupt.jsonl.zst')

    def test_unknown_encodings_are_reported_not_skipped(self):
        self.put(self.packed, 'rollout-' + C + '.jsonl.gz', b'\x1f\x8b not searched')
        self.put(self.packed, 'rollout-' + D + '.jsonl.zst.tmp', b'partial write')
        self.put(self.packed, 'journal.jsonl.zst', zstd.compress(b'{}\n'))
        self.put(self.packed, 'journal.jsonl.bak', b'{}\n')
        self.put(self.packed, 'notes.json', b'{}')
        code, out, err = self.json_cli('sources')
        self.assertEqual(code, 4, err)
        self.assertEqual([(Path(e['path']).name, e['kind'], e['detail']) for e in out['coverage']['errors']], [
            ('rollout-' + C + '.jsonl.gz', 'unsupported_session_file', "unrecognized transcript encoding '.gz'; not searched"),
            ('rollout-' + D + '.jsonl.zst.tmp', 'unsupported_session_file', "unrecognized transcript encoding '.zst.tmp'; not searched")])

    # ------------------------------------------------------------------- Python core internals

    @python_only('frame walker against the standard library')
    def test_content_size_matches_the_standard_library(self):
        sys.path.insert(0, str(PY))
        import xsession
        import random
        rng = random.Random(7)
        for n in (0, 1, 255, 256, 257, 65535, 65536, 65793, 2 ** 20 + 3):
            data = bytes(rng.randrange(97, 123) for _ in range(n))
            for level in (1, 19):
                blob = zstd.compress(data, level=level)
                path = self.put(self.packed, 'probe-%d-%d.bin' % (n, level), blob)
                with path.open('rb') as raw:
                    self.assertEqual(xsession.zstd_content_size(raw, len(blob)), n)
                    self.assertEqual(zstd.get_frame_info(blob).decompressed_size, n)

    @python_only('backward seeks restart the decoder')
    def test_backward_seek_restarts(self):
        sys.path.insert(0, str(PY))
        import xsession
        data = b''.join(b'line %05d\n' % i for i in range(20000))
        path = self.put(self.packed, 'seek.jsonl.zst', zstd.compress(data))
        with xsession.open_session(path) as f:
            f.seek(150000)
            self.assertEqual(f.read(10), data[150000:150010])
            f.seek(10)
            self.assertEqual(f.readline(), data[10:data.index(b'\n', 10) + 1])
            self.assertEqual(sum(1 for _ in f), 19999)

    @python_only('runtime without compression.zstd')
    def test_runtime_without_zstd_reports_partial(self):
        sys.path.insert(0, str(PY))
        import xsession
        self.twins('rollout-' + C + '.jsonl', rollout(C, ['needle']))
        saved = xsession._zstd
        xsession._zstd = None
        try:
            config = {'sources': [{'name': 'packed', 'kind': 'codex', 'path': str(self.packed)}]}
            coverage = xsession.initial_coverage(config)
            rows = xsession.inventory(config, coverage)
        finally:
            xsession._zstd = saved
        self.assertEqual(rows, [])
        self.assertFalse(coverage['complete'])
        self.assertEqual([(e['kind'], e['detail']) for e in coverage['errors']],
                         [('unsupported_session_file', xsession.ZSTD_UNAVAILABLE)])


if __name__ == '__main__':
    unittest.main()
