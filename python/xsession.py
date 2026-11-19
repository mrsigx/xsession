#!/usr/bin/env python3
"""Read-only, on-demand transcript retrieval. Standard library only."""
import argparse
import base64
from collections import deque
from concurrent.futures import ProcessPoolExecutor, as_completed
from concurrent.futures.process import BrokenProcessPool
from datetime import datetime, timedelta
import hashlib
import io
import json
import os
from pathlib import Path
import re
import sys
import time
import uuid
from urllib.parse import urlparse, parse_qs
import xml.etree.ElementTree as ET
import zipfile
import zlib
try:
    from compression import zstd as _zstd  # Python 3.14+ standard library
except ImportError:  # older runtimes report .jsonl.zst transcripts as partial coverage
    _zstd = None

VERSION = '2.9.0'
SNAPSHOT_SLOTS = 16
SNAPSHOT_MAX_BYTES = 4 * 1024 * 1024
PARALLEL_MIN_BYTES = 256 * 1024 * 1024
PROJECT_TEXT_SUFFIXES = ('.md', '.markdown', '.txt', '.json', '.jsonl', '.csv', '.tsv', '.jsx', '.js', '.ts', '.tsx',
                         '.py', '.rs', '.html', '.htm', '.css', '.xml', '.yaml', '.yml', '.toml', '.ps1', '.sh', '.sql')
PACKAGE_PART_LIMIT = 64 * 1024 * 1024
WORD = '{http://schemas.openxmlformats.org/wordprocessingml/2006/main}'
SHEET = '{http://schemas.openxmlformats.org/spreadsheetml/2006/main}'
UUID = re.compile(r'[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}', re.I)
ZSTD_SUFFIX = '.jsonl.zst'
ZSTD_MAGIC = b'\x28\xb5\x2f\xfd'
ZSTD_UNDECODABLE = 'zstd: data could not be decoded'
ZSTD_UNAVAILABLE = 'zstd decoding needs Python 3.14 or later (compression.zstd); not searched'
ANTIGRAVITY_EXCLUDED = ('Antigravity: only <conversation>/.system_generated/logs/transcript.jsonl is searched; '
                        'transcript_full.jsonl (the same records untruncated, at the same lines), logs/chunks/ '
                        '(byte-split copies) and step thinking are not')
HARNESS_OPENERS = ('<task-notification>', '<local-command-caveat>', '<local-command-stdout>', '<cross-session-message>',
                   '<ci-monitor-event>', '<scheduled-task>', '<fork-boilerplate>', '<artifact-view-context>')
CLAUDE_EXCLUDED = ('Claude: of attachment records only queued_command (a message that arrived mid-turn) '
                   'is searched; queue-operation records (queue bookkeeping that repeats a message), '
                   'other attachment types, system and api-request records and thinking blocks are not')
# Antigravity steps whose text the app wrote, and steps whose content is tool output. CHECKPOINT
# is a compaction summary; any other step type is reported as unknown.
ANTIGRAVITY_SYSTEM_STEPS = ('SYSTEM_MESSAGE', 'EPHEMERAL_MESSAGE', 'CONVERSATION_HISTORY', 'ERROR_MESSAGE')
ANTIGRAVITY_TOOL_STEPS = ('GENERIC', 'RUN_COMMAND', 'VIEW_FILE', 'CODE_ACTION', 'LIST_DIRECTORY', 'GREP_SEARCH',
                          'SEARCH_WEB', 'READ_URL_CONTENT', 'GENERATE_IMAGE', 'INVOKE_SUBAGENT', 'ASK_QUESTION')
SUBAGENTS_CREATED = ('Created the following subagents:', 'Created the following subagent:')
CONVERSATION_ID = re.compile(r'"conversationId":[ \t\r\n]*"([^"]*)"')
# Official data exports, extracted from their ZIP: each conversations.json (or numbered
# conversations-NNN.json) holds a JSON array of conversations.
EXPORT_KINDS = ('chatgpt_export', 'claude_export')
EXPORT_FILE = re.compile(r'conversations(-[0-9]+)?\.json')
EXPORT_EXCLUDED = {
    'chatgpt_export': ('ChatGPT export: only conversations.json and conversations-NNN.json are searched; '
                       'thoughts and reasoning_recap content, images, audio, other files and the export\'s '
                       'other documents (chat.html, user, settings, feedback, manifest) are not'),
    'claude_export': ('Claude export: only conversations.json and conversations-NNN.json are searched; '
                      'thinking, token_budget and flag blocks, files without extracted text, and users.json, '
                      'projects.json and memories.json are not')}
JSON_SPACE = re.compile(r'[ \t\n\r]*')
EPOCH_NUMBER = re.compile(r'([0-9]{1,11})(?:\.([0-9]+))?')
# ChatGPT content types whose named string fields are searched, and those with nothing to search:
# reasoning is never indexed, and computer_output is a screenshot.
CHATGPT_FIELDS = {'code': ('text',), 'execution_output': ('text',), 'tether_browsing_display': ('result', 'summary'),
                  'tether_quote': ('title', 'text'), 'sonic_webpage': ('title', 'snippet', 'text'),
                  'system_error': ('name', 'text'), 'user_editable_context': ('user_profile', 'user_instructions')}
CHATGPT_SILENT = ('thoughts', 'reasoning_recap', 'computer_output')
# multimodal_text parts that hold media, not text.
CHATGPT_MEDIA = ('image_asset_pointer', 'audio_asset_pointer', 'real_time_user_audio_video_asset_pointer')
CLAUDE_EXPORT_SILENT = ('thinking', 'token_budget', 'flag')
# tool_result content items that may carry no text: web pages, local files, retrieved documents.
CLAUDE_RESULT_ITEMS = ('text', 'knowledge', 'local_resource', 'rag_reference')
# Bounds on what one export file may hold. Deeper nesting would reach recursion limits that differ
# between the cores; the Rust core cannot tell a literal U+10F800..U+10FFFF character from the
# placeholder it uses for a lone surrogate, so a conversation holding one is not read by either.
EXPORT_FILE_LIMIT = 1 << 30
EXPORT_DEPTH_LIMIT = 256
EXPORT_UNSUPPORTED = 'not a UTF-8 JSON array of conversations nested at most 256 levels deep'
# A literal character in that range, or a JSON escape whose surrogate pair decodes into it
# (high surrogate \udbfe or \udbff).
EXPORT_LOOKALIKE = re.compile('[\U0010F800-\U0010FFFF]|\\\\u[dD][bB][fF][eEfF]')
JSON_NOT_STRUCTURE = bytes(range(256)).translate(None, b'"[]{}')
JSON_PLAIN_STRING = re.compile(rb'"[^"]*"')
JSON_EMPTY_PAIR = re.compile(rb'\[\]|\{\}')
JSON_BRACKET = re.compile(rb'[\[\]{}]')

def time_key(value):
    try:
        parsed = datetime.fromisoformat(value.replace('Z', '+00:00'))
        return parsed.timestamp() if parsed.tzinfo is not None else float('-inf')
    except (ValueError, AttributeError, TypeError, OverflowError):
        return float('-inf')

def native_locator(value):
    if not value.startswith('codex://'):
        return None
    u = urlparse(value)
    ident = u.path.strip('/')
    if u.netloc != 'threads' or not UUID.fullmatch(ident):
        raise ValueError('expected codex://threads/<full UUID>?hostId=<host>')
    host = parse_qs(u.query).get('hostId', [None])[0]
    return {'thread_id': ident, 'host_id': host, 'tool': 'read_thread',
            'instruction': 'Use the supported native app tool; page until hasMore=false. Preserve output truncation flags.'}

def config_for(args):
    config_path = args.sources or os.environ.get('XSESSION_CONFIG')
    if config_path:
        return json.loads(Path(config_path).read_text(encoding='utf-8-sig'))
    home = Path.home()
    # The Windows-side home: the caller's own home unless XSESSION_WINDOWS_HOME names another
    # (under WSL, /mnt/c/Users/<name> reads the Windows stores).
    override = os.environ.get('XSESSION_WINDOWS_HOME')
    windows = Path(override) if override else home
    claude = os.environ.get('CLAUDE_PROJECTS_DIR', str(windows / '.claude/projects'))
    win_codex = Path(os.environ.get('CODEX_WIN_SESSIONS_DIR', str(windows / '.codex/sessions')))
    sources = [{'name': 'claude', 'kind': 'claude', 'path': claude},
               {'name': 'codex-windows', 'kind': 'codex', 'path': str(win_codex)},
               {'name': 'codex-archive', 'kind': 'codex', 'path': str(win_codex.parent / 'archived_sessions')},
               {'name': 'chatgpt-projects', 'kind': 'chatgpt_project',
                'path': os.environ.get('CHATGPT_PROJECTS_DIR', str(windows / '.codex/.chatgpt-projects'))},
               {'name': 'antigravity', 'kind': 'antigravity',
                'path': os.environ.get('ANTIGRAVITY_BRAIN_DIR', str(windows / '.gemini/antigravity/brain'))}]
    wsl = os.environ.get('CODEX_SESSIONS_DIR')
    if os.name != 'nt':
        wsl = wsl or str(home / '.codex/sessions')
    # Off Windows without XSESSION_WINDOWS_HOME, codex-windows already is ~/.codex/sessions:
    # never list the same store twice.
    if wsl and not (os.name != 'nt' and Path(wsl) == win_codex):
        sources.extend([{'name': 'codex-wsl', 'kind': 'codex', 'path': wsl},
                        {'name': 'codex-wsl-archive', 'kind': 'codex', 'path': str(Path(wsl).parent / 'archived_sessions')}])
    return {'sources': sources, 'title_index': str(windows / '.codex/session_index.jsonl'),
            'imports': str(windows / '.codex/external_agent_session_imports.json'),
            'excluded': ['native app stores require supported app tools',
                         'WSL is opt-in via CODEX_SESSIONS_DIR on Windows; no distro is started',
                         'memories and handoffs require explicit doc source roots']}

def initial_coverage(config):
    excluded = config.get('excluded', [])
    for kind, line in (('claude', CLAUDE_EXCLUDED), ('antigravity', ANTIGRAVITY_EXCLUDED),
                       ('chatgpt_export', EXPORT_EXCLUDED['chatgpt_export']),
                       ('claude_export', EXPORT_EXCLUDED['claude_export'])):
        if isinstance(excluded, list) and any(s.get('kind') == kind for s in config.get('sources', [])):
            excluded = excluded + [line]
    return {'scope': 'configured sources only', 'complete': True, 'sources': [],
            'errors': [], 'excluded': excluded, 'files': 0,
            'malformed_records': 0, 'unfinished_records': 0, 'changed_files': 0,
            'unsupported_records': 0, 'notes': [], 'mode': 'completed_record_prefixes'}

def is_compressed(path):
    """A transcript stored as zstd frames. It reads as its decompressed bytes, so line numbers,
    byte offsets and record digests mean what they mean for the plain .jsonl."""
    return str(path).endswith(ZSTD_SUFFIX)

def session_stem(path):
    """Path.stem of a transcript name, ignoring a .zst storage suffix."""
    path = Path(path)
    return Path(path.name[:-4]).stem if is_compressed(path) else path.stem

def _read_exact(stream, count):
    data = stream.read(count)
    if len(data) != count:
        raise OSError('zstd: truncated frame')
    return data

def zstd_content_size(stream, stored_size):
    """Decompressed size of a .jsonl.zst from its frame headers (RFC 8878), walking block
    headers without decoding. Skippable frames carry no content. Raises OSError for a stream
    this tool does not read: no content size, a dictionary, reserved bits, truncation, or
    bytes that are not a frame."""
    total = position = 0
    while position < stored_size:
        stream.seek(position)
        magic = _read_exact(stream, 4)
        if 0x184D2A50 <= int.from_bytes(magic, 'little') <= 0x184D2A5F:
            position += 8 + int.from_bytes(_read_exact(stream, 4), 'little')
            if position > stored_size:
                raise OSError('zstd: truncated frame')
            continue
        if magic != ZSTD_MAGIC:
            raise OSError('zstd: not a zstd frame')
        descriptor = _read_exact(stream, 1)[0]
        if descriptor & 0x08:
            raise OSError('zstd: reserved frame header bit set')
        single = descriptor >> 5 & 1
        if not single:
            _read_exact(stream, 1)
        dictionary = (0, 1, 2, 4)[descriptor & 3]
        if dictionary and int.from_bytes(_read_exact(stream, dictionary), 'little'):
            raise OSError('zstd: frame needs a dictionary')
        width = (single, 2, 4, 8)[descriptor >> 6]
        if not width:
            raise OSError('zstd: frame without content size')
        size = int.from_bytes(_read_exact(stream, width), 'little')
        total += size + 256 if width == 2 else size
        position = stream.tell()
        while True:
            block = int.from_bytes(_read_exact(stream, 3), 'little')
            kind = block >> 1 & 3
            if kind == 3:
                raise OSError('zstd: reserved block type')
            position += 3 + (1 if kind == 1 else block >> 3)
            if position > stored_size:
                raise OSError('zstd: truncated frame')
            stream.seek(position)
            if block & 1:
                break
        if descriptor & 0x04:
            position += 4
            if position > stored_size:
                raise OSError('zstd: truncated frame')
    return total

