"""Readers for the official ChatGPT and claude.ai data exports (kinds chatgpt_export and
claude_export). An export arrives as a ZIP; its conversations.json (or numbered
conversations-NNN.json) is a JSON array of conversations, and each conversation is one session.
Every identity, time and text here is synthetic.

The same tests drive the Python core, or the binary named by XSESSION_BINARY."""
import hashlib
import json
import unittest
import zipfile

import test_xsession as base

G1, G2 = '6700aaaa-0000-8000-8000-000000000001', '6700aaaa-0000-8000-8000-000000000002'
C1 = '1c1a0000-0000-4000-8000-000000000001'


def gpt_message(mid, role, content, created=1700000000.25, recipient='all', hidden=None, author_name=None):
    metadata = {} if hidden is None else {'is_visually_hidden_from_conversation': hidden}
    return {'id': mid, 'author': {'role': role, 'name': author_name, 'metadata': {}}, 'create_time': created,
            'update_time': None, 'content': content, 'status': 'finished_successfully', 'end_turn': None,
            'weight': 1.0, 'metadata': metadata, 'recipient': recipient, 'channel': None}


def text(*parts):
    return {'content_type': 'text', 'parts': list(parts)}


def chatgpt_conversation():
    """A tree stored out of order: the root, a user turn edited once (two branches), and a tool round trip.
    current_node ends the edited branch."""
    nodes = [
        ('n-reply-2', 'n-user-2', [], gpt_message('n-reply-2', 'assistant', text('Edited branch reply about lanterns'), 1700000400.5)),
        ('root', None, ['n-sys'], None),
        ('n-sys', 'root', ['n-ctx'], gpt_message('n-sys', 'system', text('System prompt mentions lanterns'), None, hidden=True)),
        ('n-ctx', 'n-sys', ['n-user-1', 'n-user-2'], gpt_message('n-ctx', 'user', {
            'content_type': 'user_editable_context', 'user_profile': 'Profile mentions lanterns',
            'user_instructions': 'Instructions mention lanterns'}, None, hidden=True)),
        ('n-user-1', 'n-ctx', ['n-reply-1'], gpt_message('n-user-1', 'user', text('First lanterns question'), 1700000100.1)),
        ('n-reply-1', 'n-user-1', ['n-call'], gpt_message('n-reply-1', 'assistant', {
            'content_type': 'thoughts', 'thoughts': [{'summary': 'lanterns', 'content': 'secret lanterns reasoning',
                                                      'chunks': [], 'finished': True}]}, 1700000101)),
        ('n-call', 'n-reply-1', ['n-tool'], gpt_message('n-call', 'assistant', {
            'content_type': 'code', 'language': 'unknown', 'response_format_name': None,
            'text': 'search("lanterns")'}, 1700000102, recipient='web.run')),
        ('n-tool', 'n-call', ['n-voice'], gpt_message('n-tool', 'tool', {
            'content_type': 'tether_browsing_display', 'result': 'Found lanterns page', 'summary': '',
            'assets': None, 'tether_id': None}, 1700000103, author_name='web.run')),
        ('n-voice', 'n-tool', ['n-recap'], gpt_message('n-voice', 'user', {'content_type': 'multimodal_text', 'parts': [
            {'content_type': 'audio_transcription', 'text': 'Spoken lanterns words', 'direction': 'in'},
            {'content_type': 'image_asset_pointer', 'asset_pointer': 'file-service://lanterns', 'size_bytes': 1},
            '']}, 1700000104.9999995)),
        ('n-recap', 'n-voice', [], gpt_message('n-recap', 'assistant', {
            'content_type': 'reasoning_recap', 'content': 'Thought about lanterns'}, 1700000105)),
        ('n-user-2', 'n-ctx', ['n-reply-2'], gpt_message('n-user-2', 'user', text('Edited lanterns question', ''), 1700000300)),
    ]
    mapping = {key: {'id': key, 'message': message, 'parent': parent, 'children': children}
               for key, parent, children, message in nodes}
    return {'title': 'Lantern chat', 'create_time': 1700000000.123456789, 'update_time': 1700000400.5,
            'mapping': mapping, 'moderation_results': [], 'current_node': 'n-reply-2', 'plugin_ids': None,
            'conversation_id': G1, 'id': G1, 'default_model_slug': 'gpt-5'}


