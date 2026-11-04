"""2.3 ChatGPT project-store reader: manifest verification, extraction and coverage."""
import hashlib
import io
import json
import os
from pathlib import Path
import unittest
import zipfile

import test_xsession as base
import xsession

PROJECT = 'g-p-0123456789abcdef0123456789abcdef'
W = 'http://schemas.openxmlformats.org/wordprocessingml/2006/main'
S = 'http://schemas.openxmlformats.org/spreadsheetml/2006/main'


def docx(paragraphs):
    body = ''.join('<w:p><w:r><w:t>%s</w:t></w:r><w:r><w:t xml:space="preserve"> tail</w:t></w:r></w:p>' % p
                   for p in paragraphs)
    data = io.BytesIO()
    with zipfile.ZipFile(data, 'w') as z:
        z.writestr('[Content_Types].xml', '<Types/>')
        z.writestr('word/document.xml', '<w:document xmlns:w="%s"><w:body>%s</w:body></w:document>' % (W, body))
    return data.getvalue()


def xlsx(shared, inline):
    data = io.BytesIO()
    with zipfile.ZipFile(data, 'w') as z:
        z.writestr('xl/sharedStrings.xml', '<sst xmlns="%s">%s</sst>' % (
            S, ''.join('<si><r><t>%s</t></r><r><t>!</t></r></si>' % s for s in shared)))
        z.writestr('xl/worksheets/sheet1.xml', '<worksheet xmlns="%s"><sheetData><row>%s</row></sheetData></worksheet>' % (
            S, ''.join('<c t="inlineStr"><is><t>%s</t></is></c>' % s for s in inline)))
    return data.getvalue()