class ZstdLogical:
    """Read-only view of a .jsonl.zst as its decompressed bytes, with the file methods this tool
    uses. Seeking forward decodes; seeking back restarts at the first frame. Decoder failures
    surface as OSError, like any other read failure."""
    def __init__(self, raw, own=True):
        self._raw = raw
        self._own = own
        # An empty file holds no frame and reads as an empty transcript, like an empty .jsonl.
        self._reader = io.BytesIO() if os.fstat(raw.fileno()).st_size == 0 else _zstd.ZstdFile(raw, 'rb')

    def _call(self, method, *args):
        try:
            return method(*args)
        except (_zstd.ZstdError, EOFError) as exc:
            raise OSError(ZSTD_UNDECODABLE) from exc

    def read(self, size=-1):
        return self._call(self._reader.read, size)

    def readline(self, size=-1):
        return self._call(self._reader.readline, size)

    def seek(self, offset, whence=0):
        return self._call(self._reader.seek, offset, whence)

    def tell(self):
        return self._reader.tell()

    def __iter__(self):
        while True:
            line = self.readline()
            if not line:
                return
            yield line

    def close(self):
        try:
            self._reader.close()
        finally:
            if self._own:
                self._raw.close()

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.close()

def open_session(path):
    """open(path, 'rb') for a transcript; a .jsonl.zst opens as its decompressed bytes."""
    if not is_compressed(path):
        return Path(path).open('rb')
    if _zstd is None:
        raise OSError(ZSTD_UNAVAILABLE)
    return ZstdLogical(Path(path).open('rb'))

class SessionStat:
    """os.stat() of a transcript in logical terms. A .jsonl.zst reports its decompressed size
    and the stored file's mtime; stored_size is the size on disk (None for a plain file)."""
    __slots__ = ('st_size', 'st_mtime_ns', 'stored_size')

    def __init__(self, st_size, st_mtime_ns, stored_size=None):
        self.st_size, self.st_mtime_ns, self.stored_size = st_size, st_mtime_ns, stored_size

    def identity(self):
        return self.st_size, self.st_mtime_ns, self.stored_size

def session_stat(path, previous=None):
    """A compressed file's decompressed size comes from its frame headers, or from `previous`
    (an earlier session_stat of the same file) while the stored size and mtime are unchanged."""
    stat = Path(path).stat()
    if not is_compressed(path):
        return SessionStat(stat.st_size, stat.st_mtime_ns)
    if previous is not None and (previous.stored_size, previous.st_mtime_ns) == (stat.st_size, stat.st_mtime_ns):
        return SessionStat(previous.st_size, stat.st_mtime_ns, stat.st_size)
    with Path(path).open('rb') as raw:
        return SessionStat(zstd_content_size(raw, stat.st_size), stat.st_mtime_ns, stat.st_size)

def stored_unchanged(path, stored):
    """A compressed transcript is rewritten whole, never appended: an unchanged stored size and
    mtime mean the bytes just decoded are the bytes on disk."""
    now = Path(path).stat()
    return (now.st_size, now.st_mtime_ns) == (stored.st_size, stored.st_mtime_ns)

def undecodable(coverage, path, since=0):
    """Whether reading `path` met undecodable zstd data (a read_error recorded at or after index
    `since`). How much of a corrupt stream decodes before the failure depends on read-ahead, so a
    transcript that fails to decode contributes nothing from its interior: its hits, context, list
    details and per-record diagnostics are dropped, and one read_error stands for it."""
    target = str(Path(path))
    return any(e['kind'] == 'read_error' and e['detail'] == ZSTD_UNDECODABLE and e['path'] == target
               for e in coverage['errors'][since:])

COUNTERS = ('malformed_records', 'unfinished_records', 'changed_files', 'unsupported_records')

def coverage_mark(coverage):
    return (len(coverage['errors']), len(coverage['notes']), {key: coverage[key] for key in COUNTERS},
            coverage.get('complete'))

def undecodable_only(coverage, mark, path):
    """Undo what one read of `path` added to coverage, then record its single read_error."""
    errors, notes, counters, complete = mark
    del coverage['errors'][errors:]
    del coverage['notes'][notes:]
    coverage.update(counters)
    coverage['complete'] = complete
    problem(coverage, 'read_error', Path(path), ZSTD_UNDECODABLE)

def zstd_validates(path):
    """Decode a whole .jsonl.zst and discard the output: whether every frame decodes."""
    try:
        with open_session(path) as f:
            while f.read(1024 * 1024):
                pass
        return True
    except OSError:
        return False

def prefix_digest(path, size):
    digest = hashlib.sha256()
    with open_session(path) as stream:
        remaining = size
        while remaining:
            chunk = stream.read(min(1024 * 1024, remaining))
            if not chunk:
                raise OSError('captured prefix was truncated')
            digest.update(chunk)
            remaining -= len(chunk)
    return digest.hexdigest()

class PrefixCheck:
    """Running check over the bytes a search reads. SHA-256 when a paging snapshot pins the
    prefix; otherwise CRC32, which only has to notice an in-place rewrite during the read."""
    def __init__(self, strong):
        self.strong = strong
        self.sha = hashlib.sha256() if strong else None
        self.crc = 0

    def update(self, data):
        if self.strong:
            self.sha.update(data)
        else:
            self.crc = zlib.crc32(data, self.crc)

    def value(self):
        return self.sha.hexdigest() if self.strong else 'crc32:%08x' % self.crc

def prefix_check(path, size, strong):
    if strong:
        return prefix_digest(path, size)
    check = PrefixCheck(False)
    with open_session(path) as stream:
        remaining = size
        while remaining:
            chunk = stream.read(min(1024 * 1024, remaining))
            if not chunk:
                raise OSError('captured prefix was truncated')
            check.update(chunk)
            remaining -= len(chunk)
    return check.value()

def guards_from_stream(stream, size):
    guards = []
    for start, length in ((0, min(4096, size)), (max(4096, size - 4096), min(4096, max(0, size - 4096)))):
        if length:
            stream.seek(start)
            data = stream.read(length)
            guards.append([start, length, hashlib.sha256(data).hexdigest()])
    return guards

def capture_guards(path, size):
    with path.open('rb') as stream:
        return guards_from_stream(stream, size)

def guards_match(path, guards):
    with path.open('rb') as stream:
        for start, length, digest in guards:
            stream.seek(start)
            if hashlib.sha256(stream.read(length)).hexdigest() != digest:
                return False
    return True

def public_row(row):
    return {key: value for key, value in row.items() if not key.startswith('_')}

def problem(coverage, kind, path, detail=''):
    coverage['complete'] = False
    coverage['errors'].append({'kind': kind, 'path': str(path), 'detail': detail})

def inspect_file(path, source, coverage, guard=True, resolved=None):
    stat = path.stat()
    compressed = is_compressed(path)
    ident = None
    timestamp = None
    parent = None
    resumed_from = None
    row_kind = 'conversation'
    guards = []
    # Identity parsing is bounded; unrecognised files stay visible as incomplete.
    with path.open('rb') as raw:
        if compressed:
            # One open: the frame walk, the identity lines, and the stored-byte guards.
            content = zstd_content_size(raw, stat.st_size)
            raw.seek(0)
            f = ZstdLogical(raw, own=False)
        else:
            content, f = stat.st_size, raw
        try:
            for _ in range(64):
                line = f.readline(1024 * 1024 + 1)
                if not line:
                    break
                if len(line) > 1024 * 1024:
                    break
                try:
                    e = json.loads(line)
                except (ValueError, UnicodeError):
                    continue
                if not isinstance(e, dict):
                    continue
                if source['kind'] == 'antigravity':
                    timestamp = e.get('created_at')
                    break
                if e.get('type') == 'session_meta':
                    p = e.get('payload') or {}
                    if not isinstance(p, dict):
                        problem(coverage, 'unsupported_metadata', path)
                        continue
                    ident = p.get('id') or p.get('session_id')
                    timestamp = p.get('timestamp') or e.get('timestamp')
                    origin = p.get('source')
                    if isinstance(origin, dict) and 'subagent' in origin:
                        row_kind = 'subagent'
                        sub = origin['subagent']
                        if isinstance(sub, dict):
                            spawn = sub.get('spawn', {})
                            parent = spawn.get('parent_thread_id') if isinstance(spawn, dict) else None
                            if 'review' in sub:
                                row_kind = 'reviewer'
                    break
                if source['kind'] == 'claude' and e.get('sessionId'):
                    ident = e['sessionId']
                    timestamp = e.get('timestamp')
                    break
        finally:
            if compressed:
                f.close()
        if guard:
            # Guards hash the stored bytes; for a .jsonl.zst these are the compressed bytes.
            guards = guards_from_stream(raw, stat.st_size)
    stem = session_stem(path)
    if source['kind'] == 'antigravity':
        # <conversation id>/.system_generated/logs/transcript.jsonl: the folder names the identity.
        ident = path.parents[2].name
    elif source['kind'] == 'claude' and 'subagents' in path.parts:
        parent = next((p for p in reversed(path.parts) if UUID.fullmatch(p)), None) or ident
        ident = (parent + '::' if parent else '') + stem
        row_kind = 'subagent'
    elif source['kind'] == 'claude' and UUID.fullmatch(stem):
        resumed_from = ident if ident and ident.casefold() != stem.casefold() else None
        ident = stem
    if not ident:
        # No filename-derived Codex identity: segmented names are not UUIDs.
        problem(coverage, 'identity_unavailable', path)
        return None
    # For a .jsonl.zst, `bytes` is the decompressed size and `mtime_ns` the stored file's.
    return {'id': ident, 'agent': source['kind'], 'source': source['name'],
            'kind': row_kind, 'parent_id': parent, 'resumed_from': resumed_from, 'path': resolved or str(path.resolve()),
            'bytes': content, 'mtime_ns': stat.st_mtime_ns,
            'timestamp': timestamp, 'timestamp_basis': 'first metadata record',
            'title': None, '_guards': guards}

def project_format(name):
    suffix = Path(name).suffix.lower()
    if suffix in PROJECT_TEXT_SUFFIXES:
        return 'text'
    return suffix[1:] if suffix in ('.docx', '.xlsx') else None

def project_manifest(root, project, coverage):
    """A ChatGPT project's file manifest, `.metadata/<project>.json` schema version 1:
    {"version": 1, "files": [{"fileId", "name", "sha256"}]}. Returns {name: sha256}, or None
    when it is unavailable or has an unknown schema; files then stay searchable but unverified."""
    path = root / '.metadata' / (project.name + '.json')
    try:
        data = json.loads(path.read_text(encoding='utf-8-sig'))
        if not isinstance(data, dict) or data.get('version') != 1 or not isinstance(data.get('files'), list):
            raise ValueError('unsupported manifest schema')
        listed = {}
        for entry in data['files']:
            if not (isinstance(entry, dict) and isinstance(entry.get('name'), str) and isinstance(entry.get('sha256'), str)):
                raise ValueError('unsupported manifest entry')
            listed[entry['name']] = entry['sha256'].lower()
        return listed
    except OSError as exc:
        problem(coverage, 'project_manifest_unavailable', path, str(exc))
    except (ValueError, UnicodeError) as exc:
        problem(coverage, 'project_manifest_unsupported', path, str(exc))
    return None

def project_rows(root, resolved_root, source, status, coverage, seen_paths, guard):
    """Rows for each project's AGENTS.md instructions and its sources/ files. Searchable files
    carry their manifest verification state; images, archives and other binary files are
    disclosed in coverage notes; a listed file that is absent makes coverage incomplete."""
    rows = []
    for project in sorted(p for p in root.iterdir() if p.is_dir() and not p.name.startswith('.')):
        if project.is_symlink() or (hasattr(project, 'is_junction') and project.is_junction()):
            problem(coverage, 'excluded_link', project)
            continue
        listed = project_manifest(root, project, coverage)
        instructions = project / 'AGENTS.md'
        files = [instructions] if instructions.is_file() else []
        folder = project / 'sources'
        if folder.is_dir():
            for p in sorted(folder.iterdir()):
                if p.is_symlink() or (hasattr(p, 'is_junction') and p.is_junction()):
                    problem(coverage, 'excluded_link', p)
                elif p.is_dir():
                    coverage['notes'].append({'kind': 'project_subdirectory_not_searched', 'path': str(p), 'detail': ''})
                else:
                    files.append(p)
        present = set()
        for p in files:
            resolved = str(resolved_root / p.relative_to(root))
            if resolved in seen_paths:
                continue
            seen_paths.add(resolved)
            status['files'] += 1
            coverage['files'] += 1
            is_instructions = p == instructions
            if not is_instructions:
                present.add(p.name)
            form = project_format(p.name)
            if form is None:
                coverage['notes'].append({'kind': 'not_text_searchable', 'path': resolved, 'detail': p.suffix.lower()})
                continue
            try:
                stat = p.stat()
                if is_instructions:
                    state = 'instructions_not_manifested'
                elif listed is None:
                    state = 'unverified'
                elif p.name not in listed:
                    state = 'unlisted'
                    coverage['notes'].append({'kind': 'project_file_unlisted', 'path': resolved, 'detail': ''})
                elif hashlib.sha256(p.read_bytes()).hexdigest() == listed[p.name]:
                    state = 'verified'
                else:
                    state = 'hash_mismatch'
                    coverage['notes'].append({'kind': 'project_manifest_mismatch', 'path': resolved,
                                              'detail': 'file bytes differ from the project manifest SHA-256'})
                rows.append({'id': 'chatgpt:' + project.name + ':' + p.relative_to(project).as_posix(),
                             'agent': 'chatgpt_project', 'source': source['name'],
                             'kind': 'project_instructions' if is_instructions else 'project_source',
                             'parent_id': project.name, 'path': resolved, 'bytes': stat.st_size,
                             'mtime_ns': stat.st_mtime_ns, 'timestamp': None, 'timestamp_basis': 'unknown',
                             'title': p.name, 'format': form, 'manifest': state,
                             '_guards': capture_guards(p, stat.st_size) if guard else []})
            except OSError as exc:
                problem(coverage, 'read_error', p, str(exc))
        for name in sorted(set(listed or ()) - present):
            problem(coverage, 'project_file_missing', folder / name, 'listed in the project manifest')
    return rows

