"""Independent-review regressions. Fixtures never touch installed transcripts."""
import contextlib
import hashlib
import io
import json
import os
from pathlib import Path
import unittest
from unittest.mock import patch
import test_xsession as fixtures
import xsession

A, B, C = fixtures.A, fixtures.B, fixtures.C

class ReviewTests(unittest.TestCase):
    setUp = fixtures.RetrievalTests.setUp
    tearDown = fixtures.RetrievalTests.tearDown
    save_config = fixtures.RetrievalTests.save_config
    write = fixtures.RetrievalTests.write
    claude_file = fixtures.RetrievalTests.claude_file
    codex_file = fixtures.RetrievalTests.codex_file
    run_cli = fixtures.RetrievalTests.run_cli

    def resumed(self):
        return self.write(self.claude, 'project/' + B + '.jsonl', [
            {'type':'user','sessionId':A,'uuid':'inherited',
             'message':{'role':'user','content':'ancestor-only-marker'}},
            {'type':'assistant','sessionId':B,'uuid':'own',
             'message':{'role':'assistant','content':'resumed-only-marker'}}])

    def test_resumed_identity_uses_container_and_retains_ancestry(self):
        self.resumed()
        r = self.run_cli('resolve', B, '--json')
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        row = json.loads(r.stdout)['results'][0]['segments'][0]
        self.assertEqual(row['id'], B)
        self.assertEqual(row.get('resumed_from'), A)

    def test_current_resolves_resumed_claude_identity(self):
        p = self.resumed()
        self.env['CLAUDE_CODE_SESSION_ID'] = B
        r = self.run_cli('current')
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertEqual(Path(r.stdout.strip()), p)

    def test_inherited_records_cannot_claim_resumed_authorship(self):
        self.resumed()
        r = self.run_cli('find', 'ancestor-only-marker', '--json')
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        hit = json.loads(r.stdout)['results'][0]
        self.assertEqual(hit.get('record_session_id'), A)
        self.assertEqual(hit['id'], B)
        self.assertEqual(hit['provenance'], 'resumed_history')

    def test_claude_tool_inputs_are_decoded_and_labelled(self):
        self.write(self.claude, 'p/' + A + '.jsonl', [
            {'type':'assistant','sessionId':A,'message':{'role':'assistant','content':[
                {'type':'tool_use','id':'t','name':'Write','input':{
                    'file_path':r'N:\demo\quoted.txt',
                    'content':'Tool-only Café\nsecond line',
                    'nested':{'items':['deep-marker']}}}]}}])
        for needle in (r'N:\demo\quoted.txt', 'Café\nsecond line', 'deep-marker'):
            r = self.run_cli('find', needle, '--json')
            self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
            hit = json.loads(r.stdout)['results'][0]
            self.assertEqual(hit['role'], 'assistant')
            self.assertEqual(hit['provenance'], 'tool_input')

    def test_codex_structured_arguments_are_decoded_and_labelled(self):
        self.write(self.codex, 'rollout.jsonl', [
            {'type':'session_meta','payload':{'id':C}},
            {'type':'response_item','payload':{'type':'function_call',
                'arguments':json.dumps({'cmd':r'type N:\example\file.txt'})}}])
        r = self.run_cli('find', r'N:\example\file.txt', '--json')
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertEqual(json.loads(r.stdout)['results'][0]['provenance'], 'tool_input')

    def test_unexpected_payload_is_partial_not_process_crash(self):
        p = self.codex_file('rollout.jsonl', text='needle remains searchable')
        with p.open('a', encoding='utf-8') as f:
            f.write(json.dumps({'type':'response_item','payload':'unexpected'}) + '\n')
        r = self.run_cli('find', 'needle', '--json')
        self.assertEqual(r.returncode, 4, r.stderr)
        out = json.loads(r.stdout)
        self.assertEqual(len(out['results']), 1)
        self.assertEqual(out['coverage']['unsupported_records'], 1)
        self.assertNotIn('Traceback', r.stderr)

    @fixtures.python_only('CLI: test_binary_equivalents.test_operational_error_is_json_exit_six')
    def test_operational_exception_has_json_and_exit_six(self):
        with patch.object(xsession, 'inventory', side_effect=RuntimeError('probe failure')):
            output = io.StringIO()
            try:
                with contextlib.redirect_stdout(output), contextlib.redirect_stderr(io.StringIO()):
                    code = xsession.main(['find','needle','--sources',str(self.cfg),'--json'])
            except Exception as exc:
                code = type(exc).__name__
        self.assertEqual(code, 6)
        self.assertEqual(json.loads(output.getvalue())['status'], 'operational_error')

    def test_exact_path_is_returned_with_explicit_partial_coverage(self):
        p = self.claude_file(A)
        self.sources['sources'].append({'name':'missing','kind':'codex','path':str(self.root/'absent')})
        self.save_config()
        r = self.run_cli('path', A)
        self.assertEqual(r.returncode, 4, r.stderr)
        self.assertEqual(r.stdout.strip(), str(p))
        self.assertIn('"complete": false', r.stderr)
        legacy = self.run_cli('path', A, '--legacy')
        self.assertEqual(legacy.returncode, 4)
        self.assertIn('session_id: ' + A, legacy.stdout)

    def test_partial_coverage_never_certifies_a_prefix_path(self):
        self.claude_file(A)
        self.sources['sources'].append({'name':'missing','kind':'codex','path':str(self.root/'absent')})
        self.save_config()
        r = self.run_cli('path', A[:8])
        self.assertEqual(r.returncode, 4)
        self.assertEqual(r.stdout, '')

    @fixtures.python_only('Rust: tests::append_after_inventory_preserves_completed_prefix')
    def test_append_after_inventory_preserves_completed_prefix(self):
        p = self.claude_file(A, 'needle original')
        coverage = xsession.initial_coverage(self.sources)
        rows = xsession.inventory(self.sources, coverage)
        with p.open('a', encoding='utf-8') as f:
            f.write(json.dumps({'type':'user','sessionId':A,'message':{'role':'user','content':'later'}}) + '\n')
        records = list(xsession.records(rows[0], coverage))
        self.assertTrue(coverage['complete'], coverage)
        self.assertEqual([r['text'] for r in records], ['needle original'])

    def test_unfinished_append_tail_is_disclosed_without_failing_prefix(self):
        p = self.claude_file(A, 'needle original')
        with p.open('ab') as f:
            f.write(b'{"type":"user"')
        r = self.run_cli('find', 'needle', '--json')
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        out = json.loads(r.stdout)
        self.assertEqual(out['coverage']['unfinished_records'], 1)
        self.assertTrue(out['coverage']['complete'])
        self.assertTrue(out['coverage'].get('notes'))

    def first_page(self):
        self.env['XSESSION_SNAPSHOT_DIR'] = str(self.root/'snapshots')
        self.claude_file(A, 'needle first')
        self.claude_file(B, 'needle second')
        r = self.run_cli('find', 'needle', '--limit','1','--json')
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        return json.loads(r.stdout)

    def test_cursor_survives_append_and_excludes_new_records(self):
        page = self.first_page()
        with (self.claude/'project'/(A+'.jsonl')).open('a',encoding='utf-8') as f:
            f.write(json.dumps({'type':'user','sessionId':A,'message':{'role':'user','content':'needle later'}})+'\n')
        r = self.run_cli('find','needle','--limit','1','--cursor',page['next_cursor'],'--json')
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        out = json.loads(r.stdout)
        self.assertEqual(out['results'][0]['id'], B)
        self.assertEqual(out['total_matches'], 2)
        self.assertIn('snapshot', out)

    def test_cursor_detects_prefix_rewrite_even_with_restored_mtime(self):
        page = self.first_page()
        p = self.claude/'project'/(A+'.jsonl')
        before = p.stat()
        p.write_bytes(p.read_bytes().replace(b'first',b'other'))
        os.utime(p, ns=(before.st_atime_ns,before.st_mtime_ns))
        r = self.run_cli('find','needle','--cursor',page['next_cursor'],'--json')
        self.assertEqual(r.returncode, 3, r.stdout + r.stderr)
        self.assertEqual(json.loads(r.stdout)['status'], 'stale_cursor')

    def test_cursor_reports_missing_snapshot_as_stale(self):
        page = self.first_page()
        artifacts = list((self.root/'snapshots').glob('*.json'))
        self.assertEqual(len(artifacts), 1)
        artifacts[0].unlink()
        r = self.run_cli('find','needle','--cursor',page['next_cursor'],'--json')
        self.assertEqual(r.returncode, 3, r.stdout + r.stderr)

    def test_cursor_rejects_changed_query(self):
        page = self.first_page()
        r = self.run_cli('find','different','--cursor',page['next_cursor'],'--json')
        self.assertEqual(r.returncode, 3, r.stdout + r.stderr)

    def test_conflicting_current_identities_do_not_select_a_different_chat(self):
        self.claude_file(A)
        self.codex_file('rollout.jsonl')
        self.env['CLAUDE_CODE_SESSION_ID'] = A
        self.env['CODEX_THREAD_ID'] = C
        r = self.run_cli('current','--json')
        self.assertEqual(r.returncode, 3, r.stdout + r.stderr)
        self.assertEqual(json.loads(r.stdout)['status'], 'ambiguous_current_identity')

    def test_cursor_detects_changed_title_metadata(self):
        self.env['XSESSION_SNAPSHOT_DIR'] = str(self.root/'snapshots')
        self.claude_file(A, 'needle first')
        self.codex_file('rollout.jsonl', text='needle second')
        index = self.write(self.root,'titles.jsonl',[{'id':C,'thread_name':'needle title'}])
        self.sources['title_index'] = str(index)
        self.save_config()
        page = json.loads(self.run_cli('find','needle','--limit','1','--json').stdout)
        index.write_text(json.dumps({'id':C,'thread_name':'other title'})+'\n',encoding='utf-8')
        r = self.run_cli('find','needle','--cursor',page['next_cursor'],'--json')
        self.assertEqual(r.returncode, 3, r.stdout + r.stderr)

    def test_broad_search_limit_is_an_explicit_error(self):
        for sid in (A,B,C):
            self.claude_file(sid, 'needle')
        r = self.run_cli('find','needle','--max-matches','2','--json')
        self.assertEqual(r.returncode, 2, r.stdout + r.stderr)
        out = json.loads(r.stdout) if r.stdout else {}
        self.assertEqual(out.get('status'), 'match_limit_exceeded')

    def test_claude_custom_title_is_searchable(self):
        self.write(self.claude,'project/'+A+'.jsonl',[
            {'type':'user','sessionId':A,'message':{'role':'user','content':'normal prompt'}},
            {'type':'custom-title','sessionId':A,'customTitle':'unique title marker'}])
        r = self.run_cli('find','unique title marker','--json')
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertEqual(json.loads(r.stdout)['results'][0]['provenance'], 'title_metadata')

    def test_list_declares_bounded_read_and_finds_tail_message(self):
        rows = [{'type':'user','sessionId':A,'timestamp':'2026-09-01T00:00:00Z',
                 'message':{'role':'user','content':'first message'}}]
        rows.extend({'type':'attachment','sessionId':A,'padding':'x'*1000} for _ in range(3000))
        rows.append({'type':'assistant','sessionId':A,'timestamp':'2026-10-01T00:00:00Z',
                     'message':{'role':'assistant','content':'last message'}})
        rows.append({'type':'custom-title','sessionId':A,'customTitle':'Resume review title'})
        self.write(self.claude,'project/'+A+'.jsonl',rows)
        r = self.run_cli('list','1','--json')
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        out = json.loads(r.stdout)
        self.assertEqual(out['results'][0]['timestamp'], '2026-10-01T00:00:00Z')
        self.assertEqual(out['results'][0]['title'], 'Resume review title')
        self.assertEqual(out['coverage'].get('mode'), 'bounded_inventory')

    def test_list_keeps_a_complete_record_at_the_tail_window_boundary(self):
        p = self.claude_file(A, 'old', '2026-09-01T00:00:00Z')
        last = {'type':'assistant','sessionId':A,'timestamp':'2026-10-01T00:00:00Z',
                'message':{'role':'assistant','content':''}}
        last['message']['content'] = 'x'*(65536-len((json.dumps(last)+'\n').encode()))
        raw = (json.dumps(last)+'\n').encode()
        self.assertEqual(len(raw),65536)
        with p.open('ab') as f:
            f.write((json.dumps({'type':'attachment','padding':'x'*80000})+'\n').encode())
            f.write(raw)
        out = json.loads(self.run_cli('list','1','--json').stdout)
        self.assertEqual(out['results'][0]['timestamp'],'2026-10-01T00:00:00Z')

    def test_nested_tool_results_have_distinct_reopenable_locators(self):
        self.write(self.claude,'project/'+A+'.jsonl',[
            {'type':'user','sessionId':A,'message':{'role':'user','content':[
                {'type':'tool_result','tool_use_id':'t','content':[
                    {'type':'text','text':'needle first result'},
                    {'type':'text','text':'needle second result'}]}]}}])
        first = json.loads(self.run_cli('find','needle','--limit','1','--json').stdout)
        second = json.loads(self.run_cli('find','needle','--limit','1','--cursor',first['next_cursor'],'--json').stdout)
        self.assertNotEqual(first['results'][0]['block'],second['results'][0]['block'])
        self.assertEqual(second['results'][0]['text'],'needle second result')

    def test_tool_input_keys_cannot_collide_with_nested_field_paths(self):
        self.write(self.claude,'project/'+A+'.jsonl',[
            {'type':'assistant','sessionId':A,'message':{'role':'assistant','content':[
                {'type':'tool_use','name':'Write','input':{'a.b':'needle flat','a':{'b':'needle nested'}}}]}}])
        out=json.loads(self.run_cli('find','needle','--json').stdout)
        self.assertEqual(len({h['block'] for h in out['results']}),2)

if __name__ == '__main__':
    unittest.main()
