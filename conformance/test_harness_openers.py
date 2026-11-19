"""Text Claude Code writes into a user-role record itself (a task notification, a local command's output,
a message from another session) is not the operator's message. Such text opens with one of a few fixed
tags; it is labelled injected_context. Text the operator did write, including a paste, a slash command
or a reply quote, keeps its label. Every identity and text here is synthetic.

The same tests drive the Python core, or the binary named by XSESSION_BINARY."""
import json
import unittest
import test_xsession as fixtures

A = fixtures.A
MACHINE_OPENERS = ['<task-notification>', '<local-command-caveat>', '<local-command-stdout>',
                   '<cross-session-message>', '<ci-monitor-event>', '<scheduled-task>',
                   '<fork-boilerplate>', '<artifact-view-context>']


def user(uuid, text, content_as_blocks=False):
    content = [{'type': 'text', 'text': text}] if content_as_blocks else text
    return {'type': 'user', 'uuid': uuid, 'sessionId': A, 'timestamp': '2026-09-20T00:00:00Z',
            'message': {'role': 'user', 'content': content}}


class HarnessOpenerTests(unittest.TestCase):
    setUp = fixtures.RetrievalTests.setUp
    tearDown = fixtures.RetrievalTests.tearDown
    save_config = fixtures.RetrievalTests.save_config
    write = fixtures.RetrievalTests.write
    run_cli = fixtures.RetrievalTests.run_cli

    def find(self, query):
        r = self.run_cli('find', query, '--json', '--limit', '50')
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        return {hit['record_id']: hit for hit in json.loads(r.stdout)['results']}

    def test_machine_openers_are_injected_context(self):
        records = [user('m%d' % i, '%s needle machine %d %s' % (tag, i, tag.replace('<', '</')), i % 2 == 1)
                   for i, tag in enumerate(MACHINE_OPENERS)]
        records.append(user('lead', '\n  <task-notification>needle after leading whitespace</task-notification>'))
        records.append(user('case', '<TASK-NOTIFICATION>needle upper case</TASK-NOTIFICATION>'))
        self.write(self.claude, 'project/' + A + '.jsonl', records)
        hits = self.find('needle')
        for record_id in ['m%d' % i for i in range(len(MACHINE_OPENERS))] + ['lead', 'case']:
            hit = hits[record_id]
            self.assertEqual((hit['role'], hit['provenance'], hit['content_provenance'], hit['confidence']),
                             ('user', 'injected_context', 'injected_context', 'wrapper_heuristic'), record_id)

    def test_operator_text_keeps_its_label(self):
        self.write(self.claude, 'project/' + A + '.jsonl', [
            user('typed', 'needle typed by the operator'),
            user('mentions', 'needle: why does <task-notification> show up as mine?'),
            user('paste', '<pasted_content id="p1">needle pasted by the operator</pasted_content id="p1">'),
            user('slash', '<command-name>/model</command-name> needle slash command'),
            user('quote', '<!-- reply 1 -->\n> quoted line\n\nneedle reply under a quote')])
        hits = self.find('needle')
        for record_id in ('typed', 'mentions', 'paste', 'slash', 'quote'):
            self.assertEqual((hits[record_id]['provenance'], hits[record_id]['confidence']),
                             ('user_message', 'role_only'), record_id)

    def test_the_operator_ranks_above_a_notification(self):
        self.write(self.claude, 'project/' + A + '.jsonl', [
            user('note', '<task-notification>needle background task finished</task-notification>'),
            user('typed', 'needle typed by the operator')])
        r = self.run_cli('find', 'needle', '--json')
        self.assertEqual([hit['record_id'] for hit in json.loads(r.stdout)['results']], ['typed', 'note'])


if __name__ == '__main__':
    unittest.main()