def package_units(form, data):
    """Text units of an Office Open XML package in document order: Word paragraphs from the
    main document part; Excel shared strings, then inline and formula strings per sheet."""
    with zipfile.ZipFile(io.BytesIO(data)) as package:
        def part(name):
            info = package.getinfo(name)
            if info.file_size > PACKAGE_PART_LIMIT:
                raise ValueError(name + ' exceeds the extraction limit')
            return ET.fromstring(package.read(info))
        units = []
        if form == 'docx':
            for paragraph in part('word/document.xml').iter(WORD + 'p'):
                units.append(''.join(t.text or '' for t in paragraph.iter(WORD + 't')))
        else:
            names = package.namelist()
            if 'xl/sharedStrings.xml' in names:
                for item in part('xl/sharedStrings.xml').iter(SHEET + 'si'):
                    units.append(''.join(t.text or '' for t in item.iter(SHEET + 't')))
            for name in sorted(n for n in names if n.startswith('xl/worksheets/') and n.endswith('.xml')):
                for cell in part(name).iter(SHEET + 'c'):
                    if cell.get('t') == 'inlineStr':
                        units.append(''.join(t.text or '' for t in cell.iter(SHEET + 't')))
                    elif cell.get('t') == 'str' and cell.find(SHEET + 'v') is not None:
                        units.append(cell.find(SHEET + 'v').text or '')
    return [unit for unit in units if unit]

PACKAGE_ERRORS = (zipfile.BadZipFile, zipfile.LargeZipFile, KeyError, ValueError, ET.ParseError,
                  RuntimeError, NotImplementedError, EOFError, zlib.error)

def package_records(row, coverage, needle=None, strong=True):
    """records() for a .docx/.xlsx project file: one record per text unit. `line` is the unit
    number and `record_sha256` hashes the unit's UTF-8 text (there is no raw JSONL record)."""
    p = Path(row['path'])
    try:
        data = p.read_bytes()
        if len(data) != row['bytes'] or not guards_match(p, row.get('_guards', [])):
            problem(coverage, 'changed_since_inventory', p)
    except OSError as exc:
        problem(coverage, 'read_error', p, str(exc))
        return
    expected = row.get('_prefix_sha256')
    if strong or expected:
        digest = hashlib.sha256(data).hexdigest()
        if expected and digest != expected:
            coverage['snapshot_invalid'] = True
            problem(coverage, 'snapshot_prefix_changed', p)
        row['_prefix_sha256'] = digest
    try:
        units = package_units(row['format'], data)
    except PACKAGE_ERRORS as exc:
        problem(coverage, 'project_file_unreadable', p, type(exc).__name__ + ': ' + str(exc)[:300])
        return
    for number, text in enumerate(units, 1):
        if needle is not None and needle not in text.casefold():
            continue
        provenance, confidence = classify(row, 'document', text)
        yield {**row, 'line': number, 'byte_offset': None, 'block': 0,
               'record_sha256': hashlib.sha256(text.encode('utf-8')).hexdigest(), 'record_id': None,
               'timestamp': None, 'timestamp_basis': 'unknown', 'role': 'document',
               'record_session_id': None, 'provenance': provenance, 'content_provenance': provenance,
               'confidence': confidence, 'text': text,
               'self': False}

def antigravity_rows(root, resolved_root, source, status, coverage, seen_paths, guard):
    """Rows for an Antigravity brain folder: each conversation folder, named by its UUID, keeps its
    transcript at .system_generated/logs/transcript.jsonl. transcript_full.jsonl and chunks/ beside it
    hold copies of the same records and are not read; other folders hold artifacts and step output."""
    rows = []
    for folder in sorted(root.iterdir(), key=lambda p: p.name):
        if not UUID.fullmatch(folder.name):
            continue
        logs = folder / '.system_generated' / 'logs'
        try:
            linked = next((p for p in (folder, logs.parent, logs)
                           if p.is_symlink() or (hasattr(p, 'is_junction') and p.is_junction())), None)
            if linked:
                problem(coverage, 'excluded_link', linked)
                continue
            if not logs.is_dir():
                continue
            names = sorted(n for n in os.listdir(logs)
                           if (n == 'transcript.jsonl' or n.startswith('transcript.jsonl.')) and not (logs / n).is_dir())
        except OSError as exc:
            problem(coverage, 'read_error', folder, str(exc))
            continue
        for name in names:
            p = logs / name
            if name != 'transcript.jsonl' and not (name == 'transcript' + ZSTD_SUFFIX and _zstd is not None):
                problem(coverage, 'unsupported_session_file', p,
                        ZSTD_UNAVAILABLE if name == 'transcript' + ZSTD_SUFFIX else
                        "unrecognized transcript encoding '%s'; not searched" % name[name.index('.jsonl.') + 6:])
                continue
            if p.is_symlink():
                problem(coverage, 'excluded_link', p)
                continue
            resolved = str(resolved_root / p.relative_to(root))
            if resolved in seen_paths:
                continue
            seen_paths.add(resolved)
            status['files'] += 1
            coverage['files'] += 1
            try:
                row = inspect_file(p, source, coverage, guard, resolved)
                if row:
                    rows.append(row)
            except OSError as exc:
                problem(coverage, 'read_error', p, str(exc))
    return rows

def epoch_iso(value):
    """A ChatGPT export time, seconds since the Unix epoch as a JSON number, as ISO 8601 UTC text.
    It is read from the number's shortest decimal form and cut, not rounded, to microseconds, so
    both cores give the same text without float arithmetic. Other values, and whole parts of more
    than 11 digits (after the year 5138), give None."""
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    match = EPOCH_NUMBER.fullmatch(repr(value))
    if not match:
        return None
    moment = datetime(1970, 1, 1) + timedelta(seconds=int(match[1]))
    return '%04d-%02d-%02dT%02d:%02d:%02d.%sZ' % (moment.year, moment.month, moment.day, moment.hour,
                                                   moment.minute, moment.second, (match[2] or '').ljust(6, '0')[:6])

def nested_deeper(data, limit=EXPORT_DEPTH_LIMIT):
    """Whether JSON text in `data` nests arrays and objects more than `limit` deep. Exact for valid
    JSON: escaped backslashes and quotes go first, then everything but quotes and brackets, then
    strings; each pass then removes the innermost pairs, one level a pass."""
    text = data.replace(b'\\\\', b'').replace(b'\\"', b'').translate(None, JSON_NOT_STRUCTURE)
    text = JSON_PLAIN_STRING.sub(b'', text)
    for _ in range(limit):
        text, removed = JSON_EMPTY_PAIR.subn(b'', text)
        if not removed:
            break
    return JSON_BRACKET.search(text) is not None

def export_spans(data):
    """(start, length, value, lookalike) for each element of the top-level JSON array in `data`, one
    at a time, with byte offsets into `data`; `lookalike` says the element's text holds a literal
    U+10F800..U+10FFFF character. Raises ValueError or UnicodeError, possibly after some elements,
    when `data` is not one UTF-8 JSON array nested at most EXPORT_DEPTH_LIMIT deep."""
    bom = 3 if data.startswith(b'\xef\xbb\xbf') else 0
    text = data[bom:].decode('utf-8')
    if nested_deeper(data[bom:]):
        raise ValueError('nested too deeply')
    decoder = json.JSONDecoder()
    cursor = [0, bom]
    def at(index):
        # Byte offset of a character index; indexes only move forward.
        cursor[1] += len(text[cursor[0]:index].encode('utf-8'))
        cursor[0] = index
        return cursor[1]
    i = JSON_SPACE.match(text).end()
    if text[i:i + 1] != '[':
        raise ValueError('not a JSON array')
    i = JSON_SPACE.match(text, i + 1).end()
    if text[i:i + 1] == ']':
        i += 1
    else:
        while True:
            value, end = decoder.raw_decode(text, i)
            start = at(i)
            yield start, at(end) - start, value, EXPORT_LOOKALIKE.search(text, i, end) is not None
            i = JSON_SPACE.match(text, end).end()
            if text[i:i + 1] == ',':
                i = JSON_SPACE.match(text, i + 1).end()
            elif text[i:i + 1] == ']':
                i += 1
                break
            else:
                raise ValueError('expected , or ]')
    if JSON_SPACE.match(text, i).end() != len(text):
        raise ValueError('extra data after the array')

def export_conversation_fields(kind, value):
    """(identity, title, created, updated) of one exported conversation object."""
    if kind == 'chatgpt_export':
        ident = value.get('conversation_id') or value.get('id')
        title, created, updated = value.get('title'), epoch_iso(value.get('create_time')), epoch_iso(value.get('update_time'))
    else:
        ident, title, created, updated = value.get('uuid'), value.get('name'), value.get('created_at'), value.get('updated_at')
        created = created if isinstance(created, str) else None
        updated = updated if isinstance(updated, str) else None
    return (ident if isinstance(ident, str) and ident else None,
            title if isinstance(title, str) and title else None, created, updated)

def export_rows(root, resolved_root, source, status, coverage, seen_paths, guard):
    """Rows for a folder holding official data exports (ChatGPT or claude.ai) extracted from their
    ZIP. Every conversations.json or conversations-NNN.json is a JSON array of conversations, and
    each conversation is one row: `export_offset` and `bytes` give its span in that file, and its
    guard, kept for every command, hashes the span. A ZIP is reported, not opened; a file over
    EXPORT_FILE_LIMIT is reported, not read; a file that is not one array is reported whole, with
    no rows. An identity seen earlier in the same file is reported, not listed twice. Each file's
    newest update is a note: an export is a snapshot, and nothing after it is here."""
    rows = []
    def walk_error(exc):
        status['status'] = 'partial'
        problem(coverage, 'read_error', root, str(exc))
    for directory, dirs, names in os.walk(root, followlinks=False, onerror=walk_error):
        kept = []
        for d in dirs:
            p = Path(directory) / d
            if p.is_symlink() or (hasattr(p, 'is_junction') and p.is_junction()):
                problem(coverage, 'excluded_link', p)
            else:
                kept.append(d)
        dirs[:] = sorted(kept)
        for name in sorted(names):
            p = Path(directory) / name
            resolved = str(resolved_root / p.relative_to(root))
            if name.lower().endswith('.zip'):
                coverage['notes'].append({'kind': 'export_archive_not_searched', 'path': resolved,
                                          'detail': 'extract the archive to search its conversations'})
                continue
            if not EXPORT_FILE.fullmatch(name):
                continue
            if p.is_symlink():
                problem(coverage, 'excluded_link', p)
                continue
            if resolved in seen_paths:
                continue
            seen_paths.add(resolved)
            status['files'] += 1
            coverage['files'] += 1
            try:
                stat = p.stat()
                if stat.st_size > EXPORT_FILE_LIMIT:
                    problem(coverage, 'export_too_large', p, 'larger than 1 GiB; not read')
                    continue
                data = p.read_bytes()
            except OSError as exc:
                problem(coverage, 'read_error', p, str(exc))
                continue
            if len(data) != stat.st_size:
                problem(coverage, 'changed_during_read', p)
                continue
            found, issues, identities, newest = [], [], set(), None
            try:
                for number, (start, length, value, lookalike) in enumerate(export_spans(data), 1):
                    where = 'conversation ' + str(number)
                    if lookalike:
                        issues.append(('unsupported_record', where + ': a character in U+10F800..U+10FFFF', True))
                        continue
                    if not isinstance(value, dict):
                        issues.append(('unsupported_record', where, True))
                        continue
                    ident, title, created, updated = export_conversation_fields(source['kind'], value)
                    if ident is None:
                        issues.append(('identity_unavailable', where, False))
                        continue
                    if ident.casefold() in identities:
                        issues.append(('identity_duplicate', where, False))
                        continue
                    identities.add(ident.casefold())
                    if updated and time_key(updated) > time_key(newest):
                        newest = updated
                    found.append({'id': ident, 'agent': source['kind'], 'source': source['name'], 'kind': 'conversation',
                                  'parent_id': None, 'path': resolved, 'bytes': length, 'mtime_ns': stat.st_mtime_ns,
                                  'timestamp': created, 'timestamp_basis': 'conversation created' if created else 'unknown',
                                  'title': title, 'export_offset': start, 'export_conversation': number,
                                  '_guards': [[start, length, hashlib.sha256(data[start:start + length]).hexdigest()]]})
            except (ValueError, UnicodeError):
                problem(coverage, 'export_unsupported', p, EXPORT_UNSUPPORTED)
                continue
            for kind, detail, counted in issues:
                if counted:
                    coverage['unsupported_records'] += 1
                problem(coverage, kind, p, detail)
            rows.extend(found)
            coverage['notes'].append({'kind': 'export_snapshot', 'path': resolved,
                                      'detail': 'a snapshot as exported; newest conversation update ' + (newest or 'unknown')})
    return rows

def export_block(path):
    return json.dumps(list(path), ensure_ascii=False, separators=(',', ':'))

