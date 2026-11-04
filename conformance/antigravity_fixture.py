"""Antigravity brain-folder fixtures for the conformance suite and the differential (2.6.0 / 3.3.0).

build(root) writes stores shaped like the real one (layout and record keys verified read-only on
2026-10-29): <conversation uuid>/.system_generated/logs/transcript.jsonl, with transcript_full.jsonl
and chunks/ copies beside it. Steps carry type, source, status, step_index, created_at (UTC) and
content; planner steps add thinking, tool_calls (each argument stored as a JSON literal) and token
counts. THINKING appears only in `thinking` fields and in transcript_full.jsonl; nothing may return it.

Usage as a script: antigravity_fixture.py <folder>"""
import json
import os
from pathlib import Path
import subprocess
import sys

A = 'aaaaaaaa-1111-4111-8111-111111111111'  # operator conversation; creates B and C
B = 'bbbbbbbb-1111-4111-8111-111111111111'  # subagent from an INVOKE_SUBAGENT step; opens with USER_INPUT
C = 'cccccccc-1111-4111-8111-111111111111'  # subagent from a GENERIC step (singular form); opens with SYSTEM_MESSAGE
D = 'dddddddd-1111-4111-8111-111111111111'  # named only in quoted creation text: stays a conversation
E = 'eeeeeeee-1111-4111-8111-111111111111'  # empty transcript
F = 'ffffffff-1111-4111-8111-111111111111'  # artifacts only, no logs folder
P = '99999999-1111-4111-8111-111111111111'  # problems: a malformed record and an unfinished tail
Q = '98888888-1111-4111-8111-111111111111'  # problems: transcript.jsonl.gz only
R = '97777777-1111-4111-8111-111111111111'  # problems: transcript.jsonl.zst (read like the plain file)
S = '96666666-1111-4111-8111-111111111111'  # problems: the conversation folder is a junction (Windows)
CLAUDE = '12121212-1111-4111-8111-111111111111'  # a Claude session where the operator pastes A's draft
THINKING = 'zebra-quartz'
HEADER = 'Created At: 2026-10-29T03:00:{0:02d}-07:00\nCompleted At: 2026-10-29T03:00:{0:02d}-07:00\n'
METADATA = '\n<ADDITIONAL_METADATA>\nThe current local time is: needle metadata time.\n</ADDITIONAL_METADATA>'
SETTINGS = '\n<USER_SETTINGS_CHANGE>\nneedle settings change\n</USER_SETTINGS_CHANGE>'


