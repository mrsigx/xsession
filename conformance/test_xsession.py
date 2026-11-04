"""Behavioral regression cases, with fixed identities and hand-derived outcomes."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

BASE = Path(__file__).resolve().parent
PY = BASE.parent / 'python'  # the Python implementation under test
sys.path.insert(0, str(PY))
A = '11111111-1111-4111-8111-111111111111'
B = '11111111-2222-4222-8222-222222222222'
C = '22222222-1111-4111-8111-111111111111'
NATIVE = '01a00000-0000-7000-8000-000000000001'  # synthetic Codex thread id
# Conformance switch: the same tests drive Python 2.3.0, or the binary named here.
BINARY = os.environ.get('XSESSION_BINARY')


def python_only(reason):
    return unittest.skipIf(BINARY, 'in-process Python API; ' + reason)

class RetrievalTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(dir=BASE)
        self.root = Path(self.tmp.name)
        self.claude = self.root / 'claude'
        self.codex = self.root / 'codex'
        self.archive = self.root / 'archive'
        for p in (self.claude, self.codex, self.archive):
            p.mkdir()
        self.cfg = self.root / 'sources.json'
        self.sources = {'sources': [
            {'name': 'claude', 'kind': 'claude', 'path': str(self.claude)},
            {'name': 'codex', 'kind': 'codex', 'path': str(self.codex)},
            {'name': 'archive', 'kind': 'codex', 'path': str(self.archive)}]}
        self.save_config()
        self.env = dict(os.environ, XSESSION_CONFIG=str(self.cfg),
                        CLAUDE_PROJECTS_DIR=str(self.claude),
                        CODEX_SESSIONS_DIR=str(self.codex),
                        CODEX_WIN_SESSIONS_DIR=str(self.archive),
                        PYTHONIOENCODING='utf-8', XSESSION_SNAPSHOT_DIR=str(self.root/'snapshots'))
        self.env.pop('CODEX_THREAD_ID', None)
        self.env.pop('CLAUDE_CODE_SESSION_ID', None)

    def tearDown(self):
        self.tmp.cleanup()

    def save_config(self):
        self.cfg.write_text(json.dumps(self.sources), encoding='utf-8')

    def write(self, root, name, records):
        p = root / name
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(''.join(json.dumps(e) + '\n' for e in records), encoding='utf-8')
        return p

    def claude_file(self, sid, text='plain task', timestamp='2026-09-20T00:00:00Z'):
        return self.write(self.claude, 'project/' + sid + '.jsonl', [
            {'type': 'user', 'uuid': 'msg-' + sid, 'sessionId': sid, 'timestamp': timestamp,
             'message': {'role': 'user', 'content': [{'type': 'text', 'text': text}]}}])

    def codex_file(self, name, sid=C, text='codex message', root=None):
        return self.write(root or self.codex, name, [
            {'type': 'session_meta', 'payload': {'id': sid, 'timestamp': '2026-09-21T00:00:00Z', 'source': 'cli'}},
            {'type': 'response_item', 'timestamp': '2026-09-21T00:01:00Z',
             'payload': {'type': 'message', 'role': 'user', 'content': [{'type': 'input_text', 'text': text}]}}])

    def run_cli(self, *args):
        command = [BINARY, *args] if BINARY else [sys.executable, '-B', str(PY / 'xsession.py'), *args]
        return subprocess.run(command, env=self.env, capture_output=True, text=True,
                              encoding='utf-8', timeout=20)

    def test_unknown_id_never_returns_newest(self):
        self.claude_file(A)
        r = self.run_cli('path', '99999999-9999-4999-8999-999999999999')
        self.assertEqual(r.returncode, 1, r.stderr)
        self.assertEqual(r.stdout, '')

    def test_ambiguous_prefix_returns_candidates_not_path(self):
        self.claude_file(A)
        self.claude_file(B)
        r = self.run_cli('path', '11111111')
        self.assertEqual(r.returncode, 3, r.stderr)
        self.assertEqual(r.stdout, '')
        self.assertIn(A, r.stderr)
        self.assertIn(B, r.stderr)

    def test_exact_path_is_single_clean_line(self):
        p = self.claude_file(A)
        self.claude_file(B)
        r = self.run_cli('path', A)
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(Path(r.stdout.strip()), p)
        self.assertEqual(len(r.stdout.splitlines()), 1)

    def test_segmented_thread_resolves_all_segments(self):
        self.codex_file('rollout-base-' + C + '.jsonl')
        self.codex_file('rollout-tail-' + C + '_' + A + '.jsonl')
        r = self.run_cli('resolve', C, '--json')
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(len(json.loads(r.stdout)['results'][0]['segments']), 2)
        p = self.run_cli('path', C)
        self.assertEqual(p.returncode, 3)
        self.assertEqual(p.stdout, '')

    def test_current_does_not_guess_latest(self):
        self.claude_file(A)
        r = self.run_cli('current')
        self.assertEqual(r.returncode, 1)
        self.assertEqual(r.stdout, '')

    def test_current_prefers_explicit_caller(self):
        self.claude_file(A)
        self.env['CLAUDE_CODE_SESSION_ID'] = A
        r = self.run_cli('current')
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertTrue(r.stdout.strip().endswith(A + '.jsonl'))

    def test_native_url_keeps_host_and_never_substitutes_local(self):
        r = self.run_cli('resolve', 'codex://threads/' + NATIVE + '?hostId=durable', '--json')
        self.assertEqual(r.returncode, 5, r.stderr)
        out = json.loads(r.stdout)
        self.assertEqual(out['status'], 'native_required')
        self.assertEqual(out['native']['host_id'], 'durable')
        self.assertEqual(out['native']['thread_id'], NATIVE)

    def test_missing_source_is_partial_not_clean_no_match(self):
        self.sources['sources'].append({'name': 'missing', 'kind': 'codex', 'path': str(self.root/'absent')})
        self.save_config()
        r = self.run_cli('resolve', A, '--json')
        self.assertEqual(r.returncode, 4, r.stderr)
        out = json.loads(r.stdout)
        self.assertFalse(out['coverage']['complete'])
        self.assertEqual(out['results'], [])

    def test_decoded_literals_unicode_and_leading_hyphens(self):
        self.claude_file(A, 'Use N:\\AGENTS.md and "quoted words"\nCafé --no-verify')
        for query in ('N:\\AGENTS.md', '"quoted words"', 'words"\nCafé', 'CAFÉ', '--no-verify'):
            r = self.run_cli('find', '--json', '--', query)
            self.assertEqual(r.returncode, 0, (query, r.stderr))
            hit = json.loads(r.stdout)['results'][0]
            self.assertEqual(hit['id'], A)
            self.assertIn('line', hit)
            self.assertIn('byte_offset', hit)
            self.assertIn('record_sha256', hit)

    def test_injected_memory_cannot_outrank_original(self):
        self.claude_file(A, 'needle original', '2026-09-01T00:00:00Z')
        self.claude_file(B, '<memory>needle original</memory>', '2026-09-29T00:00:00Z')
        r = self.run_cli('find', 'needle', '--json')
        self.assertEqual(r.returncode, 0, r.stderr)
        hits = json.loads(r.stdout)['results']
        self.assertEqual(hits[0]['id'], A)
        self.assertEqual(hits[1]['provenance'], 'injected_context')
        self.assertEqual(hits[0]['confidence'], 'role_only')

    def test_context_reopens_matched_record_and_neighbors(self):
        self.write(self.claude, 'p/' + A + '.jsonl', [
            {'type': 'user', 'sessionId': A, 'uuid': 'm1', 'message': {'role': 'user', 'content': 'before'}},
            {'type': 'assistant', 'sessionId': A, 'uuid': 'm2', 'message': {'role': 'assistant', 'content': 'needle'}},
            {'type': 'user', 'sessionId': A, 'uuid': 'm3', 'message': {'role': 'user', 'content': 'after'}}])
        r = self.run_cli('find', 'needle', '--context', '1', '--json')
        self.assertEqual(r.returncode, 0, r.stderr)
        hit = json.loads(r.stdout)['results'][0]
        self.assertEqual([x['text'] for x in hit['context']], ['before', 'needle', 'after'])
        opened = self.run_cli('read', A, '--line', '2', '--context', '1', '--json')
        self.assertEqual(opened.returncode, 0, opened.stderr)
        self.assertEqual([x['text'] for x in json.loads(opened.stdout)['results']], ['before', 'needle', 'after'])

    def test_malformed_and_unfinished_records_are_disclosed(self):
        p = self.claude_file(A, 'needle')
        with p.open('ab') as f:
            f.write(b'{broken}\n{"type":')
        r = self.run_cli('find', 'needle', '--json')
        self.assertEqual(r.returncode, 4, r.stderr)
        out = json.loads(r.stdout)
        self.assertEqual(len(out['results']), 1)
        self.assertEqual(out['coverage']['malformed_records'], 1)
        self.assertEqual(out['coverage']['unfinished_records'], 1)

    def test_archived_and_nested_transcripts_are_discoverable(self):
        self.codex_file('rollout-' + C + '.jsonl', text='needle archive', root=self.archive)
        self.write(self.claude, 'p/' + A + '/subagents/agent-abc.jsonl', [
            {'type': 'user', 'sessionId': A, 'message': {'role': 'user', 'content': 'needle child'}}])
        r = self.run_cli('find', 'needle', '--json')
        self.assertEqual(r.returncode, 0, r.stderr)
        hits = json.loads(r.stdout)['results']
        self.assertEqual(len(hits), 2)
        self.assertTrue(any(x['parent_id'] == A and x['kind'] == 'subagent' for x in hits))

    def test_title_metadata_is_searchable_but_not_a_message(self):
        self.codex_file('rollout-' + C + '.jsonl')
        index = self.write(self.root, 'index.jsonl', [{'id': C, 'thread_name': 'needle title'}])
        self.sources['title_index'] = str(index)
        self.save_config()
        r = self.run_cli('find', 'needle', '--json')
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(json.loads(r.stdout)['results'][0]['provenance'], 'title_metadata')

    def test_import_with_missing_original_remains_visible(self):
        self.codex_file('rollout-' + C + '.jsonl', text='needle imported')
        imp = self.root / 'imports.json'
        imp.write_text(json.dumps({'records': [{'imported_thread_id': C,
                                               'source_path': str(self.claude/'gone'/ (A+'.jsonl'))}]}))
        self.sources['imports'] = str(imp)
        self.save_config()
        r = self.run_cli('find', 'needle', '--json')
        self.assertEqual(r.returncode, 0, r.stderr)
        hit = json.loads(r.stdout)['results'][0]
        self.assertEqual(hit['provenance'], 'import_copy')
        self.assertEqual(hit['original_id'], A)
        self.assertFalse(hit['original_available'])

    def test_list_uses_message_time_not_filesystem_mtime(self):
        a = self.claude_file(A, 'older', '2026-09-01T00:00:00Z')
        b = self.claude_file(B, 'newer', '2026-09-20T00:00:00Z')
        os.utime(a, (1900000000, 1900000000))
        os.utime(b, (1000000000, 1000000000))
        r = self.run_cli('list', '1', '--json')
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(json.loads(r.stdout)['results'][0]['id'], B)

    def test_pagination_is_stable_and_detects_rewritten_prefixes(self):
        self.claude_file(A, 'needle a')
        self.claude_file(B, 'needle b')
        r = self.run_cli('find', 'needle', '--limit', '1', '--json')
        self.assertEqual(r.returncode, 0, r.stderr)
        first = json.loads(r.stdout)
        self.assertEqual(len(first['results']), 1)
        self.assertTrue(first['truncated'])
        cursor = first['next_cursor']
        r2 = self.run_cli('find', 'needle', '--limit', '1', '--cursor', cursor, '--json')
        self.assertEqual(r2.returncode, 0, r2.stderr)
        self.assertNotEqual(first['results'][0]['id'], json.loads(r2.stdout)['results'][0]['id'])
        self.claude_file(C, 'needle c')
        continuing = self.run_cli('find', 'needle', '--cursor', cursor, '--json')
        self.assertEqual(continuing.returncode, 0)
        self.assertEqual(json.loads(continuing.stdout)['total_matches'], 2)
        self.claude_file(A, 'needle rewritten')
        stale = self.run_cli('find', 'needle', '--cursor', cursor, '--json')
        self.assertEqual(stale.returncode, 3)
        self.assertEqual(json.loads(stale.stdout)['status'], 'stale_cursor')

    def test_document_sources_remain_labelled_documents(self):
        docs = self.root/'docs'
        docs.mkdir()
        (docs/'HANDOFF.md').write_text('cached needle ruling\n', encoding='utf-8')
        self.sources['sources'].append({'name': 'handoffs', 'kind': 'document', 'path': str(docs)})
        self.save_config()
        r = self.run_cli('find', 'needle', '--json')
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(json.loads(r.stdout)['results'][0]['provenance'], 'document')

    def test_empty_query_and_invalid_limits_are_usage_errors(self):
        for args in [('find', ''), ('find', 'x', '--limit', '0'), ('list', '-1')]:
            r = self.run_cli(*args)
            self.assertEqual(r.returncode, 2, r.stderr)

    def test_read_rejects_a_changed_record_digest(self):
        p = self.claude_file(A, 'needle')
        r = self.run_cli('find', 'needle', '--json')
        hit = json.loads(r.stdout)['results'][0]
        self.claude_file(A, 'replacement')
        r = self.run_cli('read', A, '--line', '1', '--record-sha256', hit['record_sha256'], '--json')
        self.assertEqual(r.returncode, 3, r.stderr)
        self.assertEqual(json.loads(r.stdout)['status'], 'record_changed')

    @python_only('Rust: tests::changed_between_inventory_and_read_is_partial')
    def test_changed_between_inventory_and_read_is_partial(self):
        import xsession
        p = self.claude_file(A, 'needle')
        coverage = xsession.initial_coverage(self.sources)
        rows = xsession.inventory(self.sources, coverage)
        self.claude_file(A, 'needle changed after inventory')
        list(xsession.records(rows[0], coverage))
        self.assertFalse(coverage['complete'])
        self.assertTrue(any(e['kind'] == 'changed_since_inventory' for e in coverage['errors']))

    def test_legacy_resolver_preserves_metadata_and_fails_unknown(self):
        self.claude_file(A)
        r = self.run_cli('path', A, '--legacy')
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn('session_id: ' + A, r.stdout)
        self.assertIn('transcript: ', r.stdout)
        bad = self.run_cli('path', B, '--legacy')
        self.assertEqual(bad.returncode, 1)
        self.assertEqual(bad.stdout, '')

    @unittest.skipIf(sys.platform != 'win32', 'asserts the Git Bash /n/ form of a Windows path')
    def test_cross_platform_path_format_is_explicit(self):
        self.claude_file(A)
        r = self.run_cli('path', A, '--path-format', 'msys')
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertTrue(r.stdout.startswith('/n/'), r.stdout)

    def test_metadata_only_api_pagination_has_coverage_and_no_fake_file(self):
        r = self.run_cli('resolve', 'codex://threads/' + NATIVE + '?hostId=durable', '--json')
        out = json.loads(r.stdout)
        self.assertFalse(out['coverage']['complete'])
        self.assertEqual(out['results'], [])

    def test_nonobject_json_records_do_not_crash_reader(self):
        p = self.claude_file(A, 'needle')
        with p.open('a', encoding='utf-8') as f:
            f.write('[]\nnull\n123\n')
        r = self.run_cli('find', 'needle', '--json')
        self.assertEqual(r.returncode, 4, r.stderr)
        self.assertEqual(json.loads(r.stdout)['coverage']['unsupported_records'], 3)

    def test_compaction_flag_is_not_operator_authorship(self):
        self.write(self.claude, 'p/' + A + '.jsonl', [
            {'type': 'user', 'sessionId': A, 'isCompactSummary': True,
             'message': {'role': 'user', 'content': 'needle summarized'}}])
        r = self.run_cli('find', 'needle', '--json')
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(json.loads(r.stdout)['results'][0]['provenance'], 'compaction_summary')

    def test_proven_import_copy_collapses_without_hiding_orphan(self):
        self.claude_file(A, 'needle original')
        self.codex_file('rollout-' + C + '.jsonl', text='needle original')
        imp = self.root/'imports.json'
        imp.write_text(json.dumps({'records':[{'imported_thread_id':C,'source_path':str(self.claude/'project'/(A+'.jsonl'))}]}))
        self.sources['imports'] = str(imp)
        self.save_config()
        r = self.run_cli('find', 'needle', '--json')
        out = json.loads(r.stdout)
        self.assertEqual(len(out['results']),1)
        self.assertEqual(out['results'][0]['copies'][0]['id'], C)

    def test_result_byte_budget_reports_continuation(self):
        for i in range(100):
            sid = f'{i:08x}-1111-4111-8111-111111111111'
            self.claude_file(sid, 'needle ' + 'x'*5000)
        r = self.run_cli('find', 'needle', '--limit', '100', '--max-output-bytes', '8000', '--json')
        self.assertEqual(r.returncode, 0, r.stderr)
        out = json.loads(r.stdout)
        self.assertLess(len(r.stdout.encode('utf-8')), 10000)
        self.assertTrue(out['truncated'])
        self.assertIsNotNone(out['next_cursor'])

    def test_list_orders_absolute_times_across_offsets(self):
        self.claude_file(A, 'later', '2026-09-20T00:00:00-07:00')
        self.claude_file(B, 'earlier', '2026-09-20T03:00:00Z')
        r = self.run_cli('list', '1', '--json')
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(json.loads(r.stdout)['results'][0]['id'], A)

    def test_nonobject_config_is_a_usage_error(self):
        self.cfg.write_text('[]', encoding='utf-8')
        r = self.run_cli('resolve', A, '--json')
        self.assertEqual(r.returncode, 2, r.stderr)

    def test_casefold_expansion_does_not_move_snippet_past_match(self):
        self.claude_file(A, '\u00df'*1000 + ' needle end')
        r = self.run_cli('find', 'needle', '--json')
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn('needle', json.loads(r.stdout)['results'][0]['text'])

if __name__ == '__main__':
    unittest.main()
