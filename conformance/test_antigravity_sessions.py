"""Antigravity transcripts, added in 2.6.0 / 3.3.0. Only the inside of <USER_REQUEST> in a
USER_EXPLICIT USER_INPUT is the operator's; the app's text around it, system steps and subagent
prompts are labelled as such; `thinking` is never indexed or printed; record digests follow the
raw-line-with-newline convention. The same tests drive the Python core, or the binary named by
XSESSION_BINARY. Fixtures: antigravity_fixture.py."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

BASE = Path(__file__).resolve().parent
sys.path.insert(0, str(BASE))
PY = BASE.parent / 'python'  # the Python implementation under test
import antigravity_fixture as fx  # noqa: E402

BINARY = os.environ.get('XSESSION_BINARY')


class AntigravityTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.TemporaryDirectory(dir=BASE)
        cls.root = fx.build(Path(cls.tmp.name) / 'fixture')

    @classmethod
    def tearDownClass(cls):
        cls.tmp.cleanup()

    def env(self, config='sources.json', **extra):
        env = {k: v for k, v in os.environ.items()
               if not k.startswith('XSESSION_') and k not in ('CODEX_THREAD_ID', 'CLAUDE_CODE_SESSION_ID')}
        env.update(PYTHONIOENCODING='utf-8', **extra)
        if config:
            env['XSESSION_CONFIG'] = str(self.root / config)
        return env

    def run_cli(self, *args, config='sources.json', **extra):
        command = [BINARY, *args] if BINARY else [sys.executable, '-B', str(PY / 'xsession.py'), *args]
        return subprocess.run(command, env=self.env(config, **extra), capture_output=True, text=True,
                              encoding='utf-8', timeout=120)

    def json_cli(self, command, *args, config='sources.json', **extra):
        r = self.run_cli(command, '--json', *args, config=config, **extra)
        return r.returncode, json.loads(r.stdout) if r.stdout.strip() else None, r.stderr

    def find(self, query, *extra, config='sources.json'):
        code, out, err = self.json_cli('find', '--limit', '1000', *extra, '--', query, config=config)
        self.assertIn(code, (0, 1), err)
        return out['results']

    def only(self, query, **filters):
        hits = [h for h in self.find(query) if all(h.get(k) == v for k, v in filters.items())]
        self.assertEqual(len(hits), 1, hits)
        return hits[0]

    def raw_line(self, conversation, number, root='brain'):
        path = self.root / root / conversation / '.system_generated' / 'logs' / 'transcript.jsonl'
        return path.read_bytes().splitlines(keepends=True)[number - 1]

    # ------------------------------------------------------------- operator text vs app text

    def test_user_request_is_the_operator_and_metadata_is_injected(self):
        hit = self.only('needle operator request alpha')
        self.assertEqual((hit['agent'], hit['id'], hit['line'], hit['block']), ('antigravity', fx.A, 1, 0))
        self.assertEqual((hit['role'], hit['provenance'], hit['confidence']), ('user', 'user_message', 'role_only'))
        self.assertEqual(hit['text'], '\nneedle operator request alpha\n')
        # Everything after </USER_REQUEST> is one span of the app's text.
        for query in ('needle metadata time', 'needle settings change'):
            hit = self.only(query, id=fx.A, line=1)
            self.assertEqual((hit['block'], hit['role'], hit['provenance'], hit['confidence']),
                             (1, 'system', 'injected_context', 'record_type'), query)
            self.assertNotIn('needle operator request alpha', hit['text'])
        self.assertEqual(self.find('<USER_REQUEST>'), [])

    def test_inputs_outside_user_request_tags_are_never_the_operator(self):
        for query in ('needle untagged input', 'needle implicit request'):
            hit = self.only(query)
            self.assertEqual((hit['role'], hit['provenance']), ('system', 'injected_context'), query)
        lead = self.only('lead ')
        self.assertEqual((lead['block'], lead['provenance']), (0, 'injected_context'))
        unclosed = self.only('needle unclosed request')
        self.assertEqual((unclosed['block'], unclosed['provenance']), (1, 'user_message'))
        quoted = self.only('needle quoted paste')
        self.assertEqual((quoted['provenance'], quoted['confidence']), ('quoted_or_pasted', 'format_heuristic'))

    def test_step_types(self):
        expected = {
            'needle assistant reply': ('assistant', 'assistant', 'record_role', 0),
            'needle tool output': ('tool', 'tool_output', 'record_type', 0),
            'needle answer chosen': ('tool', 'tool_output', 'record_type', 0),
            'needle system message': ('system', 'injected_context', 'record_type', 0),
            'needle ephemeral': ('system', 'injected_context', 'record_type', 0),
            'needle history of other conversations': ('system', 'injected_context', 'record_type', 0),
            'needle checkpoint summary': ('summary', 'compaction_summary', 'record_type', 0),
            'needle error only': ('system', 'injected_context', 'record_type', '["error"]'),
            'needle future step': ('unknown', 'unknown', 'record_role', 0),
        }
        for query, shape in expected.items():
            hit = self.only(query, agent='antigravity')
            self.assertEqual((hit['role'], hit['provenance'], hit['confidence'], hit['block']), shape, query)

    def test_cross_agent_paste_ranks_the_operator_copy_first(self):
        hits = [(h['agent'], h['provenance']) for h in self.find('draft ruling block for the operator')]
        self.assertEqual(hits, [('claude', 'user_message'), ('antigravity', 'assistant')])

    # --------------------------------------------------------------------------- thinking

    def test_thinking_is_never_indexed_or_printed(self):
        code, out, err = self.json_cli('find', fx.THINKING)
        self.assertEqual((code, out['status'], out['results']), (1, 'no_match', []), err)
        outputs = [self.run_cli('find', 'needle', '--json', '--limit', '1000', '--context', '20'),
                   self.run_cli('find', 'needle', '--limit', '1000'),
                   self.run_cli('list', '1000', '--json'),
                   self.run_cli('resolve', fx.A, '--json')]
        for line in range(1, 23):
            outputs.append(self.run_cli('read', fx.A, '--line', str(line), '--context', '20', '--json'))
        outputs.append(self.run_cli('find', 'needle', '--json', '--limit', '1000', '--context', '20', config='sources-problems.json'))
        for r in outputs:
            self.assertIn(r.returncode, (0, 1, 4), r.stderr)
            self.assertNotIn(fx.THINKING, r.stdout + r.stderr, r.args)
        self.assertIn('step thinking are not', ' '.join(json.loads(outputs[0].stdout)['coverage']['excluded']))

    # ------------------------------------------------------------------------- tool calls

    def test_tool_arguments_are_searched_decoded_with_their_field_path(self):
        hit = self.only('needle_tool.py')
        self.assertEqual((hit['role'], hit['provenance'], hit['line']), ('assistant', 'tool_input', 2))
        self.assertEqual(hit['block'], '["tool_calls",0,"args","CommandLine"]')
        self.assertEqual(hit['text'], 'python N:\\tools\\needle_tool.py --flag')
        self.assertEqual(self.only('N:\\tools\\needle_tool')['line'], 2)
        self.assertEqual(self.only('needle edit target')['block'], '["tool_calls",0,"args","Edits",0,"Target"]')
        self.assertEqual(self.only('needle native argument')['block'], '["tool_calls",0,"args","Nested","Native"]')
        self.assertEqual(self.only('needle running the tool')['block'], '["tool_calls",0,"args","toolAction"]')
        self.assertEqual([h for h in self.find('5000') if h['agent'] == 'antigravity'], [])

    def test_truncated_fields_are_reported_and_searched_as_stored(self):
        arg = self.only('needle truncated arg head')
        self.assertEqual((arg['provenance'], arg['truncated_fields']), ('tool_input', ['tool_calls']))
        self.assertIn('<truncated 397 bytes>', arg['text'])
        answer = self.only('needle head of a long answer')
        self.assertEqual(answer['truncated_fields'], ['content', 'thinking'])
        self.assertNotIn('truncated_fields', self.only('needle tool output'))
        self.assertEqual(self.find('needle full only middle'), [])

    # ---------------------------------------------------------------------------- subagents

    def test_subagent_prompts_are_delegated_not_the_operator(self):
        code, out, err = self.json_cli('resolve', fx.B)
        self.assertEqual(code, 0, err)
        self.assertEqual((out['results'][0]['kind'], out['results'][0]['segments'][0]['parent_id']), ('subagent', fx.A))
        hit = self.only('needle delegated research task')
        self.assertEqual((hit['kind'], hit['parent_id'], hit['role'], hit['provenance'], hit['confidence']),
                         ('subagent', fx.A, 'user', 'delegated_prompt', 'metadata_or_marker'))
        child = self.only('needle message to the child')
        self.assertEqual((child['id'], child['kind'], child['provenance']), (fx.C, 'subagent', 'delegated_prompt'))
        top = self.only('needle top level request delta')
        self.assertEqual((top['kind'], top['parent_id'], top['provenance']), ('conversation', None, 'user_message'))

    # ------------------------------------------------------------------- evidence locators

    def test_record_digest_is_the_raw_line_with_its_newline(self):
        for query, number in (('needle operator request alpha', 1), ('needle assistant reply', 2), ('needle_tool.py', 2)):
            hit = self.only(query, agent='antigravity')
            raw = self.raw_line(fx.A, number)
            self.assertTrue(raw.endswith(b'\n'))
            self.assertEqual(hit['record_sha256'], hashlib.sha256(raw).hexdigest(), query)
            self.assertEqual(hit['byte_offset'], len(b''.join(self.raw_line(fx.A, n) for n in range(1, number))))
            record = json.loads(raw)
            self.assertEqual((hit['timestamp'], hit['timestamp_basis'], hit['record_id']),
                             (record['created_at'], 'record', record['step_index']))

    def test_read_reopens_by_line_and_digest(self):
        digest = hashlib.sha256(self.raw_line(fx.A, 2)).hexdigest()
        code, out, err = self.json_cli('read', fx.A, '--line', '2', '--record-sha256', digest)
        self.assertEqual(code, 0, err)
        self.assertEqual({r['line'] for r in out['results']}, {2})
        self.assertEqual(out['results'][0]['text'], 'needle assistant reply: draft ruling block for the operator')
        code, out, err = self.json_cli('read', fx.A, '--line', '2', '--record-sha256', '0' * 64)
        self.assertEqual((code, out['status']), (3, 'record_changed'), err)
        code, out, err = self.json_cli('read', fx.A[:8], '--line', '1', '--context', '1')
        self.assertEqual(code, 0, err)
        self.assertEqual([(r['line'], r['block']) for r in out['results']], [(1, 0), (1, 1), (2, 0)])

    def test_inventory_reads_only_transcript_jsonl(self):
        code, out, err = self.json_cli('sources')
        self.assertEqual(code, 0, err)
        self.assertEqual([(s['name'], s['files']) for s in out['results']], [('antigravity', 5), ('claude', 1)])
        code, out, err = self.json_cli('resolve', fx.A)
        self.assertEqual(code, 0, err)
        segment = out['results'][0]['segments'][0]
        self.assertEqual(Path(segment['path']).parts[-4:], (fx.A, '.system_generated', 'logs', 'transcript.jsonl'))
        self.assertEqual((segment['timestamp'], segment['timestamp_basis']), ('2026-10-29T10:00:00Z', 'first metadata record'))
        self.assertEqual(self.find('needle in a folder not named by a uuid'), [])
        self.assertEqual(self.find('needle step output file'), [])
        self.assertEqual(self.find('needle artifact'), [])
        self.assertEqual(len(self.find('needle operator request alpha')), 1)
        code, out, err = self.json_cli('resolve', fx.E)
        self.assertEqual((code, out['results'][0]['segments'][0]['bytes']), (0, 0), err)
        code, out, err = self.json_cli('resolve', fx.F)
        self.assertEqual(code, 1, err)
        excluded = out['coverage']['excluded']
        self.assertEqual(sum('Antigravity' in e for e in excluded), 1, excluded)

    def test_list_previews_the_request_text(self):
        code, out, err = self.json_cli('list', '20')
        self.assertEqual(code, 0, err)
        rows = {r['id']: r for r in out['results']}
        self.assertEqual(rows[fx.A]['preview'], '\nneedle operator request alpha\n')
        self.assertEqual(rows[fx.B]['preview'], '\nneedle delegated research task\n')
        self.assertEqual((rows[fx.C]['kind'], rows[fx.C]['parent_id']), ('subagent', fx.A))
        self.assertEqual(rows[fx.A]['timestamp'], '2026-10-29T10:00:21Z')

    def test_paging_reopens_antigravity_hits(self):
        with tempfile.TemporaryDirectory(dir=BASE) as folder:
            whole = self.find('needle', '--source', 'antigravity')
            seen, cursor = [], None
            for _ in range(20):
                args = ['find', 'needle', '--limit', '4', '--source', 'antigravity', '--snapshot-dir', folder]
                code, out, err = self.json_cli(*(args + (['--cursor', cursor] if cursor else [])))
                self.assertEqual(code, 0, err)
                seen += out['results']
                cursor = out['next_cursor']
                if not cursor:
                    break
            strip = lambda hits: [{k: v for k, v in h.items()} for h in hits]
            self.assertEqual(strip(seen), strip(whole))

    # ----------------------------------------------------------------------- coverage

    def test_problems_are_reported(self):
        code, out, err = self.json_cli('find', 'needle', config='sources-problems.json')
        self.assertEqual(code, 4, err)
        errors = sorted((e['kind'], Path(e['path']).name, e['detail']) for e in out['coverage']['errors'])
        expected = [('malformed_records', 'transcript.jsonl', 'line 2'),
                    ('unsupported_session_file', 'transcript.jsonl.gz', "unrecognized transcript encoding '.gz'; not searched")]
        if os.name == 'nt':
            expected.append(('excluded_link', fx.S, ''))
        self.assertEqual(errors, sorted(expected))
        self.assertEqual((out['coverage']['malformed_records'], out['coverage']['unfinished_records']), (1, 1))
        found = {h['text'] for h in out['results']}
        self.assertTrue({'needle before the damage', 'needle after the damage', 'needle compressed request',
                         'needle compressed reply'} <= found, found)
        self.assertFalse(any('needle behind a junction' in t or 'needle unfinished tail' in t for t in found))

    def test_missing_root_is_partial(self):
        code, out, err = self.json_cli('sources', config='sources-missing.json')
        self.assertEqual((code, out['results'][0]['status']), (4, 'unavailable'), err)
        self.assertEqual([e['kind'] for e in out['coverage']['errors']], ['source_unavailable'])

    def test_default_sources_include_antigravity(self):
        brain = self.root / 'brain'
        absent = str(self.root / 'absent')
        code, out, err = self.json_cli('sources', config=None, ANTIGRAVITY_BRAIN_DIR=str(brain), CLAUDE_PROJECTS_DIR=absent,
                                       CODEX_WIN_SESSIONS_DIR=absent, CHATGPT_PROJECTS_DIR=absent)
        names = [(s['name'], s['kind'], s['path']) for s in out['results']]
        self.assertIn(('antigravity', 'antigravity', str(brain)), names, err)
        self.assertEqual(names.index(('antigravity', 'antigravity', str(brain))), 4)

    def test_windows_home_override_moves_default_sources(self):
        """XSESSION_WINDOWS_HOME replaces the home that the default Windows-side sources sit under."""
        home = self.root / 'winhome'
        env = self.env(config=None, XSESSION_WINDOWS_HOME=str(home))
        for k in ('CLAUDE_PROJECTS_DIR', 'CODEX_WIN_SESSIONS_DIR', 'CODEX_SESSIONS_DIR',
                  'CHATGPT_PROJECTS_DIR', 'ANTIGRAVITY_BRAIN_DIR'):
            env.pop(k, None)
        command = [BINARY] if BINARY else [sys.executable, '-B', str(PY / 'xsession.py')]
        r = subprocess.run([*command, 'sources', '--json'], env=env, capture_output=True, text=True,
                           encoding='utf-8', timeout=120)
        paths = {s['name']: s['path'] for s in json.loads(r.stdout)['results']}
        self.assertEqual(paths['claude'], str(home / '.claude' / 'projects'), r.stderr)
        self.assertEqual(paths['codex-windows'], str(home / '.codex' / 'sessions'), r.stderr)
        self.assertEqual(paths['codex-archive'], str(home / '.codex' / 'archived_sessions'), r.stderr)
        self.assertEqual(paths['chatgpt-projects'], str(home / '.codex' / '.chatgpt-projects'), r.stderr)
        self.assertEqual(paths['antigravity'], str(home / '.gemini' / 'antigravity' / 'brain'), r.stderr)


if __name__ == '__main__':
    unittest.main()