def claude_conversation():
    root = '00000000-0000-4000-8000-000000000000'
    def message(uuid, sender, content, created, parent=root, attachments=()):
        return {'uuid': uuid, 'text': 'not read: the content blocks are', 'content': content, 'sender': sender,
                'created_at': created, 'updated_at': created, 'attachments': list(attachments), 'files': [],
                'parent_message_uuid': parent}
    return {'uuid': C1, 'name': 'Harbor chat', 'summary': '', 'created_at': '2026-01-05T10:00:00.000000Z',
            'updated_at': '2026-01-05T10:05:00.000000Z', 'account': {'uuid': 'acct'},
            'chat_messages': [
                message('m1', 'human', [{'type': 'text', 'text': 'Tell me about harbors', 'citations': []}],
                        '2026-01-05T10:00:01.000000Z',
                        attachments=[{'file_name': 'notes.txt', 'file_size': 9, 'file_type': 'txt',
                                      'extracted_content': 'Attached harbors notes'}]),
                message('m2', 'assistant', [
                    {'type': 'thinking', 'thinking': 'private harbors reasoning', 'summaries': [], 'cut_off': False},
                    {'type': 'tool_use', 'id': 't1', 'name': 'web_search', 'input': {'query': 'harbors history'}},
                    {'type': 'tool_result', 'tool_use_id': 't1', 'name': 'web_search', 'is_error': False,
                     'content': [{'type': 'knowledge', 'title': 'x', 'url': 'https://example.invalid', 'text': 'Harbors result text'}]},
                    {'type': 'token_budget', 'remaining': None},
                    {'type': 'text', 'text': 'Harbors answer', 'citations': []}], '2026-01-05T10:00:02.000000Z', parent='m1'),
                message('m3', 'human', [{'type': 'voice_note', 'title': 'Voice', 'text': 'Spoken harbors note'}],
                        '2026-01-05T10:00:03.000000Z', parent='m2'),
            ]}