def export_messages(kind, conversation):
    """(message, on_current_branch) for each message of one exported conversation, in reading order.
    ChatGPT keeps a conversation as a tree (`mapping`, with edits and regenerations as branches),
    stored in no particular order: it is read depth-first from each root, children in stored order,
    then any node left unreached, and `current_node` names the branch shown in the app. A claude.ai
    export keeps a list and names no current branch, so the flag is None. Raises ValueError on an
    unknown shape."""
    if kind == 'claude_export':
        messages = conversation.get('chat_messages')
        if not isinstance(messages, list):
            raise ValueError('chat_messages must be a list')
        return [(message, None) for message in messages]
    mapping = conversation.get('mapping')
    if not isinstance(mapping, dict):
        raise ValueError('mapping must be an object')
    def parent_of(key):
        node = mapping[key]
        parent = node.get('parent') if isinstance(node, dict) else None
        return parent if isinstance(parent, str) and parent in mapping else None
    current = set()
    key = conversation.get('current_node')
    while isinstance(key, str) and key in mapping and key not in current:
        current.add(key)
        key = parent_of(key)
    order, seen = [], set()
    for start in [k for k in mapping if parent_of(k) is None] + list(mapping):
        stack = [start]
        while stack:
            key = stack.pop()
            if key in seen:
                continue
            seen.add(key)
            order.append(key)
            children = mapping[key].get('children') if isinstance(mapping[key], dict) else None
            if isinstance(children, list):
                stack.extend(reversed([c for c in children if isinstance(c, str) and c in mapping and c not in seen]))
    messages = []
    for key in order:
        node = mapping[key]
        if not isinstance(node, dict):
            # Not a node at all (null included): passed on, to be counted as unsupported.
            messages.append((node, key in current))
        elif node.get('message') is not None:
            messages.append((node['message'], key in current))
    return messages

def chatgpt_fields(message):
    """(fields, unknown) of one ChatGPT export message. A user message the app hides (custom
    instructions, context it adds) has the system role; an assistant message addressed to a tool is
    a tool call. Reasoning (thoughts, reasoning_recap) is never searched. `unknown` names content
    types and parts this reader does not know. Raises ValueError for an unknown author or content shape."""
    author = message.get('author')
    role = author.get('role') if isinstance(author, dict) else None
    if role not in ('user', 'assistant', 'system', 'tool'):
        raise ValueError('unknown author role')
    content = message.get('content')
    if not isinstance(content, dict):
        raise ValueError('content must be an object')
    kind = content.get('content_type')
    metadata = message.get('metadata')
    if role == 'user' and isinstance(metadata, dict) and metadata.get('is_visually_hidden_from_conversation') is True:
        role = 'system'
    if role == 'assistant' and message.get('recipient') not in (None, 'all'):
        role = 'tool_input'
    fields, unknown = [], []
    if kind in ('text', 'multimodal_text'):
        parts = content.get('parts')
        if not isinstance(parts, list):
            raise ValueError('parts must be a list')
        for index, part in enumerate(parts):
            part_kind = part.get('content_type') if isinstance(part, dict) else None
            if isinstance(part, str):
                if part:
                    fields.append((role, part, export_block(('content', 'parts', index))))
            elif part_kind == 'audio_transcription':
                if isinstance(part.get('text'), str) and part['text']:
                    fields.append((role, part['text'], export_block(('content', 'parts', index, 'text'))))
            elif part_kind not in CHATGPT_MEDIA:
                unknown.append('part ' + (part_kind if isinstance(part_kind, str) else '(none)'))
        return fields, unknown
    if kind in CHATGPT_SILENT:
        return fields, []
    if not isinstance(kind, str) or kind not in CHATGPT_FIELDS:
        return fields, ['content type ' + (kind if isinstance(kind, str) else '(none)')]
    if kind == 'user_editable_context':
        role = 'system'
    for name in CHATGPT_FIELDS[kind]:
        value = content.get(name)
        if isinstance(value, str) and value:
            fields.append((role, value, export_block(('content', name))))
    return fields, []

def claude_export_fields(message):
    """(fields, unknown) of one claude.ai export message: its content blocks, as Claude Code's are
    read, and the text claude.ai extracted from attached files. Thinking is never searched. `unknown`
    names block types, tool result items and attachments this reader does not know. Raises
    ValueError for an unknown sender or shape."""
    sender = message.get('sender')
    role = {'human': 'user', 'assistant': 'assistant'}.get(sender) if isinstance(sender, str) else None
    if role is None:
        raise ValueError('unknown sender')
    content = message.get('content')
    if not isinstance(content, list):
        raise ValueError('content must be a list')
    fields, unknown = [], []
    for index, block in enumerate(content):
        kind = block.get('type') if isinstance(block, dict) else None
        if kind in ('text', 'voice_note'):
            if isinstance(block.get('text'), str) and block['text']:
                fields.append((role, block['text'], index))
        elif kind == 'tool_use':
            fields.extend(('tool_input', text, field) for text, field in input_strings(block.get('input', {}), (index, 'input'))
                          if text)
        elif kind == 'tool_result':
            value = block.get('content', '')
            if isinstance(value, str):
                if value:
                    fields.append(('tool', value, index))
            elif isinstance(value, list):
                for sub_index, sub in enumerate(value):
                    sub_kind = sub.get('type') if isinstance(sub, dict) else None
                    if sub_kind not in CLAUDE_RESULT_ITEMS:
                        unknown.append('tool result ' + (sub_kind if isinstance(sub_kind, str) else '(none)'))
                    elif isinstance(sub.get('text'), str) and sub['text']:
                        fields.append(('tool', sub['text'], str(index) + '.content[' + str(sub_index) + ']'))
            elif value is not None:
                unknown.append('tool result content')
        elif kind not in CLAUDE_EXPORT_SILENT:
            unknown.append('content type ' + (kind if isinstance(kind, str) else '(none)'))
    attachments = message.get('attachments')
    for index, item in enumerate(attachments if isinstance(attachments, list) else []):
        if not isinstance(item, dict):
            unknown.append('attachment')
        elif isinstance(item.get('extracted_content'), str) and item['extracted_content']:
            fields.append(('attachment', item['extracted_content'], export_block(('attachments', index, 'extracted_content'))))
    return fields, unknown

def export_span(row):
    with Path(row['path']).open('rb') as f:
        f.seek(row['export_offset'])
        return f.read(row['bytes'])

def export_message_fields(row, coverage, strong=True, report=True):
    """(number, message, on_current_branch, fields) for each message of an exported conversation
    that this reader can read, after checking its span against the inventory. A span that differs
    from the one the inventory read is checked again for depth and lookalike characters. With
    `report` false (listing), what this reader cannot read is skipped without being counted."""
    p = Path(row['path'])
    try:
        data = export_span(row)
    except OSError as exc:
        if row.get('_prefix_sha256'):
            coverage['snapshot_invalid'] = True
        problem(coverage, 'read_error', p, str(exc))
        return
    digest = hashlib.sha256(data).hexdigest()
    guards = row.get('_guards') or []
    verified = bool(guards) and len(data) == row['bytes'] and all(
        g == [row['export_offset'], row['bytes'], digest] for g in guards)
    if len(data) != row['bytes'] or any(g != [row['export_offset'], row['bytes'], digest] for g in guards):
        problem(coverage, 'changed_since_inventory', p)
    expected = row.get('_prefix_sha256')
    if expected and digest != expected:
        coverage['snapshot_invalid'] = True
        problem(coverage, 'snapshot_prefix_changed', p)
    if strong or expected:
        row['_prefix_sha256'] = digest
    where = 'conversation ' + str(row['export_conversation'])
    def unsupported(detail):
        if report:
            coverage['unsupported_records'] += 1
            problem(coverage, 'unsupported_record', p, detail)
    try:
        text = data.decode('utf-8')
        if not verified and (nested_deeper(data) or EXPORT_LOOKALIKE.search(text)):
            raise ValueError('not readable as inventoried')
        conversation = json.loads(text)
        if not isinstance(conversation, dict):
            raise ValueError('conversation must be an object')
        messages = export_messages(row['agent'], conversation)
    except (ValueError, UnicodeError):
        unsupported(where)
        return
    reader = chatgpt_fields if row['agent'] == 'chatgpt_export' else claude_export_fields
    id_key = 'id' if row['agent'] == 'chatgpt_export' else 'uuid'
    identities = set()
    for number, (message, current) in enumerate(messages, 1):
        try:
            if not isinstance(message, dict):
                raise ValueError('message must be an object')
            ident = message.get(id_key)
            if isinstance(ident, str):
                # A message id names one message; a second message with it is not read.
                if ident in identities:
                    unsupported(where + ' message ' + str(number) + ': duplicate message id')
                    continue
                identities.add(ident)
            fields, unknown = reader(message)
        except ValueError:
            unsupported(where + ' message ' + str(number))
            continue
        for kind in unknown:
            unsupported(where + ' message ' + str(number) + ': ' + kind)
        yield number, message, current, fields

def export_records(row, coverage, needle=None, strong=True):
    """records() for one conversation of a data export. `line` numbers its messages in reading order
    and `block` is the field within the message. An export is one JSON document, so a message has no
    line of its own: `byte_offset` is None, and `record_sha256` hashes the message object as
    json.dumps writes it (ASCII, default separators)."""
    chatgpt = row['agent'] == 'chatgpt_export'
    for number, message, current, fields in export_message_fields(row, coverage, strong):
        if needle is not None:
            fields = [(role, text, block) for role, text, block in fields if needle in text.casefold()]
        if not fields:
            continue
        digest = hashlib.sha256(json.dumps(message).encode('utf-8')).hexdigest()
        if chatgpt:
            timestamp, record_id = epoch_iso(message.get('create_time')), message.get('id')
        else:
            timestamp, record_id = message.get('created_at'), message.get('uuid')
            timestamp = timestamp if isinstance(timestamp, str) else None
        record_id = record_id if isinstance(record_id, str) else None
        for role, text, block in fields:
            provenance, confidence = classify(row, role, text)
            hit = {**row, 'line': number, 'byte_offset': None, 'block': block,
                   'record_sha256': digest, 'record_id': record_id,
                   'timestamp': timestamp, 'timestamp_basis': 'record' if timestamp else 'unknown',
                   'role': 'assistant' if role == 'tool_input' else 'user' if role == 'attachment' else role,
                   'record_session_id': None, 'provenance': provenance, 'content_provenance': provenance,
                   'confidence': confidence, 'text': text, 'self': False}
            if current is not None:
                hit['current_branch'] = current
            yield hit

def export_list_row(row, coverage):
    """list_row() for an exported conversation: the conversation's own span is read whole, and its
    SHA-256 pins the span for a paging snapshot. Like a transcript's list read, it skips what it
    cannot read without counting it; `find` and `read` count it."""
    preview, last = None, None
    chatgpt = row['agent'] == 'chatgpt_export'
    for _, message, _, fields in export_message_fields(row, coverage, report=False):
        stamp = epoch_iso(message.get('create_time')) if chatgpt else message.get('created_at')
        stamp = stamp if isinstance(stamp, str) else None
        if fields and time_key(stamp) > time_key(last):
            last = stamp
        if preview is None:
            preview = next((text[:160] for role, text, _ in fields if role == 'user'), None)
    return {**public_row(row), 'timestamp': last,
            'timestamp_basis': 'latest message in the exported conversation' if last else 'unknown',
            'preview': preview, 'title': row.get('title'), 'inspected_bytes': row['bytes']}

def link_subagents(rows, coverage):
    """Mark Antigravity conversations that another conversation created. A subagent's USER_INPUT
    holds its creator's prompt, not the operator's. In the creating step's text, the first line after
    its 'Created At:' and 'Completed At:' lines is one of SUBAGENTS_CREATED, and each child follows as
    "conversationId": "<uuid>". A child whose creator is not in the inventory is not recognised."""
    marker = b'Created the following subagent'
    created = {}
    for row in rows:
        try:
            with open_session(row['path']) as f:
                for raw in f:
                    if marker not in raw:
                        continue
                    try:
                        e = json.loads(raw)
                    except (ValueError, UnicodeError):
                        continue
                    if (not isinstance(e, dict) or e.get('type') in ('USER_INPUT', 'PLANNER_RESPONSE')
                            or not isinstance(e.get('content'), str)):
                        continue
                    text = e['content']
                    while text.startswith(('Created At:', 'Completed At:')):
                        text = text.partition('\n')[2]
                    head, _, rest = text.partition('\n')
                    if head in SUBAGENTS_CREATED:
                        for child in CONVERSATION_ID.findall(rest):
                            if UUID.fullmatch(child):
                                created.setdefault(child.casefold(), row['id'])
        except OSError as exc:
            problem(coverage, 'read_error', Path(row['path']), str(exc))
    for row in rows:
        parent = created.get(row['id'].casefold())
        if parent is not None and parent.casefold() != row['id'].casefold():
            row.update(kind='subagent', parent_id=parent)