class ChatGPTProjectTests(unittest.TestCase):
    setUp_base = base.RetrievalTests.setUp
    tearDown = base.RetrievalTests.tearDown
    save_config = base.RetrievalTests.save_config
    run_cli = base.RetrievalTests.run_cli

    def setUp(self):
        self.setUp_base()
        self.store = self.root / 'chatgpt'
        self.sources = {'sources': [{'name': 'chatgpt-projects', 'kind': 'chatgpt_project', 'path': str(self.store)}]}
        self.cfg.write_text(json.dumps(self.sources), encoding='utf-8')
        files = {'notes.md': 'line one\nThe needle in notes\n'.encode(),
                 'plan.docx': docx(['Intro', 'needle paragraph', 'Outro']),
                 'tracker.xlsx': xlsx(['needle shared', 'other'], ['inline needle']),
                 'logo.png': b'\x89PNG\r\n\x1a\nnot text needle'}
        src = self.store / PROJECT / 'sources'
        src.mkdir(parents=True)
        for name, data in files.items():
            (src / name).write_bytes(data)
        (self.store / PROJECT / 'AGENTS.md').write_text('Project instructions mention the needle.\n', encoding='utf-8')
        self.manifest = {'version': 1, 'files': [{'fileId': 'file-%d' % i, 'name': name,
                                                  'sha256': hashlib.sha256(data).hexdigest()}
                                                 for i, (name, data) in enumerate(files.items())]}
        self.write_manifest()

    def write_manifest(self):
        meta = self.store / '.metadata'
        meta.mkdir(exist_ok=True)
        (meta / (PROJECT + '.json')).write_text(json.dumps(self.manifest), encoding='utf-8')

    def find(self, *extra):
        r = self.run_cli('find', 'needle', '--json', '--limit', '50', *extra)
        return r, json.loads(r.stdout)

    def test_verified_project_files_are_searched_with_provenance(self):
        r, out = self.find()
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        by_title = {}
        for hit in out['results']:
            by_title.setdefault(hit['title'], []).append(hit)
        self.assertEqual(by_title['AGENTS.md'][0]['provenance'], 'project_instructions')
        self.assertEqual(by_title['AGENTS.md'][0]['manifest'], 'instructions_not_manifested')
        self.assertEqual(by_title['notes.md'][0]['provenance'], 'project_source')
        self.assertEqual(by_title['notes.md'][0]['line'], 2)
        self.assertEqual([h['text'] for h in by_title['plan.docx']], ['needle paragraph tail'])
        self.assertEqual(sorted(h['text'] for h in by_title['tracker.xlsx']), ['inline needle', 'needle shared!'])
        for name in ('notes.md', 'plan.docx', 'tracker.xlsx'):
            self.assertEqual({h['manifest'] for h in by_title[name]}, {'verified'})
            self.assertEqual({h['agent'] for h in by_title[name]}, {'chatgpt_project'})
            self.assertEqual({h['parent_id'] for h in by_title[name]}, {PROJECT})
        self.assertNotIn('logo.png', by_title)
        notes = [n for n in out['coverage']['notes'] if n['kind'] == 'not_text_searchable']
        self.assertEqual([Path(n['path']).name for n in notes], ['logo.png'])
        self.assertTrue(out['coverage']['complete'])

    def test_hash_mismatch_is_disclosed_on_hits_and_in_coverage(self):
        self.manifest['files'][0]['sha256'] = '0' * 64
        self.write_manifest()
        r, out = self.find()
        self.assertEqual(r.returncode, 0, r.stderr)
        notes = [h for h in out['results'] if h['title'] == 'notes.md']
        self.assertEqual({h['manifest'] for h in notes}, {'hash_mismatch'})
        self.assertIn('project_manifest_mismatch', [n['kind'] for n in out['coverage']['notes']])

    def test_listed_file_missing_makes_coverage_partial(self):
        self.manifest['files'].append({'fileId': 'gone', 'name': 'gone.md', 'sha256': '1' * 64})
        self.write_manifest()
        r, out = self.find()
        self.assertEqual(r.returncode, 4, r.stderr)
        self.assertIn('project_file_missing', [e['kind'] for e in out['coverage']['errors']])
        self.assertTrue(out['results'])

    def test_unknown_manifest_schema_fails_closed_for_verification(self):
        self.manifest['version'] = 2
        self.write_manifest()
        r, out = self.find()
        self.assertEqual(r.returncode, 4, r.stderr)
        self.assertIn('project_manifest_unsupported', [e['kind'] for e in out['coverage']['errors']])
        self.assertEqual({h['manifest'] for h in out['results'] if h['title'] != 'AGENTS.md'}, {'unverified'})

    def test_corrupt_package_is_reported_not_crashed(self):
        (self.store / PROJECT / 'sources' / 'plan.docx').write_bytes(b'not a zip')
        self.manifest['files'][1]['sha256'] = hashlib.sha256(b'not a zip').hexdigest()
        self.write_manifest()
        r, out = self.find()
        self.assertEqual(r.returncode, 4, r.stderr)
        self.assertIn('project_file_unreadable', [e['kind'] for e in out['coverage']['errors']])

    def test_extracted_unit_reopens_by_line_and_digest(self):
        r, out = self.find()
        hit = next(h for h in out['results'] if h['title'] == 'plan.docx')
        opened = self.run_cli('read', hit['id'], '--line', str(hit['line']), '--record-sha256', hit['record_sha256'], '--json')
        self.assertEqual(opened.returncode, 0, opened.stdout + opened.stderr)
        self.assertEqual(json.loads(opened.stdout)['results'][0]['text'], 'needle paragraph tail')
        changed = self.run_cli('read', hit['id'], '--line', str(hit['line']), '--record-sha256', '0' * 64, '--json')
        self.assertEqual(changed.returncode, 3, changed.stderr)

    def test_paging_reopens_extracted_units(self):
        first = json.loads(self.run_cli('find', 'needle', '--json', '--limit', '2').stdout)
        self.assertTrue(first['next_cursor'])
        later = json.loads(self.run_cli('find', 'needle', '--json', '--limit', '2', '--cursor', first['next_cursor']).stdout)
        self.assertEqual(later['status'], 'ok')
        self.assertTrue(all('needle' in h['text'].casefold() for h in later['results']))

    def test_default_sources_include_the_store(self):
        env = dict(self.env)
        env.pop('XSESSION_CONFIG')
        env['CHATGPT_PROJECTS_DIR'] = str(self.store)
        self.env = env
        r = self.run_cli('sources', '--json')
        names = {s['name']: s for s in json.loads(r.stdout)['results']}
        self.assertEqual(names['chatgpt-projects']['kind'], 'chatgpt_project')
        self.assertEqual(names['chatgpt-projects']['files'], 5)


if __name__ == '__main__':
    unittest.main()
