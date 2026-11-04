"""Generate conformance/vectors/*.json from Python 3.14 and xsession 2.3.0 (the oracle).
Seeded, so a rerun reproduces the same files. Strings that may hold lone surrogates are
stored as hex of their UTF-8 'surrogatepass' encoding."""
import json
import math
import random
import struct
import sys
from pathlib import Path, PurePosixPath, PureWindowsPath
import xml.etree.ElementTree as ET

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / 'python'))
import xsession  # noqa: E402  (the Python implementation is the oracle; shipped vectors came from 2.3.0)

OUT = ROOT / 'conformance' / 'vectors'
OUT.mkdir(parents=True, exist_ok=True)
rng = random.Random(20261010)


def hx(s):
    return s.encode('utf-8', 'surrogatepass').hex()


ONLY = set(sys.argv[1:])  # optional: vector stems to write, e.g. pathlib_posix


def save(name, data):
    if ONLY and name.rsplit('.', 1)[0] not in ONLY:
        return
    (OUT / name).write_text(json.dumps(data, ensure_ascii=True, indent=0), encoding='utf-8')
    print(name, len(data))


# ------------------------------------------------------------------------------------ JSON ---
base_docs = ['{"a": [1, 2.5, null, true, false, "x\\u00e9\\ud83d\\ude00"], "b": {"c": "d"}}', '[1, -0, 0.5e-3, 1E+2]',
             '{"type":"user","message":{"role":"user","content":[{"type":"text","text":"hi"}]}}', '"\\ud83d"',
             '{"k": NaN, "j": -Infinity, "i": Infinity}', '{"a":1,"a":2}', '  [ ]  ', '{}', '123456789012345678901234567890',
             '"tab\\tnew\\nline\\\\slash\\/"', '{"\\u00e9": "\\u0000"}', '[[[[[[]]]]]]', '-1.5e-310', '1e400']
palette = list('{}[]":,\\ \t\n\r0123456789.eE+-truefalsnulNaIy') + ['\\u', '\\ud83d', '\\ude00', 'é', '\x01', '\x7f',
                                                                    '\ud800', '﻿', 'NaN', 'Infinity', '"x"', '1e5']
json_cases = []
for doc in base_docs:
    json_cases.append(doc)
for _ in range(1500):
    s = list(rng.choice(base_docs))
    for _ in range(rng.randint(1, 3)):
        op = rng.random()
        pos = rng.randint(0, len(s))
        if op < 0.4 and s:
            del s[min(pos, len(s) - 1)]
        elif op < 0.8:
            s.insert(pos, rng.choice(palette))
        elif s:
            s[min(pos, len(s) - 1)] = rng.choice(palette)
    json_cases.append(''.join(s))
json_vectors = []
for s in json_cases:
    try:
        v = json.loads(s)
        json_vectors.append({'input': hx(s), 'ok': json.dumps(v, ensure_ascii=True), 'error': None})
    except Exception as exc:
        json_vectors.append({'input': hx(s), 'ok': None, 'error': str(exc)})
save('json_str.json', json_vectors)

bytes_vectors = []
byte_cases = [b'\xef\xbb\xbf{"a":1}', b'{"a":"\xed\xa0\xbd"}', b'{"a":"\xff"}', '[1]'.encode('utf-16-le'),
              '[1]'.encode('utf-16'), '[1]'.encode('utf-32'), '"é"'.encode('utf-16-be'), b'5\x00\x00\x00', b'\x00\x00\x00[',
              b'{"a":"\xed\xa0"}', b'\xef\xbb\xbf\xef\xbb\xbf[]']
for _ in range(400):
    raw = bytearray(rng.choice(json_cases).encode('utf-8', 'surrogatepass'))
    for _ in range(rng.randint(0, 2)):
        if raw:
            raw[rng.randrange(len(raw))] = rng.choice([0x00, 0xed, 0xa0, 0xbd, 0xff, 0xc3, 0x80, 0x22, 0x5c])
    byte_cases.append(bytes(raw))
for b in byte_cases:
    try:
        v = json.loads(b)
        bytes_vectors.append({'input': b.hex(), 'ok': json.dumps(v, ensure_ascii=True)})
    except Exception:
        bytes_vectors.append({'input': b.hex(), 'ok': None})
save('json_bytes.json', bytes_vectors)