def inventory(config, coverage, guard=True):
    rows = []
    seen_paths = set()
    for source in config.get('sources', []):
        root = Path(source['path']).expanduser()
        status = {'name': source['name'], 'kind': source['kind'], 'path': str(root),
                  'status': 'searched', 'files': 0}
        coverage['sources'].append(status)
        if not root.is_dir():
            status['status'] = 'unavailable'
            problem(coverage, 'source_unavailable', root)
            continue
        if source['kind'] in ('chatgpt_project', 'antigravity') + EXPORT_KINDS:
            reader = (project_rows if source['kind'] == 'chatgpt_project' else
                      antigravity_rows if source['kind'] == 'antigravity' else export_rows)
            try:
                rows.extend(reader(root, root.resolve(), source, status, coverage, seen_paths, guard))
            except OSError as exc:
                status['status'] = 'partial'
                problem(coverage, 'read_error', root, str(exc))
            continue
        def walk_error(exc):
            status['status'] = 'partial'
            problem(coverage, 'read_error', root, str(exc))
        # Links below the root are excluded, so a file's canonical path is the resolved root
        # joined with its walked relative path; resolving once per root avoids a per-file
        # final-path lookup (the dominant inventory cost on Windows).
        resolved_root = root.resolve()
        for directory, dirs, names in os.walk(root, followlinks=False, onerror=walk_error):
            kept = []
            for d in dirs:
                p = Path(directory) / d
                if p.is_symlink() or (hasattr(p, 'is_junction') and p.is_junction()):
                    problem(coverage, 'excluded_link', p)
                else:
                    kept.append(d)
            dirs[:] = sorted(kept)
            for name in sorted(names):
                if source['kind'] == 'document':
                    eligible = Path(name).suffix.lower() in ('.md', '.json', '.jsonl', '.txt')
                else:
                    eligible = (name.endswith('.jsonl') and name != 'journal.jsonl') or (
                        name.endswith(ZSTD_SUFFIX) and name != 'journal' + ZSTD_SUFFIX and _zstd is not None)
                    if not eligible and '.jsonl.' in name and not name.startswith('journal.jsonl'):
                        # A transcript stored in a form this runtime cannot read is a coverage gap, never a silent skip.
                        problem(coverage, 'unsupported_session_file', Path(directory) / name,
                                ZSTD_UNAVAILABLE if name.endswith(ZSTD_SUFFIX) else
                                "unrecognized transcript encoding '%s'; not searched" % name[name.index('.jsonl.') + 6:])
                if not eligible:
                    continue
                p = Path(directory) / name
                if p.is_symlink():
                    problem(coverage, 'excluded_link', p)
                    continue
                resolved = str(resolved_root / p.relative_to(root))
                if resolved in seen_paths:
                    continue
                seen_paths.add(resolved)
                status['files'] += 1
                coverage['files'] += 1
                try:
                    if source['kind'] == 'document':
                        stat = p.stat()
                        row = {'id': 'doc:' + source['name'] + ':' + p.relative_to(root).as_posix(),
                               'agent': 'document', 'source': source['name'], 'kind': 'document',
                               'parent_id': None, 'path': resolved, 'bytes': stat.st_size,
                               'mtime_ns': stat.st_mtime_ns, 'timestamp': None,
                               'timestamp_basis': 'unknown', 'title': name,
                               '_guards': capture_guards(p, stat.st_size) if guard else []}
                    else:
                        row = inspect_file(p, source, coverage, guard, resolved)
                    if row:
                        rows.append(row)
                except OSError as exc:
                    problem(coverage, 'read_error', p, str(exc))
    link_subagents([row for row in rows if row['agent'] == 'antigravity'], coverage)
    return rows

def read_metadata(config, rows, coverage):
    titles, imports = {}, {}
    if config.get('title_index'):
        p = Path(config['title_index'])
        try:
            with p.open('rb') as f:
                offset = 0
                for number, raw in enumerate(f, 1):
                    try:
                        e = json.loads(raw)
                        if e.get('id') and e.get('thread_name'):
                            titles[e['id']] = {'text': e['thread_name'], 'path': str(p), 'line': number,
                                               'byte_offset': offset, 'record_sha256': hashlib.sha256(raw).hexdigest()}
                    except (ValueError, UnicodeError, AttributeError):
                        problem(coverage, 'metadata_malformed', p, 'line ' + str(number))
                    offset += len(raw)
        except OSError as exc:
            problem(coverage, 'metadata_unavailable', p, str(exc))
    if config.get('imports'):
        p = Path(config['imports'])
        try:
            data = json.loads(p.read_text(encoding='utf-8-sig'))
            imports = {r['imported_thread_id']: r for r in data.get('records', []) if r.get('imported_thread_id')}
        except (OSError, ValueError, AttributeError, TypeError) as exc:
            problem(coverage, 'metadata_unavailable', p, str(exc))
    original_ids = {r['id'] for r in rows if r['agent'] == 'claude'}
    for row in rows:
        if row['id'] in titles:
            row['title'] = titles[row['id']]['text']
        if row['id'] in imports:
            row['kind'] = 'import'
            matches = UUID.findall(imports[row['id']].get('source_path', ''))
            row['original_id'] = matches[-1] if matches else None
            row['original_available'] = row['original_id'] in original_ids
    return titles

def input_strings(value, field=('input',)):
    """Keep decoded tool argument strings separate, with their field paths."""
    if isinstance(value, str):
        yield value, json.dumps(field, ensure_ascii=False, separators=(',', ':'))
    elif isinstance(value, dict):
        for key, child in value.items():
            yield from input_strings(child, field + (str(key),))
    elif isinstance(value, list):
        for index, child in enumerate(value):
            yield from input_strings(child, field + (index,))

def blocks_from_event(e):
    """Yield only documented text-bearing fields; no arbitrary object flattening."""
    if not isinstance(e, dict):
        return
    kind = e.get('type')
    if kind in ('user', 'assistant'):
        message = e.get('message') or {}
        if isinstance(message, dict):
            role = message.get('role') or kind
            content = message.get('content', [])
        else:
            role, content = kind, message
    elif kind == 'response_item':
        message = e.get('payload') or {}
        if not isinstance(message, dict):
            raise ValueError('response_item payload must be an object')
        if message.get('type') == 'message':
            role, content = message.get('role', 'unknown'), message.get('content', [])
        elif message.get('type') in ('function_call_output', 'custom_tool_call_output'):
            role, content = 'tool', message.get('output', '')
        elif message.get('type') in ('function_call', 'custom_tool_call'):
            value = message.get('arguments', message.get('input', ''))
            if isinstance(value, str):
                try:
                    decoded = json.loads(value)
                    if isinstance(decoded, (dict, list)):
                        value = decoded
                except ValueError:
                    pass
            for text, field in input_strings(value):
                yield 'tool_input', text, field
            return
        else:
            return
    elif kind == 'event_msg':
        message = e.get('payload') or {}
        if not isinstance(message, dict):
            raise ValueError('event_msg payload must be an object')
        if message.get('type') == 'user_message':
            role, content = 'user', message.get('message', '')
        elif message.get('type') == 'agent_message':
            role, content = 'assistant', message.get('message', '')
        else:
            return
    elif kind in ('compacted', 'compaction', 'summary'):
        message = e.get('payload') or e
        if not isinstance(message, dict):
            raise ValueError('summary payload must be an object')
        role, content = 'summary', message.get('summary', message.get('message', ''))
    elif kind == 'custom-title' and isinstance(e.get('customTitle'), str):
        role, content = 'metadata', e['customTitle']
    elif kind == 'attachment':
        # A message that reached Claude Code mid-turn is a queued_command attachment, not a user
        # record: its text is in `prompt`. Only a sender recorded as human has the user role.
        attachment = e.get('attachment')
        if not isinstance(attachment, dict) or attachment.get('type') != 'queued_command':
            return
        origin = attachment.get('origin')
        human = isinstance(origin, dict) and origin.get('kind') == 'human'
        role, content = ('user' if human else 'system'), attachment.get('prompt')
    else:
        return
    if isinstance(content, str):
        yield role, content, 0
    elif isinstance(content, list):
        for index, block in enumerate(content):
            if not isinstance(block, dict):
                continue
            if block.get('type') in ('text', 'input_text', 'output_text') and isinstance(block.get('text'), str):
                yield role, block['text'], index
            elif block.get('type') == 'tool_use':
                for text, field in input_strings(block.get('input', {}), (index, 'input')):
                    yield 'tool_input', text, field
            elif block.get('type') == 'tool_result':
                value = block.get('content', '')
                if isinstance(value, str):
                    yield 'tool', value, index
                elif isinstance(value, list):
                    for sub_index, sub in enumerate(value):
                        if isinstance(sub, dict) and isinstance(sub.get('text'), str):
                            yield 'tool', sub['text'], str(index) + '.content[' + str(sub_index) + ']'

def user_request_spans(text):
    """A USER_INPUT text split at its <USER_REQUEST> tags: (True, inside) for the request, (False,
    span) for the app's text around it (<ADDITIONAL_METADATA>, <USER_SETTINGS_CHANGE>). An unclosed
    tag runs to the end. The tags themselves and empty spans are dropped."""
    spans = []
    while '<USER_REQUEST>' in text:
        before, _, text = text.partition('<USER_REQUEST>')
        inside, _, text = text.partition('</USER_REQUEST>')
        spans += [(False, before), (True, inside)]
    spans.append((False, text))
    return [(inside, span) for inside, span in spans if span]

def antigravity_argument(value, field):
    """One tool-call argument. transcript.jsonl stores each as a JSON literal ('"N:\\\\"', '5000'):
    strings are searched decoded, objects and arrays by field, other values not at all. An argument
    that does not parse (a truncated one) is searched as stored."""
    if isinstance(value, str):
        try:
            value = json.loads(value)
        except ValueError:
            pass
    for text, block in input_strings(value, field):
        yield 'tool_input', text, block

def blocks_from_antigravity(e):
    """Text-bearing fields of one Antigravity step; `thinking` is never read. Only the inside of
    <USER_REQUEST> in a USER_EXPLICIT USER_INPUT has the user role; text the app wrote has role
    'system'."""
    if not isinstance(e, dict):
        return
    kind, content = e.get('type'), e.get('content')
    if kind == 'USER_INPUT':
        if isinstance(content, str):
            explicit = e.get('source') == 'USER_EXPLICIT'
            for index, (inside, text) in enumerate(user_request_spans(content)):
                yield ('user' if inside and explicit else 'system'), text, index
    elif kind == 'PLANNER_RESPONSE':
        if isinstance(content, str):
            yield 'assistant', content, 0
        calls = e.get('tool_calls')
        for index, call in enumerate(calls if isinstance(calls, list) else []):
            args = call.get('args') if isinstance(call, dict) else None
            for key, value in (args.items() if isinstance(args, dict) else ()):
                yield from antigravity_argument(value, ('tool_calls', index, 'args', key))
    else:
        role = ('summary' if kind == 'CHECKPOINT' else 'system' if kind in ANTIGRAVITY_SYSTEM_STEPS
                else 'tool' if kind in ANTIGRAVITY_TOOL_STEPS else 'unknown')
        if isinstance(content, str):
            yield role, content, 0
        elif isinstance(e.get('error'), str):
            yield role, e['error'], '["error"]'

def event_fields(row, e):
    return blocks_from_antigravity(e) if row['agent'] == 'antigravity' else blocks_from_event(e)

def classify(row, role, text):
    if row['kind'] == 'document':
        return 'document', 'source_type'
    if row['kind'] in ('project_instructions', 'project_source'):
        return row['kind'], 'source_type'
    if row['kind'] == 'import':
        return 'import_copy', 'import_registry'
    if role == 'summary':
        return 'compaction_summary', 'record_type'
    if role == 'tool':
        return 'tool_output', 'record_type'
    if role == 'tool_input':
        return 'tool_input', 'record_type'
    if role == 'metadata':
        return 'title_metadata', 'record_type'
    if role == 'system' and row['agent'] in ('antigravity', 'claude') + EXPORT_KINDS:
        # Claude rows get this role only from a mid-turn message whose sender is not human; export
        # rows from system messages and from user messages the app hides.
        return 'injected_context', 'record_type'
    if role == 'attachment':
        # Text claude.ai extracted from a file attached to a message.
        return 'quoted_or_pasted', 'record_type'
    lower = text.lstrip().casefold()
    if '<codex_delegation>' in lower or row['kind'] in ('subagent', 'reviewer') and role == 'user':
        return 'delegated_prompt', 'metadata_or_marker'
    if any(token in lower for token in ('<memory>', '<environment_context>', '<user_instructions>',
            '<system-reminder>', 'memory_summary begins', '# agents.md instructions',
            'current-frontier (memory status head)', '## skills\n')):
        return 'injected_context', 'wrapper_heuristic'
    if role == 'user' and lower.startswith(HARNESS_OPENERS):
        # Claude Code wrote this user-role text itself: a task notification, a local command's output,
        # a message from another session. The operator's own pastes and slash commands are not listed.
        return 'injected_context', 'wrapper_heuristic'
    if role == 'user' and (lower.startswith('>') or lower.startswith('```') or lower.startswith('<external_')):
        return 'quoted_or_pasted', 'format_heuristic'
    if role == 'user':
        return 'user_message', 'role_only'
    return 'assistant' if role == 'assistant' else 'unknown', 'record_role'