class ExportSourceTests(unittest.TestCase):
    setUp_base = base.RetrievalTests.setUp
    tearDown = base.RetrievalTests.tearDown
    save_config = base.RetrievalTests.save_config
    run_cli = base.RetrievalTests.run_cli

    def setUp(self):
        self.setUp_base()
        self.gpt = self.root / 'gpt-export'
        self.cla = self.root / 'claude-export'
        self.gpt.mkdir()
        self.cla.mkdir()
        self.sources = {'sources': [{'name': 'gpt', 'kind': 'chatgpt_export', 'path': str(self.gpt)},
                                    {'name': 'cla', 'kind': 'claude_export', 'path': str(self.cla)}]}
        self.save_config()
        self.gpt_file = self.gpt / 'conversations-000.json'
        self.gpt_file.write_text(json.dumps([chatgpt_conversation()], indent=2), encoding='utf-8')
        self.cla_file = self.cla / 'conversations.json'
        self.cla_file.write_text(json.dumps([claude_conversation()]), encoding='utf-8')

    def find(self, query, *extra):
        r = self.run_cli('find', query, '--json', '--limit', '100', *extra)
        return r, json.loads(r.stdout)

    def by_text(self, out):
        return {hit['text']: hit for hit in out['results']}

    def test_chatgpt_roles_reasoning_and_branches(self):
        r, out = self.find('lanterns', '--source', 'gpt')
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        hits = self.by_text(out)
        self.assertNotIn('secret lanterns reasoning', hits)
        self.assertNotIn('Thought about lanterns', hits)
        self.assertEqual(hits['First lanterns question']['provenance'], 'user_message')
        self.assertEqual(hits['Spoken lanterns words']['provenance'], 'user_message')
        self.assertEqual(hits['System prompt mentions lanterns']['provenance'], 'injected_context')
        self.assertEqual(hits['Profile mentions lanterns']['provenance'], 'injected_context')
        self.assertEqual(hits['Instructions mention lanterns']['role'], 'system')
        self.assertEqual(hits['search("lanterns")']['provenance'], 'tool_input')
        self.assertEqual(hits['search("lanterns")']['role'], 'assistant')
        self.assertEqual(hits['Found lanterns page']['provenance'], 'tool_output')
        self.assertEqual(hits['Edited branch reply about lanterns']['provenance'], 'assistant')
        self.assertIs(hits['Edited lanterns question']['current_branch'], True)
        self.assertIs(hits['First lanterns question']['current_branch'], False)
        self.assertIs(hits['Spoken lanterns words']['current_branch'], False)
        # Read depth-first from the root: system, context, first branch (user, thoughts, call, tool, voice, recap), edited branch.
        self.assertEqual([hits[t]['line'] for t in ('System prompt mentions lanterns', 'Profile mentions lanterns',
                          'First lanterns question', 'search("lanterns")', 'Found lanterns page', 'Spoken lanterns words',
                          'Edited lanterns question', 'Edited branch reply about lanterns')], [1, 2, 3, 5, 6, 7, 9, 10])
        self.assertEqual(hits['Spoken lanterns words']['block'], '["content","parts",0,"text"]')
        self.assertEqual(hits['Found lanterns page']['block'], '["content","result"]')
        self.assertIsNone(hits['First lanterns question']['byte_offset'])
        # Times are read from the number's shortest form and cut to microseconds, never rounded.
        self.assertEqual(hits['First lanterns question']['timestamp'], '2023-11-14T22:15:00.100000Z')
        self.assertEqual(hits['Spoken lanterns words']['timestamp'], '2023-11-14T22:15:04.999999Z')
        self.assertIsNone(hits['System prompt mentions lanterns']['timestamp'])
        first = hits['First lanterns question']
        self.assertEqual((first['id'], first['agent'], first['kind'], first['title']), (G1, 'chatgpt_export', 'conversation', 'Lantern chat'))
        message = chatgpt_conversation()['mapping']['n-user-1']['message']
        self.assertEqual(first['record_sha256'], hashlib.sha256(json.dumps(message).encode('utf-8')).hexdigest())
        self.assertEqual(first['record_id'], 'n-user-1')
        # Own words rank above the app's text.
        order = [hit['provenance'] for hit in out['results']]
        self.assertLess(order.index('user_message'), order.index('injected_context'))
        snapshot = [n for n in out['coverage']['notes'] if n['kind'] == 'export_snapshot']
        self.assertEqual(snapshot[0]['detail'], 'a snapshot as exported; newest conversation update 2023-11-14T22:20:00.500000Z')
        self.assertTrue(any(line.startswith('ChatGPT export:') for line in out['coverage']['excluded']))

    def test_chatgpt_conversation_span_and_identity(self):
        r = self.run_cli('resolve', G1, '--json')
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        segment = json.loads(r.stdout)['results'][0]['segments'][0]
        data = self.gpt_file.read_bytes()
        start, length = segment['export_offset'], segment['bytes']
        self.assertEqual(json.loads(data[start:start + length]), chatgpt_conversation())
        self.assertEqual(data[start:start + 1], b'{')
        self.assertEqual(segment['timestamp'], '2023-11-14T22:13:20.123456Z')
        self.assertEqual(segment['export_conversation'], 1)

    def test_claude_blocks_attachments_and_thinking(self):
        r, out = self.find('harbors', '--source', 'cla')
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        hits = self.by_text(out)
        self.assertNotIn('private harbors reasoning', hits)
        self.assertNotIn('not read: the content blocks are', hits)
        self.assertEqual(hits['Tell me about harbors']['provenance'], 'user_message')
        self.assertEqual(hits['Attached harbors notes']['provenance'], 'quoted_or_pasted')
        self.assertEqual(hits['Attached harbors notes']['role'], 'user')
        self.assertEqual(hits['Attached harbors notes']['block'], '["attachments",0,"extracted_content"]')
        self.assertEqual(hits['harbors history']['provenance'], 'tool_input')
        self.assertEqual(hits['harbors history']['block'], '[1,"input","query"]')
        self.assertEqual(hits['Harbors result text']['provenance'], 'tool_output')
        self.assertEqual(hits['Harbors result text']['block'], '2.content[0]')
        self.assertEqual(hits['Harbors answer']['block'], 4)
        self.assertEqual(hits['Spoken harbors note']['provenance'], 'user_message')
        self.assertEqual(hits['Harbors answer']['timestamp'], '2026-01-05T10:00:02.000000Z')
        self.assertEqual(hits['Harbors answer']['record_id'], 'm2')
        self.assertNotIn('current_branch', hits['Harbors answer'])
        self.assertEqual(out['coverage']['unsupported_records'], 0)
        self.assertTrue(any(line.startswith('Claude export:') for line in out['coverage']['excluded']))

    def test_read_back_by_line_and_digest(self):
        r, out = self.find('Harbors answer', '--source', 'cla')
        hit = out['results'][0]
        r = self.run_cli('read', C1, '--line', str(hit['line']), '--record-sha256', hit['record_sha256'], '--context', '1', '--json')
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        lines = [record['line'] for record in json.loads(r.stdout)['results']]
        self.assertEqual(sorted(set(lines)), [1, 2, 3])
        r = self.run_cli('read', C1, '--line', '2', '--record-sha256', '0' * 64, '--json')
        self.assertEqual(r.returncode, 3)
        self.assertEqual(json.loads(r.stdout)['status'], 'record_changed')

    def test_list_reports_latest_message_and_first_user_text(self):
        r = self.run_cli('list', '5', '--json')
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        rows = {row['id']: row for row in json.loads(r.stdout)['results']}
        self.assertEqual(rows[C1]['preview'], 'Tell me about harbors')
        self.assertEqual(rows[C1]['timestamp'], '2026-01-05T10:00:03.000000Z')
        self.assertEqual(rows[G1]['preview'], 'First lanterns question')
        self.assertEqual(rows[G1]['timestamp'], '2023-11-14T22:20:00.500000Z')
        self.assertEqual(rows[G1]['inspected_bytes'], rows[G1]['bytes'])

    def test_unknown_content_is_unsupported_not_skipped(self):
        conversation = chatgpt_conversation()
        conversation['mapping']['n-user-1']['message']['content'] = {'content_type': 'hologram', 'parts': ['lanterns']}
        conversation['mapping']['n-voice']['message']['author']['role'] = 'narrator'
        self.gpt_file.write_text(json.dumps([conversation]), encoding='utf-8')
        data = claude_conversation()
        data['chat_messages'][1]['content'].append({'type': 'hologram', 'text': 'harbors'})
        self.cla_file.write_text(json.dumps([data]), encoding='utf-8')
        r, out = self.find('lanterns')
        self.assertEqual(r.returncode, 4, r.stdout + r.stderr)
        details = sorted(e['detail'] for e in out['coverage']['errors'] if e['kind'] == 'unsupported_record')
        self.assertEqual(details, ['conversation 1 message 2: content type hologram',
                                   'conversation 1 message 3: content type hologram', 'conversation 1 message 7'])
        self.assertEqual(out['coverage']['unsupported_records'], 3)
        self.assertNotIn('Spoken lanterns words', self.by_text(out))

    def test_files_found_and_files_reported(self):
        (self.gpt / 'conversations.json').write_text(json.dumps([{**chatgpt_conversation(), 'conversation_id': G2, 'id': G2}]),
                                                     encoding='utf-8-sig')
        (self.gpt / 'user.json').write_text(json.dumps({'note': 'lanterns'}), encoding='utf-8')
        (self.gpt / 'nested').mkdir()
        (self.gpt / 'nested' / 'conversations-001.json').write_text('{"lanterns": true}', encoding='utf-8')
        with zipfile.ZipFile(self.cla / 'data-batch-0000.zip', 'w') as archive:
            archive.writestr('conversations.json', json.dumps([claude_conversation()]))
        r, out = self.find('First lanterns question')
        self.assertEqual(r.returncode, 4, r.stdout + r.stderr)
        self.assertEqual(sorted(hit['id'] for hit in out['results']), [G1, G2])
        errors = [(e['kind'], e['detail']) for e in out['coverage']['errors']]
        self.assertEqual(errors, [('export_unsupported', 'not a UTF-8 JSON array of conversations nested at most 256 levels deep')])
        notes = [n['kind'] for n in out['coverage']['notes']]
        self.assertIn('export_archive_not_searched', notes)
        self.assertEqual(notes.count('export_snapshot'), 3)
        sources = {s['name']: s['files'] for s in out['coverage']['sources']}
        self.assertEqual(sources, {'gpt': 3, 'cla': 1})

    def test_the_same_conversation_in_two_exports_is_a_copy(self):
        later = self.cla / 'later'
        later.mkdir()
        conversation = claude_conversation()
        conversation['chat_messages'].append({**conversation['chat_messages'][0], 'uuid': 'm4',
                                              'content': [{'type': 'text', 'text': 'A later harbors question'}],
                                              'attachments': []})
        (later / 'conversations.json').write_text(json.dumps([conversation]), encoding='utf-8')
        r, out = self.find('harbors', '--source', 'cla')
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        hits = self.by_text(out)
        self.assertEqual(len(out['results']), len(hits))
        self.assertIn('A later harbors question', hits)
        self.assertNotIn('copies', hits['A later harbors question'])
        copies = hits['Tell me about harbors']['copies']
        self.assertEqual(len(copies), 1)
        self.assertEqual(copies[0]['record_sha256'], hits['Tell me about harbors']['record_sha256'])
        self.assertNotEqual(copies[0]['path'], hits['Tell me about harbors']['path'])
        self.assertEqual(list(copies[0]), ['id', 'path', 'line', 'record_sha256', 'export_offset'])
        r = self.run_cli('resolve', C1, '--json')
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertEqual(len(json.loads(r.stdout)['results'][0]['segments']), 2)
        r = self.run_cli('path', C1, '--json')
        self.assertEqual((r.returncode, json.loads(r.stdout)['status']), (3, 'multiple_segments_use_resolve'))
        r = self.run_cli('read', C1, '--line', '4', '--json')
        self.assertEqual((r.returncode, json.loads(r.stdout)['status']), (3, 'multiple_segments_choose_segment'))
        r = self.run_cli('read', C1, '--line', '4', '--segment', str((later / 'conversations.json').resolve()), '--json')
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertEqual([record['text'] for record in json.loads(r.stdout)['results']], ['A later harbors question'])

    def test_list_pages_with_a_snapshot(self):
        (self.gpt / 'conversations-001.json').write_text(json.dumps([{**chatgpt_conversation(), 'conversation_id': G2, 'id': G2}]),
                                                         encoding='utf-8')
        r = self.run_cli('list', '1', '--json')
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        first = json.loads(r.stdout)
        self.assertTrue(first['next_cursor'])
        r = self.run_cli('list', '1', '--json', '--cursor', first['next_cursor'])
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        second = json.loads(r.stdout)
        self.assertEqual(second['offset'], 1)
        self.assertNotEqual(first['results'][0]['id'], second['results'][0]['id'])
        page3 = self.run_cli('list', '1', '--json', '--cursor', second['next_cursor'])
        self.assertEqual(page3.returncode, 0, page3.stdout + page3.stderr)
        self.cla_file.write_bytes(self.cla_file.read_bytes().replace(b'Tell me about harbors', b'Tell me about harborz'))
        r = self.run_cli('list', '1', '--json', '--cursor', first['next_cursor'])
        self.assertEqual((r.returncode, json.loads(r.stdout)['status']), (3, 'stale_cursor'))

    def test_unknown_parts_items_and_nodes_are_counted(self):
        conversation = chatgpt_conversation()
        conversation['mapping']['n-voice']['message']['content']['parts'].extend(
            [{'content_type': 'hologram', 'text': 'lost lanterns'}, 7, {'content_type': 'audio_asset_pointer'}])
        conversation['mapping']['n-stray'] = 42
        self.gpt_file.write_text(json.dumps([conversation]), encoding='utf-8')
        data = claude_conversation()
        data['chat_messages'][1]['content'][2]['content'].extend([{'type': 'image', 'source': 'x'}, {'type': 'local_resource'},
                                                                  {'type': 'hologram', 'text': 'lanterns in a hologram'}])
        data['chat_messages'][0]['attachments'].append('notes.txt')
        self.cla_file.write_text(json.dumps([data]), encoding='utf-8')
        r, out = self.find('lanterns')
        self.assertEqual(r.returncode, 4, r.stdout + r.stderr)
        details = sorted(e['detail'] for e in out['coverage']['errors'] if e['kind'] == 'unsupported_record')
        self.assertEqual(details, ['conversation 1 message 11', 'conversation 1 message 1: attachment',
                                   'conversation 1 message 2: tool result hologram', 'conversation 1 message 2: tool result image',
                                   'conversation 1 message 7: part (none)', 'conversation 1 message 7: part hologram'])
        self.assertIn('Spoken lanterns words', self.by_text(out))
        self.assertNotIn('lost lanterns', self.by_text(out))
        self.assertNotIn('lanterns in a hologram', self.by_text(out))

    def test_deep_nesting_and_lookalike_characters_are_refused(self):
        deep = chatgpt_conversation()
        deep['extra'] = json.loads('[' * 300 + ']' * 300)
        self.gpt_file.write_text(json.dumps([deep]), encoding='utf-8')
        shallow = claude_conversation()
        shallow['extra'] = json.loads('[' * 200 + ']' * 200)
        odd = {**claude_conversation(), 'uuid': '1c1a0000-0000-4000-8000-000000000002', 'name': 'odd \U0010f800 name'}
        self.cla_file.write_text(json.dumps([shallow, odd], ensure_ascii=False), encoding='utf-8')
        r, out = self.find('harbors')
        self.assertEqual(r.returncode, 4, r.stdout + r.stderr)
        errors = [(e['kind'], e['detail']) for e in out['coverage']['errors']]
        self.assertEqual(errors, [('export_unsupported', 'not a UTF-8 JSON array of conversations nested at most 256 levels deep'),
                                  ('unsupported_record', 'conversation 2: a character in U+10F800..U+10FFFF')])
        self.assertEqual({hit['id'] for hit in out['results']}, {C1})

    def test_duplicate_identity_in_one_file_is_reported(self):
        self.cla_file.write_text(json.dumps([claude_conversation(), {**claude_conversation(), 'name': 'again'}]), encoding='utf-8')
        r = self.run_cli('resolve', C1, '--json')
        self.assertEqual(r.returncode, 4, r.stdout + r.stderr)
        out = json.loads(r.stdout)
        self.assertEqual(len(out['results'][0]['segments']), 1)
        self.assertEqual([(e['kind'], e['detail']) for e in out['coverage']['errors']], [('identity_duplicate', 'conversation 2')])

    def test_identical_messages_in_one_conversation_are_not_copies(self):
        conversation = chatgpt_conversation()
        twin = {key: value for key, value in conversation['mapping']['n-user-1']['message'].items() if key != 'id'}
        conversation['mapping']['n-user-1']['message'] = twin
        conversation['mapping']['n-user-2']['message'] = dict(twin)
        self.gpt_file.write_text(json.dumps([conversation]), encoding='utf-8')
        (self.gpt / 'conversations-001.json').write_text(json.dumps([{**conversation, 'current_node': 'n-reply-1'}]), encoding='utf-8')
        r, out = self.find('First lanterns question', '--source', 'gpt')
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        hits = out['results']
        self.assertEqual(len(hits), 2)
        self.assertEqual([hit['line'] for hit in hits], [3, 9])
        self.assertEqual([hit['path'] for hit in hits], [str(self.gpt_file.resolve())] * 2)
        copies = [copy for hit in hits for copy in hit['copies']]
        self.assertEqual(sorted((c['line'], c['current_branch']) for c in copies), [(3, True), (9, False)])
        self.assertTrue(all(c['path'].endswith('conversations-001.json') and c['export_offset'] for c in copies))

    def test_an_import_copy_is_folded_before_export_copies(self):
        self.claude.joinpath('project').mkdir(parents=True)
        session = '33333333-3333-4333-8333-333333333333'
        (self.claude / 'project' / (session + '.jsonl')).write_text(json.dumps(
            {'type': 'user', 'uuid': 'u1', 'sessionId': session, 'timestamp': '2026-01-05T09:00:00Z',
             'message': {'role': 'user', 'content': 'Tell me about harbors'}}) + '\n', encoding='utf-8')
        imports = self.root / 'imports.json'
        imports.write_text(json.dumps({'records': [{'imported_thread_id': C1,
                                                    'source_path': 'C:/x/' + session + '.jsonl'}]}), encoding='utf-8')
        self.sources = {'sources': [{'name': 'claude', 'kind': 'claude', 'path': str(self.claude)},
                                    {'name': 'cla', 'kind': 'claude_export', 'path': str(self.cla)}],
                        'imports': str(imports)}
        self.save_config()
        r, out = self.find('Tell me about harbors')
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertEqual(len(out['results']), 1)
        hit = out['results'][0]
        self.assertEqual((hit['id'], hit['agent']), (session, 'claude'))
        self.assertEqual([(c['id'], c['line']) for c in hit['copies']], [(C1, 1)])

    def test_listing_does_not_count_and_continuation_does_not_double_count(self):
        data = claude_conversation()
        data['chat_messages'][1]['content'].append({'type': 'hologram'})
        self.cla_file.write_text(json.dumps([data]), encoding='utf-8')
        (self.gpt / 'conversations-001.json').write_text(json.dumps([{**chatgpt_conversation(), 'conversation_id': G2, 'id': G2}]),
                                                         encoding='utf-8')
        r = self.run_cli('list', '1', '--json')
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        first = json.loads(r.stdout)
        self.assertEqual(first['coverage']['unsupported_records'], 0)
        r = self.run_cli('list', '1', '--json', '--cursor', first['next_cursor'])
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertEqual(json.loads(r.stdout)['coverage']['unsupported_records'], 0)
        r, out = self.find('harbors', '--source', 'cla')
        self.assertEqual((r.returncode, out['coverage']['unsupported_records']), (4, 1))

    def test_escaped_lookalikes_null_nodes_and_repeated_ids_are_counted(self):
        conversation = chatgpt_conversation()
        conversation['mapping']['n-null'] = None
        conversation['mapping']['n-ctx']['children'].append('n-null')
        twin = dict(conversation['mapping']['n-user-1']['message'])
        conversation['mapping']['n-twin'] = {'id': 'n-twin', 'message': twin, 'parent': 'n-ctx', 'children': []}
        conversation['mapping']['n-ctx']['children'].append('n-twin')
        self.gpt_file.write_text(json.dumps([conversation]), encoding='utf-8')
        escaped = {**claude_conversation(), 'uuid': '1c1a0000-0000-4000-8000-000000000003'}
        escaped['chat_messages'][0]['extra'] = '\U0010f800'
        self.cla_file.write_text(json.dumps([claude_conversation(), escaped]), encoding='utf-8')
        self.assertIn(b'\\udbfe\\udc00', self.cla_file.read_bytes())
        r, out = self.find('lanterns')
        self.assertEqual(r.returncode, 4, r.stdout + r.stderr)
        details = sorted(e['detail'] for e in out['coverage']['errors'] if e['kind'] == 'unsupported_record')
        self.assertEqual(details, ['conversation 1 message 11', 'conversation 1 message 12: duplicate message id',
                                   'conversation 2: a character in U+10F800..U+10FFFF'])
        self.assertEqual(out['coverage']['unsupported_records'], 3)

    def test_a_title_for_an_export_conversation_is_a_title_hit(self):
        index = self.root / 'session_index.jsonl'
        index.write_text(json.dumps({'id': G1, 'thread_name': 'Lantern title needle'}) + '\n', encoding='utf-8')
        self.sources['title_index'] = str(index)
        self.save_config()
        r, out = self.find('title needle')
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertEqual([(h['provenance'], h['id']) for h in out['results']], [('title_metadata', G1)])

    def test_paging_snapshot_refuses_a_changed_conversation(self):
        r, out = self.find('lanterns', '--source', 'gpt', '--limit', '2')
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        cursor = out['next_cursor']
        self.assertTrue(cursor)
        r = self.run_cli('find', 'lanterns', '--source', 'gpt', '--limit', '2', '--json', '--cursor', cursor)
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        page = json.loads(r.stdout)
        self.assertEqual(page['offset'], 2)
        # A reopened page shows exactly what a fresh search shows at that place.
        fresh = self.find('lanterns', '--source', 'gpt', '--limit', '4')[1]['results'][2:4]
        self.assertEqual(page['results'], fresh)
        data = self.gpt_file.read_bytes().replace(b'First lanterns question', b'First lanterns questioN')
        self.gpt_file.write_bytes(data)
        r = self.run_cli('find', 'lanterns', '--source', 'gpt', '--limit', '2', '--json', '--cursor', cursor)
        self.assertEqual(r.returncode, 3, r.stdout + r.stderr)
        self.assertEqual(json.loads(r.stdout)['status'], 'stale_cursor')


if __name__ == '__main__':
    unittest.main()