# -------------------------------------------------------------------------------- floats ---
floats = [0.0, -0.0, 1.0, 1.5, 0.1, 1e16, 1e-5, 1e-4, 9999999999999998.0, 1e22, 2.5e-310, 5e-324, 1.7976931348623157e308,
          123456789012345680.0, 1e15, 1e17, 0.001, 3.14159, float(2 ** 53), float(2 ** 63), 1 / 3, 21564461478380.3125, 0.5, 2.5, 1e23, 5e-324 * 3]
for _ in range(3000):
    bits = rng.getrandbits(64)
    f = struct.unpack('<d', struct.pack('<Q', bits))[0]
    if math.isfinite(f):
        floats.append(f)
for _ in range(1000):
    floats.append(round(rng.uniform(-1e6, 1e6), rng.randint(0, 9)))
save('floats.json', [{'bits': struct.unpack('<Q', struct.pack('<d', f))[0], 'repr': repr(f), 'json': json.dumps(f)}
                     for f in floats])

# ------------------------------------------------------------------------------ time_key ---
stamps = ['2026-10-10T18:04:23.123Z', '2026-10-10T18:04:23Z', '2026-10-10T18:04:23+00:00', '2026-10-10T18:04:23-07:00',
          '2026-10-10 18:04:23.123456+05:30', '2026-10-10T18:04:23,5Z', '2026-10-10T18:04:23.1234567Z', '2026-10-10T18:04:23',
          '2026-10-10', '20261010T180423Z', '2026-W41-5T12:00Z', '2026W415T12:00Z', '2026-W41T12:00Z', '2026-W53-1T00:00Z',
          '2020-W53-1T00:00Z', '2026-02-30T00:00Z', '2026-12-31T24:00:00Z', '2026-12-31T24:00:01Z', '2026-10-10T23:59:60Z',
          '0001-01-01T00:00:00+00:01', '9999-12-31T23:59:59.999999-23:59', '2026-10-10T18:04:23+05', '2026-10-10T18:04:23+0530',
          '2026-10-10T18:04:23+05:30:15', '2026-10-10T18:04:23+05:30:15.5', '2026-10-10T18:04:23+00:00:00.5', '2026-10-10T18z',
          '2026-10-10T1804Z', '2026-10-10T18:04Z', '2026-10-10T18Z', '2026-10-10T1230456+00:00', '2026-10-10T', '2026-10-10TZ',
          '2026-10-10\ud800T12:00Z', '2026-10-10éT12:00Z', '2026-10-10T12:00Zjunk', '2026-10-10T12:00+24:00',
          '2026-00-00T24:00Z', '2026-10-10T12:00:00.Z', 'abcdefghij', '', '2026-10', '2026-1-1T00:00Z', ' 2026-10-10T00:00Z',
          '2026-10-10T00:00:00.000000001Z', '1970-01-01T00:00:00Z', '1969-12-31T23:59:59.999999Z', '2026-10-10T18:04:23+23:59:59.999999']
stamp_palette = list('0123456789-:T Z+.,W') + ['é', '\ud800', 'z']
for _ in range(2500):
    s = list(rng.choice(stamps[:30]))
    for _ in range(rng.randint(1, 2)):
        pos = rng.randint(0, len(s))
        op = rng.random()
        if op < 0.35 and s:
            del s[min(pos, len(s) - 1)]
        elif op < 0.7:
            s.insert(pos, rng.choice(stamp_palette))
        elif s:
            s[min(pos, len(s) - 1)] = rng.choice(stamp_palette)
    stamps.append(''.join(s))
save('time_key.json', [{'input': hx(s), 'key': repr(xsession.time_key(s))} for s in stamps])

# ------------------------------------------------------------------------------- excerpt ---
excerpts = []
pieces = ['needle', 'NEEDLE', 'ß', 'ﬁ', 'İ', 'x', ' ', 'café', '\U0001F600', '\ud83d', 'Straße']
for _ in range(800):
    text = ''.join(rng.choice(pieces) for _ in range(rng.randint(0, 600)))
    query = rng.choice(['needle', 'ss', 'strasse', 'ß', 'fi', 'i̇', 'zz', ''])
    out = xsession.excerpt({'text': text, '_private': 1, 'id': 'x'}, query)
    excerpts.append({'text': hx(text), 'query': hx(query), 'out_text': hx(out['text']),
                     'truncated': out['text_truncated'], 'start': out['text_start_character']})