def records(row, coverage, needle=None, strong=True):
    if row.get('format') in ('docx', 'xlsx'):
        yield from package_records(row, coverage, needle, strong)
        return
    if row['agent'] in EXPORT_KINDS:
        yield from export_records(row, coverage, needle, strong)
        return
    text_mode = row['kind'] == 'document' or row.get('format') == 'text'
    p = Path(row['path'])
    compressed = is_compressed(p)
    try:
        before = session_stat(p)
        boundary = row['bytes']
        expected_digest = row.get('_prefix_sha256')
        strong = strong or bool(expected_digest)
        digest = PrefixCheck(strong)
        if before.st_size < boundary or (not expected_digest and (
                not guards_match(p, row.get('_guards', [])) or
                (before.st_size == boundary and before.st_mtime_ns != row['mtime_ns']))):
            problem(coverage, 'changed_since_inventory', p)
        elif before.st_size > boundary:
            coverage['notes'].append({'kind': 'append_outside_snapshot', 'path': str(p), 'captured_bytes': boundary})
        offset = 0
        with open_session(p) as f:
            number = 0
            while offset < boundary:
                raw = f.readline(boundary - offset)
                if not raw:
                    break
                number += 1
                start = offset
                offset += len(raw)
                if not text_mode and not raw.endswith(b'\n') and offset == boundary:
                    coverage['unfinished_records'] += 1
                    coverage['notes'].append({'kind': 'unfinished_tail_excluded', 'path': str(p), 'bytes': len(raw)})
                    boundary = start
                    row['bytes'] = start
                    if not compressed:
                        row['_guards'] = capture_guards(p, start)
                    break
                digest.update(raw)
                if not raw.strip():
                    continue
                if text_mode:
                    try:
                        fields = [('document', raw.decode('utf-8-sig').rstrip('\r\n'), 0)]
                        event = {}
                    except UnicodeError as exc:
                        problem(coverage, 'encoding_error', p, str(exc))
                        continue
                else:
                    try:
                        event = json.loads(raw)
                    except (ValueError, UnicodeError):
                        unfinished = not raw.endswith(b'\n') and offset == before.st_size
                        key = 'unfinished_records' if unfinished else 'malformed_records'
                        coverage[key] += 1
                        problem(coverage, key, p, 'line ' + str(number))
                        continue
                    if not isinstance(event, dict):
                        coverage['unsupported_records'] += 1
                        problem(coverage, 'unsupported_record', p, 'line ' + str(number))
                        continue
                    try:
                        fields = list(event_fields(row, event))
                    except (ValueError, TypeError, AttributeError) as exc:
                        coverage['unsupported_records'] += 1
                        problem(coverage, 'unsupported_record', p, 'line ' + str(number) + ': ' + str(exc))
                        continue
                if needle is not None:
                    fields = [(role, text, block) for role, text, block in fields if needle in text.casefold()]
                record_digest = hashlib.sha256(raw).hexdigest() if fields else None
                for role, text, block in fields:
                    if event.get('isCompactSummary') or event.get('is_compact_summary'):
                        role = 'summary'
                    provenance, confidence = classify(row, role, text)
                    content_provenance = provenance
                    record_session_id = event.get('sessionId') if row['agent'] == 'claude' else None
                    if record_session_id and row['kind'] == 'conversation' and record_session_id != row['id']:
                        provenance, confidence = 'resumed_history', 'record_session_id'
                    if row['agent'] == 'antigravity':
                        timestamp, record_id = event.get('created_at'), event.get('step_index')
                    else:
                        timestamp = event.get('timestamp') if isinstance(event, dict) else None
                        if not timestamp and isinstance(event.get('payload'), dict):
                            timestamp = event['payload'].get('timestamp')
                        record_id = event.get('uuid') or event.get('id')
                    hit = {**row, 'line': number, 'byte_offset': start, 'block': block,
                           'record_sha256': record_digest, 'record_id': record_id,
                           'timestamp': timestamp, 'timestamp_basis': 'record' if timestamp else 'unknown',
                           'role': 'assistant' if role == 'tool_input' else role,
                           'record_session_id': record_session_id,
                           'provenance': provenance, 'content_provenance': content_provenance,
                           'confidence': confidence, 'text': text,
                           'self': row['id'] in (os.environ.get('CODEX_THREAD_ID'), os.environ.get('CLAUDE_CODE_SESSION_ID'))}
                    if row['agent'] == 'antigravity' and event.get('truncated_fields'):
                        # Those fields keep their first and last ~2 KiB here; transcript_full.jsonl has them whole.
                        hit['truncated_fields'] = event['truncated_fields']
                    yield hit
            if compressed:
                # Decode to the end of the stream (final blocks, checksum, later frames), so every
                # reader meets a corrupt block wherever it lies.
                while f.read(1024 * 1024):
                    pass
        after = session_stat(p, before)
        observed_digest = digest.value()
        if strong:
            row['_prefix_sha256'] = observed_digest
        if expected_digest and observed_digest != expected_digest:
            coverage['snapshot_invalid'] = True
            problem(coverage, 'snapshot_prefix_changed', p)
        if after.st_size < boundary or offset < boundary or (
                before.identity() != after.identity()
                and prefix_check(p, boundary, strong) != observed_digest):
            coverage['changed_files'] += 1
            problem(coverage, 'changed_during_read', p)
    except OSError as exc:
        if row.get('_prefix_sha256'):
            coverage['snapshot_invalid'] = True
        problem(coverage, 'read_error', p, str(exc))