def step(index, kind, source, content=None, status='DONE', **extra):
    record = {'step_index': index, 'source': source, 'type': kind, 'status': status,
              'created_at': '2026-10-29T10:%02d:%02dZ' % (index // 60, index % 60)}
    record.update(extra)
    if content is not None:
        record['content'] = content
    return record


def literal(value):
    """An argument as transcript.jsonl stores it: a JSON literal inside a string."""
    return json.dumps(value)


def lines(records):
    return b''.join((json.dumps(r) + '\n').encode() for r in records)


def put(path, data):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(data)
    return path


def conversation_a():
    """(compact records, full records) for A; the full copy differs only where compact truncates."""
    compact = [
        step(0, 'USER_INPUT', 'USER_EXPLICIT', '<USER_REQUEST>\nneedle operator request alpha\n</USER_REQUEST>' + METADATA + SETTINGS),
        step(1, 'PLANNER_RESPONSE', 'MODEL', 'needle assistant reply: draft ruling block for the operator',
             thinking='needle private plan ' + THINKING, input_tokens=10, cache_read_tokens=0, output_tokens=5,
             tool_calls=[{'name': 'run_command', 'args': {
                 'CommandLine': literal('python N:\\tools\\needle_tool.py --flag'), 'Cwd': literal('N:\\'),
                 'WaitMsBeforeAsync': literal(5000), 'IsDaemon': literal(False), 'toolAction': literal('needle running the tool'),
                 'Edits': literal([{'Target': 'needle edit target', 'Replacement': 'x'}])}}]),
        step(2, 'GENERIC', 'MODEL', HEADER.format(2) + 'needle tool output: READ OK'),
        step(3, 'SYSTEM_MESSAGE', 'SYSTEM', 'The following is a <SYSTEM_MESSAGE> not actually sent by the user.\n\n'
             '<SYSTEM_MESSAGE>\nneedle system message\n</SYSTEM_MESSAGE>'),
        step(4, 'EPHEMERAL_MESSAGE', 'SYSTEM', 'The following is an <EPHEMERAL_MESSAGE> not actually sent by the user.\nneedle ephemeral'),
        step(5, 'CONVERSATION_HISTORY', 'SYSTEM', '# Conversation History\nneedle history of other conversations'),
        step(6, 'CHECKPOINT', 'SYSTEM', '# Resuming from a compaction\n\nneedle checkpoint summary'),
        step(7, 'ERROR_MESSAGE', 'SYSTEM', error='needle error only'),
        step(8, 'USER_INPUT', 'USER_EXPLICIT', 'needle untagged input'),
        step(9, 'USER_INPUT', 'USER_IMPLICIT', '<USER_REQUEST>needle implicit request</USER_REQUEST>'),
        step(10, 'USER_INPUT', 'USER_EXPLICIT', 'lead <USER_REQUEST>\nneedle unclosed request'),
        step(11, 'PLANNER_RESPONSE', 'MODEL', None, thinking=THINKING + ' only here', truncated_fields=['tool_calls'],
             tool_calls=[{'name': 'write_file', 'args': {'Content': '"needle truncated arg head\n<truncated 397 bytes>\ntail"'}}]),
        step(12, 'PLANNER_RESPONSE', 'MODEL', 'needle head of a long answer\n<truncated 100 bytes>\nend of answer',
             thinking='needle ' + THINKING, truncated_fields=['content', 'thinking']),
        step(13, 'FUTURE_STEP', 'MODEL', 'needle future step'),
        step(14, 'USER_INPUT', 'USER_EXPLICIT', '<USER_REQUEST>\n> needle quoted paste\n</USER_REQUEST>' + METADATA),
        step(15, 'INVOKE_SUBAGENT', 'MODEL', HEADER.format(15) + 'Created the following subagents:\n{\n  "conversationId": "%s",\n'
             '  "logAbsoluteUri": "file:///C:/brain/%s/.system_generated/logs/transcript.jsonl"\n}\nThe subagents will send you a message.' % (B, B)),
        step(16, 'GENERIC', 'MODEL', HEADER.format(16) + 'Created the following subagent:\n{\n  "conversationId":  "%s"\n}' % C),
        step(17, 'VIEW_FILE', 'MODEL', HEADER.format(17) + 'File Path: `file:///N:/notes.md`\nCreated the following subagents:\n'
             '{"conversationId": "%s"}' % D),
        step(18, 'PLANNER_RESPONSE', 'MODEL', 'Created the following subagents:\n{"conversationId": "%s"}' % D),
        step(19, 'USER_INPUT', 'USER_EXPLICIT', 'Created At: x\nCreated the following subagents:\n{"conversationId": "%s"}' % D),
        step(20, 'ASK_QUESTION', 'MODEL', HEADER.format(20) + 'A1: (Recommended) needle answer chosen'),
        step(21, 'PLANNER_RESPONSE', 'MODEL', '', tool_calls=[{'name': 'view_file', 'args': {'AbsolutePath': literal('N:\\needle.md'),
                                                            'StartLine': literal(1), 'Nested': {'Native': 'needle native argument'}}}]),
    ]
    full = [dict(r) for r in compact]
    full[11] = dict(full[11], tool_calls=[{'name': 'write_file', 'args': {'Content': 'needle truncated arg head ' + THINKING + ' tail'}}])
    full[12] = dict(full[12], content='needle head of a long answer needle full only middle end of answer')
    for record in full:
        record.pop('truncated_fields', None)
    return compact, full


def build(root):
    """Write the fixture stores under `root` and their source configurations."""
    root = Path(root)
    brain = root / 'brain'
    compact, full = conversation_a()
    logs = brain / A / '.system_generated' / 'logs'
    data = lines(compact)
    put(logs / 'transcript.jsonl', data)
    put(logs / 'transcript_full.jsonl', lines(full))
    middle = len(data) // 2
    put(logs / 'chunks' / 'transcript' / '00000000.jsonl', data[:middle])
    put(logs / 'chunks' / 'transcript' / '00000001.jsonl', data[middle:])
    put(brain / A / '.system_generated' / 'steps' / '2' / 'output.txt', b'needle step output file\n')
    put(brain / A / 'implementation_plan.md', b'needle artifact\n')
    put(brain / B / '.system_generated' / 'logs' / 'transcript.jsonl', lines([
        step(0, 'USER_INPUT', 'USER_EXPLICIT', '<USER_REQUEST>\nneedle delegated research task\n</USER_REQUEST>' + METADATA),
        step(1, 'PLANNER_RESPONSE', 'MODEL', 'needle subagent reply', thinking=THINKING)]))
    put(brain / C / '.system_generated' / 'logs' / 'transcript.jsonl', lines([
        step(0, 'SYSTEM_MESSAGE', 'SYSTEM', '<SYSTEM_MESSAGE>\n[Message] sender=%s needle child task\n</SYSTEM_MESSAGE>' % A),
        step(1, 'USER_INPUT', 'USER_EXPLICIT', '<USER_REQUEST>needle message to the child</USER_REQUEST>')]))
    put(brain / D / '.system_generated' / 'logs' / 'transcript.jsonl', lines([
        step(0, 'USER_INPUT', 'USER_EXPLICIT', '<USER_REQUEST>needle top level request delta</USER_REQUEST>' + METADATA + SETTINGS)]))
    put(brain / E / '.system_generated' / 'logs' / 'transcript.jsonl', b'')
    put(brain / F / 'walkthrough.md', b'needle artifact without logs\n')
    put(brain / 'tempmediaStorage' / '.system_generated' / 'logs' / 'transcript.jsonl',
        lines([step(0, 'USER_INPUT', 'USER_EXPLICIT', '<USER_REQUEST>needle in a folder not named by a uuid</USER_REQUEST>')]))
    put(brain / 'notes.txt', b'needle loose file\n')

    claude = root / 'claude' / 'N--'
    put(claude / (CLAUDE + '.jsonl'), lines([
        {'type': 'user', 'sessionId': CLAUDE, 'uuid': 'u1', 'timestamp': '2026-10-29T10:30:00Z',
         'message': {'role': 'user', 'content': 'needle assistant reply: draft ruling block for the operator'}},
        {'type': 'assistant', 'sessionId': CLAUDE, 'uuid': 'u2', 'timestamp': '2026-10-29T10:30:05Z',
         'message': {'role': 'assistant', 'content': [{'type': 'text', 'text': 'Recorded the needle ruling.'}]}}]))

    problems = root / 'brain-problems'
    put(problems / P / '.system_generated' / 'logs' / 'transcript.jsonl',
        lines([step(0, 'USER_INPUT', 'USER_EXPLICIT', '<USER_REQUEST>needle before the damage</USER_REQUEST>')])
        + b'{"step_index": 1, "type": "GENERIC", "content": "needle broken\n'
        + lines([step(2, 'GENERIC', 'MODEL', 'needle after the damage')])
        + json.dumps(step(3, 'GENERIC', 'MODEL', 'needle unfinished tail')).encode())
    put(problems / Q / '.system_generated' / 'logs' / 'transcript.jsonl.gz', b'\x1f\x8b needle not searched')
    packed = lines([step(0, 'USER_INPUT', 'USER_EXPLICIT', '<USER_REQUEST>needle compressed request</USER_REQUEST>' + METADATA),
                    step(1, 'PLANNER_RESPONSE', 'MODEL', 'needle compressed reply', thinking=THINKING)])
    try:
        from compression import zstd
        put(problems / R / '.system_generated' / 'logs' / 'transcript.jsonl.zst', zstd.compress(packed))
    except ImportError:  # Python before 3.14: the plain twin keeps the case meaningful
        put(problems / R / '.system_generated' / 'logs' / 'transcript.jsonl', packed)
    if os.name == 'nt':
        target = root / 'junction-target' / S
        put(target / '.system_generated' / 'logs' / 'transcript.jsonl',
            lines([step(0, 'USER_INPUT', 'USER_EXPLICIT', '<USER_REQUEST>needle behind a junction</USER_REQUEST>')]))
        subprocess.run(['cmd', '/c', 'mklink', '/J', str(problems / S), str(target)], check=True, capture_output=True)

    def config(name, sources):
        (root / name).write_text(json.dumps({'sources': sources}), encoding='utf-8')
    config('sources.json', [{'name': 'antigravity', 'kind': 'antigravity', 'path': str(brain)},
                            {'name': 'claude', 'kind': 'claude', 'path': str(root / 'claude')}])
    config('sources-problems.json', [{'name': 'problems', 'kind': 'antigravity', 'path': str(problems)}])
    config('sources-missing.json', [{'name': 'missing', 'kind': 'antigravity', 'path': str(root / 'brain-absent')}])
    return root


if __name__ == '__main__':
    target = Path(sys.argv[1])
    if target.exists():
        raise SystemExit('refusing to write into an existing folder: %s' % target)
    build(target)