save('excerpt.json', excerpts)

# ------------------------------------------------------------------------------ pathlib ---
parts = ['C:', 'c:', 'D:', '\\', '/', '//', 'a', 'b', '.', '..', 'x.jsonl', 'y.', '.z', 'server', 'share', '?', 'UNC', 'A:b', ' ', '~']
path_cases = []
for _ in range(1500):
    s = ''.join(rng.choice(parts) for _ in range(rng.randint(0, 6)))
    t = ''.join(rng.choice(parts) for _ in range(rng.randint(0, 3)))
    path_cases.append((s, t))
# The posix set can be generated on Windows (same Python 3.14 oracle as the Windows set).
posix = sys.platform != 'win32' or 'pathlib_posix' in ONLY
flavour = PurePosixPath if posix else PureWindowsPath
vectors = []
for s, t in path_cases:
    p = flavour(s)
    vectors.append({'s': s, 't': t, 'str': str(p), 'name': p.name, 'suffix': p.suffix, 'stem': p.stem,
                    'parts': list(p.parts), 'parent': str(p.parent), 'join': str(p / t),
                    'relative_to_parent': p.is_relative_to(p.parent)})
save('pathlib_%s.json' % ('posix' if posix else 'windows'), vectors)

# --------------------------------------------------------------------------- utf-8-sig ---
utf8 = []
for _ in range(1500):
    b = bytes(rng.choice([0x41, 0x0a, 0xc3, 0xa9, 0xed, 0xa0, 0x80, 0xbf, 0xf0, 0x9f, 0x98, 0xff, 0xef, 0xbb, 0xe0, 0xf4, 0x90, 0xc0])
              for _ in range(rng.randint(0, 8)))
    try:
        utf8.append({'input': b.hex(), 'ok': hx(b.decode('utf-8-sig')), 'error': None})
    except UnicodeDecodeError as exc:
        utf8.append({'input': b.hex(), 'ok': None, 'error': str(exc)})
save('utf8_sig.json', utf8)

# ----------------------------------------------------------------------------------- XML ---
W = 'http://schemas.openxmlformats.org/wordprocessingml/2006/main'
xml_docs = [b'<w:document xmlns:w="%s"><w:body><w:p><w:r><w:t>Intro &amp; more</w:t></w:r><w:r><w:t xml:space="preserve"> tail</w:t></w:r></w:p></w:body></w:document>' % W.encode(),
            b'<?xml version="1.0" encoding="UTF-8" standalone="yes"?>\r\n<sst xmlns="u" count="2"><si><t>a</t></si><si><r><t>b</t></r><r><t>c</t></r></si></sst>',
            b'<a b="1" c=\'2\'><!-- x --><b>t<![CDATA[<x>]]>u</b>tail<c/></a>']
xml_cases = [d for d in xml_docs]
xml_palette = [b'<', b'>', b'/', b'&', b';', b'"', b"'", b'=', b' ', b'\n', b'\r', b'x', b':', b'!', b'?', b'#', b'&#65;',
               b'&lt;', b'\xc3\xa9', b'\xff', b']]>', b'<![CDATA[', b'-->', b'<!--']
for _ in range(1500):
    s = bytearray(rng.choice(xml_docs))
    for _ in range(rng.randint(1, 2)):
        pos = rng.randint(0, len(s))
        op = rng.random()
        if op < 0.4 and s:
            del s[min(pos, len(s) - 1)]
        elif op < 0.8:
            s[pos:pos] = rng.choice(xml_palette)
        elif s:
            s[min(pos, len(s) - 1):min(pos, len(s) - 1) + 1] = rng.choice(xml_palette)
    xml_cases.append(bytes(s))
xml_vectors = []
for data in xml_cases:
    try:
        root = ET.fromstring(data)
        tree = [[e.tag, sorted([list(kv) for kv in e.attrib.items()]), e.text] for e in root.iter()]
        xml_vectors.append({'input': data.hex(), 'tree': tree, 'error': None})
    except Exception as exc:
        xml_vectors.append({'input': data.hex(), 'tree': None, 'error': type(exc).__name__ + ': ' + str(exc)})
save('xml.json', xml_vectors)