def excerpt(hit, query='', length=800):
    out = public_row(hit)
    text = out['text']
    folded = text.casefold()
    index = folded.find(query.casefold()) if query else 0
    if index > 0 and len(folded) != len(text):
        # Map folded-string positions back to original character positions.
        folded_position = 0
        for original_position, character in enumerate(text):
            folded_position += len(character.casefold())
            if folded_position > index:
                index = original_position
                break
    start = max(0, index - length // 3)
    out['text'] = text[start:start + length]
    out['text_truncated'] = start > 0 or start + length < len(text)
    out['text_start_character'] = start
    return out

def context_for(row, line, count, coverage):
    before = deque(maxlen=count)
    selected = []
    found = False
    remaining = count
    mark = coverage_mark(coverage)
    stopped = False
    for record in records(row, coverage):
        if record['line'] == line:
            if not found:
                selected.extend(before)
            selected.append(excerpt(record))
            found = True
        elif found:
            if remaining:
                selected.append(excerpt(record))
                remaining -= 1
            else:
                stopped = True
                break
        else:
            before.append(excerpt(record))
    if (stopped and is_compressed(row['path']) and not undecodable(coverage, row['path'], mark[0])
            and not zstd_validates(row['path'])):
        problem(coverage, 'read_error', Path(row['path']), ZSTD_UNDECODABLE)
    if undecodable(coverage, row['path'], mark[0]):
        undecodable_only(coverage, mark, row['path'])
        return []
    return selected

class MatchLimit(Exception):
    pass

def scan_row(row, needle, query, strong, max_matches):
    """records() for one file with its own coverage part; at most max_matches + 1 excerpted hits."""
    coverage = {'complete': True, 'errors': [], 'notes': [], 'malformed_records': 0,
                'unfinished_records': 0, 'changed_files': 0, 'unsupported_records': 0}
    hits = []
    mark = coverage_mark(coverage)
    for hit in records(row, coverage, needle, strong=strong):
        hits.append(excerpt(hit, query))
        if len(hits) > max_matches:
            break
    if undecodable(coverage, row['path']):
        hits = []
        undecodable_only(coverage, mark, row['path'])
    updates = {key: row[key] for key in ('bytes', '_guards', '_prefix_sha256') if key in row}
    return updates, hits, coverage

_WORKER = {}

def worker_setup(needle, query, strong, max_matches):
    _WORKER.update(needle=needle, query=query, strong=strong, max_matches=max_matches)

def worker_batch(batch):
    w = _WORKER
    return [(index, *scan_row(row, w['needle'], w['query'], w['strong'], w['max_matches']))
            for index, row in batch]

def batches_for(rows, jobs):
    """Largest files first; small files grouped so each task carries useful work."""
    order = sorted(range(len(rows)), key=lambda i: rows[i]['bytes'], reverse=True)
    target = max(8 * 1024 * 1024, sum(r['bytes'] for r in rows) // (jobs * 16))
    batches, current, size = [], [], 0
    for index in order:
        current.append((index, rows[index]))
        size += rows[index]['bytes']
        if size >= target:
            batches.append(current)
            current, size = [], 0
    if current:
        batches.append(current)
    return batches

def scan_rows(rows, needle, query, strong, max_matches, jobs, coverage):
    """Run records() over every row, in worker processes when the scan is large enough.
    Results merge in inventory order, so hits and diagnostics match a serial scan."""
    results = [None] * len(rows)
    used = 1
    minimum = int(os.environ.get('XSESSION_PARALLEL_MIN_BYTES', PARALLEL_MIN_BYTES))
    if jobs > 1 and len(rows) > 1 and sum(r['bytes'] for r in rows) >= minimum:
        try:
            with ProcessPoolExecutor(max_workers=jobs, initializer=worker_setup,
                                     initargs=(needle, query, strong, max_matches)) as pool:
                for future in as_completed([pool.submit(worker_batch, batch) for batch in batches_for(rows, jobs)]):
                    for index, updates, hits, part in future.result():
                        results[index] = (updates, hits, part)
            used = jobs
        except (OSError, BrokenProcessPool) as exc:
            results = [None] * len(rows)
            coverage['notes'].append({'kind': 'parallel_scan_unavailable', 'path': '',
                                      'detail': type(exc).__name__ + ': ' + str(exc)[:300]})
    per_row = []
    for index, row in enumerate(rows):
        updates, hits, part = results[index] or scan_row(row, needle, query, strong, max_matches)
        row.update(updates)
        if not part['complete']:
            coverage['complete'] = False
        coverage['errors'].extend(part['errors'])
        coverage['notes'].extend(part['notes'])
        for key in ('malformed_records', 'unfinished_records', 'changed_files', 'unsupported_records'):
            coverage[key] += part[key]
        if part.get('snapshot_invalid'):
            coverage['snapshot_invalid'] = True
        per_row.append(hits)
    return per_row, used

def search(rows, query, coverage, titles, max_matches=10000, jobs=1, strong=True):
    hits = []
    needle = query.casefold()
    per_row, used = scan_rows(rows, needle, query, strong, max_matches, jobs, coverage)
    coverage['scan'] = {'jobs': used, 'prefix_check': 'sha256' if strong else 'crc32'}
    title_done = set()
    available_originals = {r['id'].casefold() for r in rows if r['agent'] == 'claude'}
    for row, row_hits in zip(rows, per_row):
        for hit in row_hits:
            if hit['provenance'] == 'resumed_history':
                # Same fields and key order as setting them before the excerpt.
                tail = {key: hit.pop(key) for key in ('text_truncated', 'text_start_character')}
                hit['original_available'] = hit['record_session_id'].casefold() in available_originals
                hit['availability_basis'] = 'container identity in configured source inventory; absence does not establish deletion'
                hit.update(tail)
            hits.append(hit)
            if len(hits) > max_matches:
                raise MatchLimit('more than ' + str(max_matches) + ' matches; narrow the source or query')
        title = titles.get(row['id'])
        if title and row['id'] not in title_done and needle in title['text'].casefold():
            title_done.add(row['id'])
            hits.append({**row, **title, 'role': 'metadata', 'provenance': 'title_metadata',
                         'confidence': 'metadata', 'timestamp_basis': 'unknown', 'block': 0})
            if len(hits) > max_matches:
                raise MatchLimit('more than ' + str(max_matches) + ' matches; narrow the source or query')
    ranks = {'user_message': 0, 'quoted_or_pasted': 1, 'assistant': 2, 'tool_input': 3, 'tool_output': 3,
             'document': 4, 'project_instructions': 4, 'project_source': 4, 'title_metadata': 5, 'delegated_prompt': 6,
             'compaction_summary': 7, 'injected_context': 8, 'import_copy': 9, 'resumed_history': 9, 'unknown': 10}
    def rank(hit):
        provenance = hit['provenance']
        if provenance == 'resumed_history' and not hit.get('original_available'):
            provenance = hit['content_provenance']
        return ranks.get(provenance, 10)
    hits.sort(key=lambda h: (rank(h), time_key(h.get('timestamp')), h['id'], h['path'], h['line'], str(h['block'])))
    # Collapse only byte-identical extracted text tied by the import registry, and the same message
    # field of the same conversation in another export (a later or earlier snapshot of it).
    originals = {(h['id'], h['text']): h for h in hits if h['agent'] == 'claude' and not h.get('text_truncated')}
    exported = {}
    kept = []
    for hit in hits:
        original = originals.get((hit.get('original_id'), hit['text'])) if not hit.get('text_truncated') else None
        if hit['provenance'] == 'import_copy' and original:
            original.setdefault('copies', []).append({k: hit[k] for k in ('id', 'path', 'line', 'record_sha256')})
            continue
        if hit['agent'] in EXPORT_KINDS and hit['provenance'] != 'title_metadata':
            # The message's own id names it within its conversation; without one, its line does.
            place = hit['record_id'] if hit['record_id'] is not None else hit['line']
            key = (hit['agent'], hit['id'], json.dumps(place), hit['record_sha256'], json.dumps(hit['block']))
            first = exported.get(key)
            if first is not None and (first['path'], first.get('export_offset')) != (hit['path'], hit.get('export_offset')):
                copy = {k: hit[k] for k in ('id', 'path', 'line', 'record_sha256')}
                copy['export_offset'] = hit.get('export_offset')
                if 'current_branch' in hit:
                    copy['current_branch'] = hit['current_branch']
                first.setdefault('copies', []).append(copy)
                continue
            exported.setdefault(key, hit)
        kept.append(hit)
    return kept

def list_row(row, coverage):
    """Bounded head/tail inspection; never claim full transcript validation."""
    if row['agent'] in EXPORT_KINDS:
        return export_list_row(row, coverage)
    window = 65536
    size = row['bytes']
    ranges = [(0, min(size, window))]
    if size > window:
        start = max(window, size - window)
        ranges.append((start, size - start))
    p = Path(row['path'])
    compressed = is_compressed(p)
    preview, last, title = None, None, row.get('title')
    hashes = []
    try:
        stored = p.stat() if compressed else None
        with open_session(p) as f:
            for start, length in ranges:
                if start:
                    # The byte before the window, read first so a compressed stream only seeks forward.
                    f.seek(start - 1)
                    preceding = f.read(1)
                f.seek(start)
                raw = f.read(length)
                hashes.append([start, length, hashlib.sha256(raw).hexdigest()])
                if len(raw) != length:
                    raise OSError('captured list window was truncated')
                if start:
                    # Keep a complete record when the window begins at a line boundary.
                    if preceding != b'\n':
                        raw = raw.partition(b'\n')[2]
                if raw and not raw.endswith(b'\n'):
                    raw = raw.rpartition(b'\n')[0]
                for line in raw.splitlines():
                    try:
                        e = json.loads(line)
                        if not isinstance(e, dict):
                            continue
                        if e.get('type') == 'custom-title' and isinstance(e.get('customTitle'), str):
                            title = e['customTitle']
                        fields = list(event_fields(row, e))
                        if row['agent'] == 'antigravity':
                            stamp = e.get('created_at')
                        else:
                            stamp = e.get('timestamp')
                            if not stamp and isinstance(e.get('payload'), dict):
                                stamp = e['payload'].get('timestamp')
                        if fields and time_key(stamp) > time_key(last):
                            last = stamp
                        if preview is None:
                            for role, text, _ in fields:
                                if role == 'user':
                                    preview = text[:160]
                                    break
                    except (ValueError, TypeError, AttributeError):
                        continue
            if compressed:
                # Past the tail window to the end of the stream: a corrupt final block, checksum or
                # later frame is met here by every reader alike.
                f.read(1)
        if row.get('_list_guards') and row['_list_guards'] != hashes:
            coverage['snapshot_invalid'] = True
            problem(coverage, 'snapshot_list_window_changed', p)
        elif not (stored_unchanged(p, stored) if compressed else guards_match(p, hashes)):
            problem(coverage, 'changed_during_list', p)
        row['_list_guards'] = hashes
    except OSError as exc:
        if row.get('_list_guards'):
            coverage['snapshot_invalid'] = True
        problem(coverage, 'read_error', p, str(exc))
        if undecodable(coverage, p, len(coverage['errors']) - 1):
            preview, last, title = None, None, row.get('title')
    return {**public_row(row), 'timestamp': last, 'timestamp_basis': 'latest message observed in bounded head/tail' if last else 'unknown',
            'preview': preview, 'title': title, 'inspected_bytes': sum(length for _, length in ranges)}

class StaleCursor(Exception):
    pass

def snapshot_directory(args):
    configured = args.snapshot_dir or os.environ.get('XSESSION_SNAPSHOT_DIR')
    return Path(configured).expanduser() if configured else None

def request_key(args, config, query):
    return hashlib.sha256(json.dumps([VERSION, args.command, config, query, getattr(args, 'context', 0), getattr(args, 'max_matches', None)],
                                     sort_keys=True).encode()).hexdigest()

def metadata_prefixes(config):
    prefixes = []
    for name in ('title_index', 'imports'):
        if config.get(name):
            path = Path(config[name])
            if path.is_file():
                size = path.stat().st_size
                prefixes.append({'path': str(path), 'bytes': size, '_prefix_sha256': prefix_digest(path, size)})
    return prefixes

def load_snapshot(args, config, query):
    try:
        cursor = json.loads(base64.urlsafe_b64decode(args.cursor.encode()))
        name, digest, offset = cursor['snapshot'], cursor['sha256'], cursor['offset']
        if not re.fullmatch('[0-9a-f]{32}', name) or not re.fullmatch('[0-9a-f]{64}', digest):
            raise ValueError('invalid snapshot identifier')
        if type(offset) is not int or offset < 0:
            raise ValueError('invalid cursor offset')
        slot = cursor.get('slot')
        if type(slot) is not int or not 0 <= slot < SNAPSHOT_SLOTS:
            raise StaleCursor('unsupported cursor storage format; start a new search')
    except (ValueError, KeyError, TypeError) as exc:
        raise ValueError('invalid cursor') from exc
    try:
        folder = snapshot_directory(args)
        if folder is None:
            raise StaleCursor('continuation requires the original --snapshot-dir or XSESSION_SNAPSHOT_DIR')
        with (folder / ('xsession-snapshot-%02d.json' % slot)).open('rb') as stream:
            raw = stream.read(SNAPSHOT_MAX_BYTES + 1)
        if len(raw) > SNAPSHOT_MAX_BYTES:
            raise StaleCursor('snapshot exceeds the storage limit')
        if hashlib.sha256(raw).hexdigest() != digest:
            raise StaleCursor('snapshot metadata changed')
        saved = json.loads(raw)
        if saved.get('id') != name:
            raise StaleCursor('snapshot identity changed')
        if saved['request_key'] != request_key(args, config, query):
            raise StaleCursor('query or source configuration changed')
        if time.time() - saved['created_at'] > 86400:
            raise StaleCursor('snapshot is older than 24 hours; start a new search')
    except (OSError, ValueError, KeyError, TypeError) as exc:
        raise StaleCursor('snapshot metadata unavailable or invalid') from exc
    roots = {s['name']: Path(s['path']).expanduser().resolve() for s in config['sources']}
    for row in saved['rows']:
        if row['source'] not in roots or not Path(row['path']).resolve().is_relative_to(roots[row['source']]):
            raise StaleCursor('snapshot path falls outside configured sources')
    args._cursor_offset = offset
    return saved, {'snapshot': name, 'sha256': digest, 'slot': slot}

def save_snapshot(args, config, query, rows, titles, coverage, reference, created_at, hits, metadata):
    folder = snapshot_directory(args)
    saved = {'version': VERSION, 'id': reference['snapshot'], 'request_key': request_key(args, config, query),
             'created_at': created_at, 'rows': rows, 'titles': titles, 'coverage': coverage, 'metadata': metadata,
             'matches': [{k:v for k,v in hit.items() if k not in ('text','context')} for hit in hits] if args.command == 'find' else None}
    raw = json.dumps(saved, ensure_ascii=False).encode('utf-8')
    if len(raw) > SNAPSHOT_MAX_BYTES:
        raise OSError('snapshot exceeds 4 MiB; narrow the source or query')
    folder.mkdir(parents=True, exist_ok=True)
    # Exclusive bounded slots need no mutable index or automatic deletion.
    # Even concurrent writers create at most 16 files of at most 4 MiB here.
    for slot in range(SNAPSHOT_SLOTS):
        try:
            stream = (folder / ('xsession-snapshot-%02d.json' % slot)).open('xb')
        except FileExistsError:
            continue
        with stream:
            stream.write(raw)
        reference.update(sha256=hashlib.sha256(raw).hexdigest(), slot=slot)
        return
    raise OSError('snapshot store has 16 occupied slots; operator-managed cleanup or another explicitly selected directory is required')

def encode_cursor(reference, offset):
    return base64.urlsafe_b64encode(json.dumps({**reference, 'offset': offset}).encode()).decode()

def verify_search_snapshot(rows):
    for row in rows:
        expected = row.get('_prefix_sha256')
        if expected:
            try:
                observed = (hashlib.sha256(export_span(row)).hexdigest() if row.get('agent') in EXPORT_KINDS
                            else prefix_digest(row['path'], row['bytes']))
                if observed != expected:
                    raise StaleCursor('captured source prefix changed: ' + row['path'])
            except OSError as exc:
                raise StaleCursor('captured source unavailable: ' + row['path']) from exc

def reopen_export(hit, query, captured, cache, wanted=None):
    """reopen_hit() for a saved export match. Its conversation is read once per page, from the row
    the snapshot captured, whose span digest the bytes must still match. Only the fields the page
    shows (`wanted`, by span) are kept, each as its digest and its excerpt, not its whole text."""
    key = (hit['path'], hit.get('export_offset'))
    if key not in cache:
        row = captured.get(key)
        if row is None or not row.get('_prefix_sha256'):
            raise StaleCursor('captured source unavailable: ' + hit['path'])
        part = {'complete': True, 'errors': [], 'notes': [], 'malformed_records': 0,
                'unfinished_records': 0, 'changed_files': 0, 'unsupported_records': 0}
        found = {}
        needed = None if wanted is None else wanted.get(key, set())
        for record in export_records(dict(row), part):
            place = (record['line'], json.dumps(record['block']))
            if (needed is None or place in needed) and place not in found:
                found[place] = (record['record_sha256'], excerpt({'text': record['text']}, query))
        if part.get('snapshot_invalid') or any(e['kind'] in ('read_error', 'changed_since_inventory') for e in part['errors']):
            raise StaleCursor('captured source prefix changed: ' + hit['path'])
        cache[key] = found
    kept = cache[key].get((hit['line'], json.dumps(hit['block'])))
    if kept is None:
        raise StaleCursor('matching content block unavailable')
    digest, piece = kept
    if digest != hit['record_sha256']:
        raise StaleCursor('matching record changed: ' + hit['path'])
    # The same fields, in the same order, as excerpt({**hit, 'text': <whole text>}, query).
    out = public_row({**hit, 'text': piece['text']})
    out['text_truncated'], out['text_start_character'] = piece['text_truncated'], piece['text_start_character']
    return out

def reopen_hit(hit, query, titles, captured=None, cache=None, wanted=None):
    if hit['provenance'] == 'title_metadata' and hit['id'] in titles and hit['path'] == titles[hit['id']]['path']:
        return {**hit, 'text': titles[hit['id']]['text']}
    if hit.get('format') in ('docx', 'xlsx'):
        try:
            units = package_units(hit['format'], Path(hit['path']).read_bytes())
        except (OSError, *PACKAGE_ERRORS) as exc:
            raise StaleCursor('matching package unavailable: ' + hit['path']) from exc
        if not 1 <= hit['line'] <= len(units) or hashlib.sha256(units[hit['line'] - 1].encode('utf-8')).hexdigest() != hit['record_sha256']:
            raise StaleCursor('matching record changed: ' + hit['path'])
        return excerpt({**hit, 'text': units[hit['line'] - 1]}, query)
    if hit.get('agent') in EXPORT_KINDS:
        return reopen_export(hit, query, captured or {}, {} if cache is None else cache, wanted)
    with open_session(hit['path']) as stream:
        stream.seek(hit['byte_offset'])
        raw = stream.readline()
    if hashlib.sha256(raw).hexdigest() != hit['record_sha256']:
        raise StaleCursor('matching record changed: ' + hit['path'])
    if hit['kind'] == 'document' or hit.get('format') == 'text':
        text = raw.decode('utf-8-sig').rstrip('\r\n')
    else:
        texts = [text for _,text,block in event_fields(hit, json.loads(raw)) if block == hit['block']]
        if not texts:
            raise StaleCursor('matching content block unavailable')
        text = next((text for text in texts if query.casefold() in text.casefold()), texts[0])
    return excerpt({**hit, 'text': text}, query)

def paginate(hits, args, key, result):
    offset = getattr(args, '_cursor_offset', 0)
    result.update(results=hits[offset:offset + args.limit], total_matches=len(hits),
                  truncated=offset + args.limit < len(hits), next_cursor=None, offset=offset)
    if result['truncated'] and result['pagination']['status'] in ('requested', 'ready'):
        result['next_cursor'] = encode_cursor(key, offset + args.limit)
    return True

def fit_output(args, key, result):
    def update_cursor():
        following = result['offset'] + len(result['results'])
        result['truncated'] = following < result['total_matches']
        result['next_cursor'] = (encode_cursor(key, following)
                                 if result['truncated'] and result['pagination']['status'] in ('requested', 'ready') else None)
    update_cursor()
    while len(json.dumps(result, ensure_ascii=False).encode('utf-8')) + 1 > args.max_output_bytes:
        if len(result['results']) > 1:
            result['results'].pop()
            update_cursor()
        else:
            result.update(status='output_budget_too_small', results=[])
            update_cursor()
            return False
    return True

def resolve(rows, query):
    # Container identities and inherited evidence are distinct; never alias a child.
    grouped = {}
    for row in rows:
        key = (row['agent'], row['id'].casefold())
        group = grouped.setdefault(key, {'id': row['id'], 'agent': row['agent'],
                                         'kind': row['kind'], 'segments': [], 'related_segments': []})
        group['kind'] = row['kind']
        group['segments'].append(row)
        if row.get('resumed_from'):
            ancestor = row['resumed_from']
            relation = grouped.setdefault((row['agent'], ancestor.casefold()), {
                'id': ancestor, 'agent': row['agent'], 'kind': 'inherited_history',
                'segments': [], 'related_segments': []})
            relation['related_segments'].append({**public_row(row), 'relation': 'resumed_history',
                'relation_basis': 'recorded header sessionId differs from container UUID',
                'read_identity': row['id']})
    exact = [g for g in grouped.values() if g['id'].casefold() == query.casefold()]
    selected = exact or [g for g in grouped.values() if g['id'].casefold().startswith(query.casefold())]
    for group in grouped.values():
        group['segments'].sort(key=lambda r: (time_key(r['timestamp']), r['path']))
        group['related_segments'].sort(key=lambda r: (r['id'], r['path']))
    return sorted(selected, key=lambda r: (r['agent'], r['id']))

def emit(args, result, code):
    if args.json:
        print(json.dumps(result, ensure_ascii=False))
    else:
        if args.command in ('path', 'current'):
            if code == 0 or (code == 4 and result.get('exact_path_available')):
                group = result['results'][0]
                segment = group['segments'][0]
                path = segment['path']
                style = args.path_format or os.environ.get('XSESSION_PATH_FORMAT', 'native')
                if style in ('msys', 'wsl') and re.match(r'^[A-Za-z]:', path):
                    path = ('/mnt/' if style == 'wsl' else '/') + path[0].lower() + path[2:].replace('\\', '/')
                elif style == 'windows' and re.match(r'^/(mnt/)?[A-Za-z]/', path):
                    match = re.match(r'^/(?:mnt/)?([A-Za-z])/(.*)', path)
                    path = match[1].upper() + ':\\' + match[2].replace('/', '\\')
                if args.legacy:
                    with open_session(segment['path']) as f:
                        lines = sum(1 for _ in f)
                    print('session_id: ' + group['id'])
                    print('transcript: ' + path)
                    print('lines: ' + str(lines))
                    print('size: ' + str(segment['bytes']))
                    print('project: ' + Path(segment['path']).parent.name)
                else:
                    print(path)
        else:
            for row in result.get('results', []):
                print(json.dumps(row, ensure_ascii=False))
        if 'pagination' in result:
            print(json.dumps({k: result.get(k) for k in ('pagination', 'truncated', 'total_matches', 'next_cursor')}, ensure_ascii=False), file=sys.stderr)
        if code in (1, 2, 3, 5, 6):
            print(result['status'], file=sys.stderr)
            if code == 3:
                for row in result.get('results', []):
                    print(row['id'], file=sys.stderr)
        print(json.dumps({'coverage': result['coverage']}, ensure_ascii=False), file=sys.stderr)
    return code

def parser():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--version', action='version', version=VERSION)
    subs = p.add_subparsers(dest='command', required=True)
    for mode in ('path', 'resolve', 'current', 'find', 'read', 'list', 'sources'):
        q = subs.add_parser(mode)
        if mode in ('path', 'resolve', 'find', 'read'):
            q.add_argument('query')
        if mode == 'list':
            q.add_argument('count', nargs='?', default=15, type=int)
        if mode in ('find', 'read'):
            q.add_argument('--context', type=int, default=0)
        if mode == 'find':
            q.add_argument('--max-matches', type=int, default=10000, help='bound retained matches; narrow overly broad searches')
            q.add_argument('--jobs', type=int, help='worker processes for large scans (default: CPU count, at most 8; XSESSION_JOBS)')
        if mode == 'read':
            q.add_argument('--line', type=int, required=True)
            q.add_argument('--segment', help='exact segment path returned by resolve')
            q.add_argument('--record-sha256', help='refuse a changed source record')
        if mode in ('path', 'current'):
            q.add_argument('--legacy', action='store_true', help='session-jsonl metadata output')
            q.add_argument('--path-format', choices=('native', 'windows', 'msys', 'wsl'))
        if mode in ('find', 'list'):
            q.add_argument('--limit', type=int, default=20)
            q.add_argument('--cursor')
            q.add_argument('--snapshot-dir', help='opt in to persistent paging in this caller-selected directory (16 snapshots, 4 MiB each)')
            q.add_argument('--max-output-bytes', type=int, default=262144)
        q.add_argument('--json', action='store_true')
        q.add_argument('--sources')
        q.add_argument('--source', action='append', help='search only this configured source name')
    return p

def main(argv=None):
    for stream in (sys.stdout, sys.stderr):
        if hasattr(stream, 'reconfigure'):
            stream.reconfigure(encoding='utf-8', errors='backslashreplace')
    args = parser().parse_args(argv)
    started = time.monotonic()
    try:
        query = getattr(args, 'query', None)
        if args.command == 'current':
            query = os.environ.get('CODEX_THREAD_ID') or os.environ.get('CLAUDE_CODE_SESSION_ID')
        config = config_for(args)
        if not isinstance(config, dict) or not isinstance(config.get('sources'), list):
            raise ValueError('source configuration must be an object with a sources array')
        for source in config['sources']:
            if (not isinstance(source, dict)
                    or source.get('kind') not in ('claude', 'codex', 'antigravity', 'document', 'chatgpt_project') + EXPORT_KINDS
                    or not isinstance(source.get('name'), str) or not isinstance(source.get('path'), str)):
                raise ValueError('each source needs string name/path and kind claude, codex, antigravity, document, '
                                 'chatgpt_project, chatgpt_export, or claude_export')
        if args.source:
            names = {s['name'] for s in config.get('sources', [])}
            if set(args.source) - names:
                raise ValueError('unknown source name')
            config['sources'] = [s for s in config['sources'] if s['name'] in args.source]
        coverage = initial_coverage(config)
        result = {'schema_version': 1, 'version': VERSION, 'status': 'no_match',
                  'results': [], 'coverage': coverage}
        if not query and args.command == 'current':
            result['status'] = 'current_identity_unavailable'
            return emit(args, result, 1)
        if args.command == 'current':
            identities = {value for value in (os.environ.get('CODEX_THREAD_ID'), os.environ.get('CLAUDE_CODE_SESSION_ID')) if value}
            if len(identities) > 1:
                result.update(status='ambiguous_current_identity', results=[{'id': ident} for ident in sorted(identities)])
                return emit(args, result, 3)
        native = native_locator(query) if query else None
        if native:
            result.update(status='native_required', native=native)
            coverage.update(complete=False, scope='native source not searched by this CLI')
            return emit(args, result, 5)
        if query is not None and not query.strip():
            raise ValueError('empty query')
        if not 1 <= getattr(args, 'limit', 1) <= 1000 or not 0 <= getattr(args, 'context', 0) <= 20:
            raise ValueError('limit must be 1..1000 and context 0..20')
        if not 4096 <= getattr(args, 'max_output_bytes', 4096) <= 4194304:
            raise ValueError('max-output-bytes must be 4096..4194304')
        if args.command == 'list' and not 1 <= args.count <= 1000:
            raise ValueError('count must be 1..1000')
        if args.command == 'read' and args.line < 1:
            raise ValueError('line must be positive')
        if args.command == 'find' and not 1 <= args.max_matches <= 100000:
            raise ValueError('max-matches must be 1..100000')
        if args.command == 'find':
            jobs = args.jobs if args.jobs is not None else int(os.environ.get('XSESSION_JOBS') or min(os.cpu_count() or 1, 8))
            if not 1 <= jobs <= 64:
                raise ValueError('jobs must be 1..64')
        saved = None
        created_at = time.time()
        key = {'snapshot': uuid.uuid4().hex, 'sha256': '0' * 64, 'slot': SNAPSHOT_SLOTS - 1}
        if args.command in ('find', 'list') and args.cursor:
            saved, key = load_snapshot(args, config, query)
            rows, titles = saved['rows'], saved['titles']
            metadata = saved.get('metadata', [])
            verify_search_snapshot(metadata)
            coverage = saved['coverage']
            result['coverage'] = coverage
            created_at = saved['created_at']
        else:
            rows = inventory(config, coverage, guard=args.command == 'find')
        inventory_coverage = json.loads(json.dumps(coverage))
        if args.command == 'sources':
            result.update(status='ok' if coverage['complete'] else 'partial_coverage', results=coverage['sources'])
            return emit(args, result, 0 if coverage['complete'] else 4)
        if args.command in ('find', 'list'):
            folder = snapshot_directory(args)
            result['pagination'] = {'status': 'ready' if saved else ('requested' if folder else 'disabled'),
                'detail': 'Continuation uses the explicitly selected directory.' if folder else
                          'No snapshot written. Use --snapshot-dir or XSESSION_SNAPSHOT_DIR for persistent paging.'}
            if not saved:
                titles = read_metadata(config, rows, coverage)
                metadata = metadata_prefixes(config)
                inventory_coverage = json.loads(json.dumps(coverage))
            if args.command == 'find':
                if saved and saved.get('matches') is not None:
                    verify_search_snapshot(rows)
                    hits = saved['matches']
                else:
                    hits = search(rows, query, coverage, titles, args.max_matches, jobs=jobs,
                                  strong=folder is not None)
            else:
                coverage['mode'] = 'bounded_inventory'
                coverage['scope'] = 'configured file inventory and up to 64 KiB at each end; transcript interiors not validated'
                hits = [list_row(row, coverage) for row in rows]
                hits.sort(key=lambda r: (time_key(r['timestamp']), r['id'], r['path']), reverse=True)
                args.limit = args.count
            if coverage.get('snapshot_invalid'):
                raise StaleCursor('captured source bytes changed or disappeared')
            if not paginate(hits, args, key, result):
                return emit(args, result, 3)
            captured = {(r['path'], r.get('export_offset')): r for r in rows if r.get('agent') in EXPORT_KINDS}
            if args.command == 'find' and saved and saved.get('matches') is not None:
                wanted = {}
                for hit in result['results']:
                    if hit.get('agent') in EXPORT_KINDS:
                        wanted.setdefault((hit['path'], hit.get('export_offset')), set()).add((hit['line'], json.dumps(hit['block'])))
                cache = {}
                result['results'] = [reopen_hit(hit, query, titles, captured, cache, wanted) for hit in result['results']]
            if args.command == 'find' and args.context:
                for hit in result['results']:
                    if hit['provenance'] != 'title_metadata':
                        context_coverage = initial_coverage(config)
                        target = hit
                        if hit['agent'] in EXPORT_KINDS:
                            # The context is read against the span guard the inventory captured.
                            source = captured.get((hit['path'], hit.get('export_offset')))
                            target = {**hit, '_guards': source['_guards'] if source else []}
                        hit['context'] = context_for(target, hit['line'], args.context, context_coverage)
                        matching = [r for r in hit['context'] if r['line'] == hit['line']]
                        if not matching or matching[0]['record_sha256'] != hit['record_sha256']:
                            problem(coverage, 'context_record_changed', hit['path'])
                        if not context_coverage['complete']:
                            coverage['complete'] = False
                            coverage['errors'].extend(context_coverage['errors'])
            result['elapsed_seconds'] = round(time.monotonic() - started, 3)
            result['snapshot'] = {'captured_at_unix': created_at,
                                  'scope': 'captured files and byte prefixes; subsequent files and appends are outside this search',
                                  'id': key['snapshot'] if saved else None, 'persisted': bool(saved)}
            result['status'] = 'ok' if hits else 'no_match'
            if not coverage['complete']:
                result['status'] = 'partial_coverage'
            if not fit_output(args, key, result):
                return emit(args, result, 2)
            if result['next_cursor'] and not saved:
                try:
                    save_snapshot(args, config, query, rows, titles, coverage, key, created_at, hits, metadata)
                    result['pagination']['status'] = 'ready'
                    result['snapshot'].update(id=key['snapshot'], persisted=True)
                except OSError as exc:
                    result['pagination'] = {'status': 'unavailable',
                        'detail': 'Search results retained; continuation unavailable: ' + str(exc)[:500]}
                if not fit_output(args, key, result):
                    return emit(args, result, 2)
            elif not result['truncated'] and result['pagination']['status'] == 'requested':
                result['pagination']['status'] = 'not_needed'
            result['elapsed_seconds'] = round(time.monotonic() - started, 3)
            return emit(args, result, (0 if hits else 1) if coverage['complete'] else 4)
        coverage.update(mode='identity_inventory',
            scope='configured file identities and header-observed resume relations; transcript interiors and exhaustive ancestry not scanned')
        groups = resolve(rows, query)
        result['results'] = groups
        result['exact_path_available'] = (len(groups) == 1 and len(groups[0]['segments']) == 1
                                          and groups[0]['id'].casefold() == query.casefold())
        code = 0 if groups else 1
        result['status'] = 'ok' if groups else 'no_match'
        if len(groups) > 1:
            code, result['status'] = 3, 'ambiguous'
        elif groups and not groups[0]['segments'] and groups[0]['related_segments']:
            if args.command == 'resolve':
                code, result['status'] = (0, 'related_history_available') if coverage['complete'] else (4, 'partial_coverage')
            else:
                code, result['status'] = 3, 'related_history_use_resolve'
        elif groups and args.command in ('path', 'current') and len(groups[0]['segments']) != 1:
            code, result['status'] = 3, 'multiple_segments_use_resolve'
        elif not coverage['complete']:
            code, result['status'] = 4, 'partial_coverage'
        if args.command == 'read' and (code == 0 or (code == 4 and len(groups) == 1 and groups[0]['id'].casefold() == query.casefold())):
            segments = groups[0]['segments']
            if args.segment:
                segments = [s for s in segments if s['path'] == args.segment]
                if not segments:
                    raise ValueError('segment must be one returned by resolve')
            if len(segments) != 1:
                code, result['status'] = 3, 'multiple_segments_choose_segment'
            else:
                result['results'] = context_for(segments[0], args.line, args.context, coverage)
                if args.record_sha256:
                    matched = [r for r in result['results'] if r['line'] == args.line]
                    if not matched or matched[0]['record_sha256'] != args.record_sha256:
                        result.update(status='record_changed', results=[])
                        return emit(args, result, 3)
                code = (0 if result['results'] else 1) if coverage['complete'] else 4
                result['status'] = 'ok' if code == 0 else ('partial_coverage' if code == 4 else 'no_match')
        return emit(args, result, code)
    except MatchLimit as exc:
        coverage['complete'] = False
        result.update(status='match_limit_exceeded', results=[], error=str(exc), next_cursor=None)
        return emit(args, result, 2)
    except StaleCursor as exc:
        result['coverage']['complete'] = False
        result.update(status='stale_cursor', results=[], error=str(exc))
        return emit(args, result, 3)
    except (ValueError, KeyError, TypeError) as exc:
        print('xsession: invalid input/config: ' + str(exc), file=sys.stderr)
        return 2
    except OSError as exc:
        if args.json:
            print(json.dumps({'schema_version': 1, 'version': VERSION, 'status': 'operational_error',
                              'results': [], 'error': {'type': type(exc).__name__, 'detail': str(exc)}}))
        else:
            print('xsession: ' + str(exc), file=sys.stderr)
        return 6
    except Exception as exc:
        if args.json:
            print(json.dumps({'schema_version': 1, 'version': VERSION, 'status': 'operational_error',
                              'results': [], 'error': {'type': type(exc).__name__, 'detail': str(exc)[:500]}}))
        else:
            print('xsession: operational error: ' + type(exc).__name__ + ': ' + str(exc)[:500], file=sys.stderr)
        return 6

if __name__ == '__main__':
    raise SystemExit(main())
