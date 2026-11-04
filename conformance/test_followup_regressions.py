"""Real regressions from Claude's 2.1.1 verification; source fixtures are local."""
import concurrent.futures
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

class FollowupTests(unittest.TestCase):
    setUp = fixtures.RetrievalTests.setUp
    tearDown = fixtures.RetrievalTests.tearDown
    save_config = fixtures.RetrievalTests.save_config
    write = fixtures.RetrievalTests.write
    claude_file = fixtures.RetrievalTests.claude_file
    run_cli = fixtures.RetrievalTests.run_cli

    def resumed(self, child=B, text='needle inherited user'):
        return self.write(self.claude, 'project/' + child + '.jsonl', [
            {'type':'user','sessionId':A,'uuid':'inherited',
             'message':{'role':'user','content':text}},
            {'type':'assistant','sessionId':child,'uuid':'own',
             'message':{'role':'assistant','content':'own resumed message'}}])

    def two_hits(self):
        self.claude_file(A, 'needle one')
        self.claude_file(B, 'needle two')

    def test_missing_parent_returns_related_evidence_not_clean_no_match(self):
        child = self.resumed()
        r = self.run_cli('resolve', A, '--json')
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        data = json.loads(r.stdout)
        self.assertEqual(data['status'], 'related_history_available')
        group = data['results'][0]
        self.assertEqual(group['id'], A)
        self.assertEqual(group['segments'], [])
        self.assertEqual(group['related_segments'][0]['id'], B)
        self.assertEqual(group['related_segments'][0]['path'], str(child))

    def test_parent_path_cannot_silently_select_child_container(self):
        self.resumed()
        r = self.run_cli('path', A)
        self.assertEqual(r.returncode, 3, r.stdout + r.stderr)
        self.assertEqual(r.stdout, '')
        self.assertIn('related_history_use_resolve', r.stderr)

    def test_parent_read_requires_explicit_container_identity(self):
        self.resumed()
        r = self.run_cli('read', A, '--line', '1', '--json')
        self.assertEqual(r.returncode, 3, r.stdout + r.stderr)
        self.assertEqual(json.loads(r.stdout)['status'], 'related_history_use_resolve')

    def test_original_parent_path_wins_and_related_evidence_remains_visible(self):
        original = self.claude_file(A)
        self.resumed()
        r = self.run_cli('path', A)
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertEqual(r.stdout.strip(), str(original))
        group = json.loads(self.run_cli('resolve', A, '--json').stdout)['results'][0]
        self.assertEqual([s['id'] for s in group.get('related_segments', [])], [B])

    def test_prefix_ambiguity_includes_parent_identity(self):
        self.resumed()
        r = self.run_cli('path', '11111111')
        self.assertEqual(r.returncode, 3, r.stdout + r.stderr)
        self.assertEqual(r.stdout, '')
        self.assertIn(A, r.stderr)
        self.assertIn(B, r.stderr)

    def test_parent_with_two_children_is_one_identity_with_two_relations(self):
        self.resumed()
        self.resumed(C)
        r = self.run_cli('resolve', A, '--json')
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        groups = json.loads(r.stdout)['results']
        self.assertEqual(len(groups), 1)
        self.assertEqual({s['id'] for s in groups[0]['related_segments']}, {B,C})

    def test_surviving_user_history_ranks_above_injected_echo(self):
        self.resumed()
        self.claude_file(C, '<memory>needle injected echo</memory>')
        r = self.run_cli('find', 'needle', '--json')
        hits = json.loads(r.stdout)['results']
        self.assertEqual([h['id'] for h in hits], [B,C])
        self.assertEqual(hits[0]['provenance'], 'resumed_history')
        self.assertEqual(hits[0]['content_provenance'], 'user_message')
        self.assertIs(hits[0]['original_available'], False)
        self.assertEqual(hits[0]['record_session_id'], A)

    def test_inherited_injection_is_not_promoted_to_user_evidence(self):
        self.resumed(text='<memory>needle inherited echo</memory>')
        self.claude_file(C, 'needle actual user turn')
        hits = json.loads(self.run_cli('find','needle','--json').stdout)['results']
        self.assertEqual(hits[0]['id'], C)
        inherited = next(h for h in hits if h['id'] == B)
        self.assertEqual(inherited.get('content_provenance'), 'injected_context')

    def test_available_original_stays_ahead_of_inherited_copy(self):
        self.resumed()
        self.claude_file(A, 'needle original')
        hits = json.loads(self.run_cli('find','needle','--json').stdout)['results']
        self.assertEqual(hits[0]['id'], A)
        self.assertIs(hits[1].get('original_available'), True)

    @fixtures.python_only('CLI: test_binary_equivalents.test_default_page_writes_no_snapshot')
    def test_default_page_does_not_attempt_persistent_storage(self):
        self.two_hits()
        self.env.pop('XSESSION_SNAPSHOT_DIR')
        out, err = io.StringIO(), io.StringIO()
        # Prevent the old implementation from writing to a sealed live package.
        with patch.dict(os.environ,self.env,clear=True), patch.object(xsession,'save_snapshot',side_effect=OSError('unexpected persistent write')):
            with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
                code = xsession.main(['find','needle','--limit','1','--json'])
        self.assertEqual(code, 0, out.getvalue()+err.getvalue())
        data = json.loads(out.getvalue())
        self.assertEqual(len(data['results']), 1)
        self.assertTrue(data['truncated'])
        self.assertIsNone(data['next_cursor'])
        self.assertEqual(data['pagination']['status'], 'disabled')

    def test_unwritable_snapshot_location_keeps_successful_page(self):
        self.two_hits()
        blocked = self.root/'blocked'
        blocked.write_text('existing file',encoding='utf-8')
        self.env['XSESSION_SNAPSHOT_DIR'] = str(blocked)
        r = self.run_cli('find','needle','--limit','1','--json')
        self.assertEqual(r.returncode, 0, r.stdout+r.stderr)
        data = json.loads(r.stdout)
        self.assertEqual(data['total_matches'], 2)
        self.assertEqual(len(data['results']), 1)
        self.assertIsNone(data['next_cursor'])
        self.assertEqual(data['pagination']['status'], 'unavailable')
        self.assertTrue(data['truncated'])
        self.assertEqual(blocked.read_text(), 'existing file')

    def test_plain_output_discloses_snapshot_failure_with_results(self):
        self.two_hits()
        blocked = self.root/'blocked'
        blocked.write_text('existing file',encoding='utf-8')
        self.env['XSESSION_SNAPSHOT_DIR'] = str(blocked)
        r = self.run_cli('list','1')
        self.assertEqual(r.returncode, 0, r.stdout+r.stderr)
        self.assertIn('"id":', r.stdout)
        self.assertIn('"status": "unavailable"', r.stderr)

    def test_explicit_directory_supports_later_continuation(self):
        self.two_hits()
        self.env.pop('XSESSION_SNAPSHOT_DIR')
        folder = self.root/'chosen'
        common = ['find','needle','--limit','1','--json','--snapshot-dir',str(folder)]
        r = self.run_cli(*common)
        self.assertEqual(r.returncode, 0, r.stdout+r.stderr)
        first = json.loads(r.stdout)
        self.assertTrue(first['next_cursor'])
        second = self.run_cli(*common,'--cursor',first['next_cursor'])
        self.assertEqual(second.returncode, 0, second.stdout+second.stderr)
        self.assertEqual(json.loads(second.stdout)['results'][0]['id'], B)

    def test_snapshot_slots_are_bounded_even_under_concurrent_writers(self):
        self.two_hits()
        folder = self.root/'snapshots'
        with concurrent.futures.ThreadPoolExecutor(max_workers=6) as pool:
            results = list(pool.map(lambda _: self.run_cli('list','1','--json'), range(20)))
        data = []
        for r in results:
            self.assertEqual(r.returncode, 0, r.stdout+r.stderr)
            data.append(json.loads(r.stdout))
        continued = [d for d in data if d['next_cursor']]
        refused = [d for d in data if not d['next_cursor']]
        self.assertEqual(len(continued), 16)
        self.assertEqual(len(refused), 4)
        self.assertEqual(len(list(folder.iterdir())), 16)
        for d in refused:
            self.assertEqual(len(d['results']), 1)
            self.assertEqual(d['pagination']['status'], 'unavailable')
        for d in continued:
            r = self.run_cli('list','1','--json','--cursor',d['next_cursor'])
            self.assertEqual(r.returncode, 0, r.stdout+r.stderr)

    def test_oversized_snapshot_keeps_page_without_writing(self):
        self.two_hits()
        titles = self.root/'titles.jsonl'
        titles.write_text(json.dumps({'id':C,'thread_name':'x'*(5*1024*1024)})+'\n',encoding='utf-8')
        self.sources['title_index'] = str(titles)
        self.save_config()
        r = self.run_cli('find','needle','--limit','1','--json')
        self.assertEqual(r.returncode, 0, r.stdout+r.stderr)
        data = json.loads(r.stdout)
        self.assertEqual(len(data['results']), 1)
        self.assertIsNone(data['next_cursor'])
        self.assertEqual(data['pagination']['status'], 'unavailable')
        self.assertFalse(list((self.root/'snapshots').glob('*')))

if __name__ == '__main__':
    unittest.main()
