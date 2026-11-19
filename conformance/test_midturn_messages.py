"""A message sent while Claude Code is in the middle of a turn is not stored as a `user` record.
It is stored as an `attachment` record whose attachment has type `queued_command`, with the text
in `prompt` and the sender in `origin.kind`. Every identity and text here is synthetic.

The same tests drive the Python core, or the binary named by XSESSION_BINARY."""
import json
import unittest
import test_xsession as fixtures

A, B = fixtures.A, fixtures.B
WHEN = '2026-09-20T00:02:00Z'
ABSENT = object()


def user(sid, text, uuid='turn-start'):
    return {'type': 'user', 'uuid': uuid, 'sessionId': sid, 'timestamp': '2026-09-20T00:00:00Z',
            'message': {'role': 'user', 'content': text}}


def assistant(sid, text, uuid='reply'):
    return {'type': 'assistant', 'uuid': uuid, 'sessionId': sid, 'timestamp': '2026-09-20T00:01:00Z',
            'message': {'role': 'assistant', 'content': [{'type': 'text', 'text': text}]}}


def queued(sid, uuid, prompt, origin='human', mode='prompt'):
    """One mid-turn record. `origin` is the sender kind, or ABSENT for the older shape without it."""
    attachment = {'type': 'queued_command', 'prompt': prompt, 'timestamp': WHEN}
    if mode is not None:
        attachment['commandMode'] = mode
    if origin is not ABSENT:
        attachment['origin'] = {'kind': origin}
    if origin == 'human':
        attachment['humanTurn'] = True
    return {'type': 'attachment', 'uuid': uuid, 'sessionId': sid, 'timestamp': WHEN, 'attachment': attachment}


def raw_attachment(sid, uuid, attachment=ABSENT):
    record = {'type': 'attachment', 'uuid': uuid, 'sessionId': sid, 'timestamp': WHEN}
    if attachment is not ABSENT:
        record['attachment'] = attachment
    return record


def queue_operation(sid, operation, content=None, reason=None):
    record = {'type': 'queue-operation', 'operation': operation, 'sessionId': sid, 'timestamp': WHEN}
    if content is not None:
        record['content'] = content
    if reason is not None:
        record['reason'] = reason
    return record


class MidTurnMessageTests(unittest.TestCase):
    setUp = fixtures.RetrievalTests.setUp
    tearDown = fixtures.RetrievalTests.tearDown
    save_config = fixtures.RetrievalTests.save_config
    write = fixtures.RetrievalTests.write
    run_cli = fixtures.RetrievalTests.run_cli

    def session(self, sid, records):
        return self.write(self.claude, 'project/' + sid + '.jsonl', records)

    def find(self, query, *options):
        r = self.run_cli('find', query, '--json', '--limit', '50', *options)
        self.assertIn(r.returncode, (0, 1), r.stdout + r.stderr)
        return r, json.loads(r.stdout)

    def mid_turn_session(self):
        """A turn that starts, gets a message while it is working, and absorbs it."""
        return self.session(A, [
            user(A, 'opening task'),
            assistant(A, 'working on it'),
            queue_operation(A, 'enqueue', 'needle sent mid-turn'),
            queued(A, 'mid-1', 'needle sent mid-turn'),
            queue_operation(A, 'remove', 'needle sent mid-turn', 'absorbed_mid_turn')])

    def test_human_mid_turn_message_is_found_once_as_a_user_message(self):
        self.mid_turn_session()
        r, out = self.find('needle sent mid-turn')
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        # The queue records repeat the text; only the delivered message is a match.
        self.assertEqual((out['total_matches'], len(out['results'])), (1, 1), out['results'])
        hit = out['results'][0]
        self.assertEqual((hit['id'], hit['line'], hit['block']), (A, 4, 0))
        self.assertEqual((hit['role'], hit['provenance'], hit['content_provenance'], hit['confidence']),
                         ('user', 'user_message', 'user_message', 'role_only'))
        self.assertEqual((hit['record_id'], hit['timestamp'], hit['record_session_id']), ('mid-1', WHEN, A))
        self.assertEqual(hit['text'], 'needle sent mid-turn')
        self.assertTrue(out['coverage']['complete'])
        self.assertEqual(out['coverage']['unsupported_records'], 0)

    def test_read_reopens_the_mid_turn_record_by_line_and_hash(self):
        self.mid_turn_session()
        hit = self.find('needle sent mid-turn')[1]['results'][0]
        opened = self.run_cli('read', A, '--line', str(hit['line']), '--record-sha256', hit['record_sha256'],
                              '--context', '1', '--json')
        self.assertEqual(opened.returncode, 0, opened.stdout + opened.stderr)
        # Neighbouring text records only: the queue records around it hold no searched text.
        self.assertEqual([x['text'] for x in json.loads(opened.stdout)['results']],
                         ['working on it', 'needle sent mid-turn'])
        changed = self.run_cli('read', A, '--line', str(hit['line']), '--record-sha256', '0' * 64, '--json')
        self.assertEqual(changed.returncode, 3, changed.stdout + changed.stderr)

    def test_search_context_includes_the_mid_turn_message(self):
        self.mid_turn_session()
        hit = self.find('working on it', '--context', '1')[1]['results'][0]
        self.assertEqual([x['text'] for x in hit['context']],
                         ['opening task', 'working on it', 'needle sent mid-turn'])

    def test_inherited_copy_in_a_child_file_is_resumed_history(self):
        # A resumed or forked file carries its parent's records, mid-turn ones included.
        self.session(B, [
            user(A, 'inherited opening'),
            queued(A, 'mid-parent', 'needle inherited mid-turn'),
            assistant(B, 'own message')])
        hit = self.find('needle inherited mid-turn')[1]['results'][0]
        self.assertEqual((hit['id'], hit['line'], hit['record_session_id']), (B, 2, A))
        self.assertEqual((hit['provenance'], hit['content_provenance'], hit['confidence']),
                         ('resumed_history', 'user_message', 'record_session_id'))

    def test_only_a_human_origin_has_the_user_role(self):
        self.session(A, [
            user(A, 'opening task'),
            queued(A, 'n1', '<task-notification>needle task finished</task-notification>',
                   'task-notification', 'task-notification'),
            queued(A, 'n2', '<task-notification>needle older task shape</task-notification>',
                   ABSENT, 'task-notification'),
            queued(A, 'n3', '<ci-monitor-event>needle ci event</ci-monitor-event>', ABSENT, 'prompt'),
            queued(A, 'n4', 'needle from another session', 'peer', 'prompt'),
            queued(A, 'n5', 'needle from a coordinator', 'coordinator', None),
            queued(A, 'n6', 'needle from a kind not yet invented', 'future-kind', 'prompt'),
            queued(A, 'h1', 'needle typed by the operator')])
        r, out = self.find('needle')
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        by_id = {hit['record_id']: hit for hit in out['results']}
        self.assertEqual(sorted(by_id), ['h1', 'n1', 'n2', 'n3', 'n4', 'n5', 'n6'])
        for record_id in ('n1', 'n2', 'n3', 'n4', 'n5', 'n6'):
            hit = by_id[record_id]
            self.assertEqual((hit['role'], hit['provenance'], hit['confidence']),
                             ('system', 'injected_context', 'record_type'), record_id)
        self.assertEqual((by_id['h1']['role'], by_id['h1']['provenance']), ('user', 'user_message'))
        # The operator's own message ranks first.
        self.assertEqual(out['results'][0]['record_id'], 'h1')

    def test_text_blocks_of_a_list_prompt_are_searched(self):
        self.session(A, [
            user(A, 'opening task'),
            queued(A, 'mid-list', [
                {'type': 'text', 'text': 'needle before the picture'},
                {'type': 'image', 'source': {'type': 'base64', 'media_type': 'image/png', 'data': 'AAAA'}},
                {'type': 'text', 'text': 'needle after the picture'}])])
        r, out = self.find('needle')
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertEqual([(hit['line'], hit['block'], hit['role'], hit['text']) for hit in out['results']],
                         [(2, 0, 'user', 'needle before the picture'), (2, 2, 'user', 'needle after the picture')])

    def test_malformed_mid_turn_records_never_break_coverage(self):
        self.session(A, [
            user(A, 'opening task'),
            raw_attachment(A, 'm01'),
            raw_attachment(A, 'm02', None),
            raw_attachment(A, 'm03', 'needle attachment is a string'),
            raw_attachment(A, 'm04', ['needle attachment is a list']),
            raw_attachment(A, 'm05', {'type': 'queued_command'}),
            raw_attachment(A, 'm06', {'type': 'queued_command', 'prompt': None}),
            raw_attachment(A, 'm07', {'type': 'queued_command', 'prompt': 7}),
            raw_attachment(A, 'm08', {'type': 'queued_command', 'prompt': {'text': 'needle prompt is an object'}}),
            raw_attachment(A, 'm09', {'type': 'queued_command', 'origin': {'kind': 'human'}, 'prompt': [
                'needle bare string in a list', None, 5, {'type': 'text'}, {'type': 'text', 'text': 5},
                {'text': 'needle block without a type'}]}),
            raw_attachment(A, 'm10', {'type': ['queued_command'], 'prompt': 'needle type is a list'}),
            raw_attachment(A, 'm11', {'type': 'file', 'filename': 'needle.txt', 'content': 'needle other kind'}),
            raw_attachment(A, 'k1', {'type': 'queued_command', 'prompt': 'needle origin is a string', 'origin': 'human'}),
            raw_attachment(A, 'k2', {'type': 'queued_command', 'prompt': 'needle origin is null', 'origin': None}),
            raw_attachment(A, 'k3', {'type': 'queued_command', 'prompt': 'needle kind is a list',
                                     'origin': {'kind': ['human']}}),
            raw_attachment(A, 'k4', {'type': 'queued_command', 'prompt': 'needle origin is a list', 'origin': ['human']})])
        r, out = self.find('needle')
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        # A prompt that is text is still searched; a sender that is not plainly human is not the user.
        self.assertEqual(sorted((hit['record_id'], hit['role'], hit['provenance']) for hit in out['results']),
                         [(record_id, 'system', 'injected_context') for record_id in ('k1', 'k2', 'k3', 'k4')])
        coverage = out['coverage']
        self.assertTrue(coverage['complete'])
        self.assertEqual((coverage['malformed_records'], coverage['unsupported_records'], coverage['errors']),
                         (0, 0, []))

    def test_queue_bookkeeping_is_not_searched_and_that_is_disclosed(self):
        # A queued text that never became a message exists only in queue-operation records.
        self.session(A, [
            user(A, 'opening task'),
            queue_operation(A, 'enqueue', 'needle typed and withdrawn'),
            queue_operation(A, 'remove', 'needle typed and withdrawn')])
        r, out = self.find('needle typed and withdrawn')
        self.assertEqual((r.returncode, out['status'], out['results']), (1, 'no_match', []), r.stderr)
        excluded = out['coverage']['excluded']
        self.assertEqual(sum('queue-operation' in line for line in excluded), 1, excluded)
        self.assertTrue(out['coverage']['complete'])


if __name__ == '__main__':
    unittest.main()
