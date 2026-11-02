// check-dist's own rules, on small hand-made dists and strings: the tag scanner, the attribute decoding, the URL
// checks, the module and stylesheet rules, and the import walk. (The built site, and the budgets, are in
// csp.test.ts and budget.test.ts.)

import assert from 'node:assert/strict';
import { mkdirSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { test } from 'node:test';
import { checkDist, decodeAttr, DistError, importsOf, isExternalUrl, isScriptUrl, listFiles, readTags, REQUIRED } from '../tools/check-dist';
import { scratch } from './support';

const PAGE = '<!doctype html><html><head><title>t</title></head><body><p>x</p></body></html>';
const escaped = (s: string): string => s.replace(/\W/g, '\\$&');

/** A dist holding every required page (PAGE, unless overridden) and these extra files. */
function dist(name: string, files: Record<string, string> = {}, pages: Record<string, string> = {}): string {
  const dir = scratch(`check-dist-${name}`);
  const all: Record<string, string> = {};
  for (const r of REQUIRED) all[r] = pages[r] ?? PAGE;
  Object.assign(all, files);
  for (const [f, text] of Object.entries(all)) {
    mkdirSync(dirname(join(dir, f)), { recursive: true });
    writeFileSync(join(dir, f), text);
  }
  return dir;
}

/** A dist whose first required page has this body. */
function withBody(name: string, body: string, files: Record<string, string> = {}): string {
  return dist(name, files, { [REQUIRED[0]]: `<!doctype html><html><head></head><body>${body}</body></html>` });
}

test('listFiles returns nested files as sorted relative paths with forward slashes', () => {
  const files = listFiles(dist('list', { 'assets/b/z.js': '', 'assets/a.css': '' }));
  assert.deepEqual(files, [...files].sort());
  assert.ok(files.includes('assets/b/z.js') && files.includes('assets/a.css'));
});

test('importsOf resolves static, side-effect and dynamic relative imports against the module path and skips bare ones', () => {
  const src = "import a from './x.js'; export { b } from '../y.js'; import './z.js'; import('./w.js'); import c from 'bare';";
  assert.deepEqual(importsOf('assets/m/main.js', src), ['assets/m/x.js', 'assets/y.js', 'assets/m/z.js', 'assets/m/w.js']);
});

test('DistError names itself and lists every problem on its own line', () => {
  const e = new DistError(['one', 'two']);
  assert.equal(e.name, 'DistError');
  assert.equal(e.message, 'dist check failed:\n  one\n  two');
});

test('readTags reads start tags with lowercased names and attributes, and skips end tags and the doctype', () => {
  const { tags, problems } = readTags('<!DOCTYPE html><A HREF="/x" Id=y data-k=\'v w\' hidden></A><br/>');
  assert.deepEqual(problems, []);
  assert.deepEqual(tags, [
    ['a', [['href', '/x'], ['id', 'y'], ['data-k', 'v w'], ['hidden', '']]],
    ['br', []],
  ]);
});

test('readTags treats a slash as an attribute separator and a name may start with an equals sign', () => {
  const { tags, problems } = readTags('<img/src="a"/alt = "b" =x=y>');
  assert.deepEqual(problems, []);
  assert.deepEqual(tags, [['img', [['src', 'a'], ['alt', 'b'], ['=x', 'y']]]]);
});

test('readTags reports a comment, and a "<" before a non-letter, as a "<" that does not start a tag', () => {
  for (const html of ['<p><!-- c --></p>', '<p>a < b</p>', '<p>1 <2</p>', '<p></ p>']) {
    const { problems } = readTags(html);
    assert.ok(problems.some((p) => p.includes('does not start a tag')), html);
  }
});

test('readTags reports a "<" inside a tag, even inside a quoted value (the check can fail)', () => {
  assert.ok(readTags('<a title="x" <b>').problems.includes("a '<' inside a tag, at offset 0"));
  assert.deepEqual(readTags('<a title="<b>">').problems, ["a '<' inside a tag, at offset 0"]);
  assert.deepEqual(readTags('<a title="b">').problems, []);
});

test('readTags reports a tag the page does not close wherever the page ends', () => {
  for (const html of ['<a', '<a ', '<a href', '<a href=', '<a href="x', "<a href='x", '</a']) {
    const { tags, problems } = readTags(html);
    assert.deepEqual(problems, ['a tag the page does not close, at offset 0'], html);
    assert.deepEqual(tags, [], html);
  }
});

test('readTags stops at the unclosed tag and keeps the tags before it', () => {
  const { tags, problems } = readTags('<b></b><i');
  assert.deepEqual(tags, [['b', []]]);
  assert.equal(problems.length, 1);
});

test('decodeAttr decodes named, decimal and hexadecimal references, with or without the semicolon', () => {
  assert.equal(decodeAttr('a&amp;b&lt;&gt;&quot;&apos;'), 'a&b<>"\'');
  assert.equal(decodeAttr('java&colon;x&Tab;&NewLine;'), 'java:x\t\n');
  assert.equal(decodeAttr('&#106;&#x41;&#X42'), 'jAB');
  assert.equal(decodeAttr('plain & text'), 'plain & text');
});

test('decodeAttr turns a code point that is zero or past U+10FFFF into U+FFFD', () => {
  assert.equal(decodeAttr('&#0;'), '�');
  assert.equal(decodeAttr('&#x110000;'), '�');
  assert.equal(decodeAttr('&#x10FFFF;'), '\u{10ffff}');
});

test('decodeAttr returns null for an unknown named reference (the check can fail)', () => {
  assert.equal(decodeAttr('a&nbsp;b'), null);
  assert.equal(decodeAttr('&amp;&bogus;'), null);
});

test('isScriptUrl refuses javascript:, data: and vbscript: after decoding, controls and case, and an unknown reference', () => {
  for (const u of ['javascript:x', 'JaVaScRiPt:x', ' \x01java\tscript:x', 'java&#10;script:x', '&#106;avascript:x', 'data:text/html,x', 'vbscript:x', '&bogus;']) {
    assert.equal(isScriptUrl(u), true, u);
  }
  for (const u of ['/x', 'https://a.example/', 'mailto:a@b.example', 'javascript', '#javascript:x']) assert.equal(isScriptUrl(u), false, u);
});

test('isExternalUrl names a scheme, a protocol-relative or backslash prefix, and an unknown reference as another origin', () => {
  for (const u of ['https://a.example/', 'mailto:a@b', '//a.example/', '\\\\a.example', ' \thttp://a', 'ht&#x74;ps://a', '&bogus;']) {
    assert.equal(isExternalUrl(u), true, u);
  }
  for (const u of ['/x', 'x/y:z', '#a:b', '?q=a:b', '']) assert.equal(isExternalUrl(u), false, u);
});

test('a minimal dist of the required pages passes check-dist', () => {
  checkDist(dist('minimal'));
});

test('check-dist refuses a dist missing a required page (the check can fail)', () => {
  const t = scratch('check-dist-missing');
  for (const r of REQUIRED.slice(1)) {
    mkdirSync(dirname(join(t, r)), { recursive: true });
    writeFileSync(join(t, r), PAGE);
  }
  assert.throws(() => checkDist(t), new RegExp(`missing ${escaped(REQUIRED[0])}`));
});

test('check-dist refuses a script outside assets/', () => {
  assert.throws(() => checkDist(dist('js-outside', { 'x.js': 'export {};' })), /x\.js: a script outside assets\//);
});

test('check-dist refuses a stylesheet outside assets/', () => {
  assert.throws(() => checkDist(dist('css-outside', { 'x.css': 'a{}' })), /x\.css: a stylesheet outside assets\//);
});

test('check-dist accepts a data module that is one JSON value and nothing else', () => {
  checkDist(dist('data-ok', { 'assets/0a1b/data/t.js': 'export default {"a":[1,2]};\n' }));
});

test('check-dist refuses a data module that is not export default JSON (the check can fail)', () => {
  const bad: Record<string, string> = {
    'no export': 'const a = 1;\n',
    'trailing code': 'export default {};\nfetch(1);\n',
    'no trailing newline': 'export default {};',
    'not JSON': 'export default {a:1};\n',
  };
  for (const [why, text] of Object.entries(bad)) {
    assert.throws(() => checkDist(dist('data-bad', { 'assets/0a1b/data/t.js': text })), /a data module must be `export default <JSON>;` and nothing else/, why);
  }
});

test('check-dist does not scan a data module for forbidden words, since it is only JSON', () => {
  checkDist(dist('data-words', { 'assets/0a1b/data/t.js': 'export default {"text":"fetch() and eval and import(x)"};\n' }));
});

test('check-dist refuses a module that imports a file not in dist', () => {
  assert.throws(() => checkDist(dist('import-missing', { 'assets/m.js': "import './gone.js';\n" })), /assets\/m\.js: imports assets\/gone\.js, which is not in dist/);
});

test('check-dist accepts a module that imports a file in dist', () => {
  checkDist(dist('import-present', { 'assets/m.js': "import './n.js';\n", 'assets/n.js': 'export {};\n' }));
});

test('check-dist refuses a dynamic import in a module', () => {
  const files = { 'assets/m.js': 'const m = import ("./n.js");\n', 'assets/n.js': 'export {};\n' };
  assert.throws(() => checkDist(dist('dyn-import', files)), /assets\/m\.js: a dynamic import/);
});

test('check-dist names the word a forbidden script uses', () => {
  assert.throws(() => checkDist(dist('forbidden', { 'assets/m.js': 'new WebSocket(u);\n' })), /assets\/m\.js: uses WebSocket/);
});

test('check-dist refuses a stylesheet url() to another origin but accepts one on the site', () => {
  checkDist(dist('css-url-ok', { 'assets/a.css': 'a{background:url(/assets/x.png)} b{background:url( "y.png" )}' }));
  const bad = { 'assets/a.css': "a{background:url( 'https://e.example/x.png' )}" };
  assert.throws(() => checkDist(dist('css-url-bad', bad)), /assets\/a\.css: url\(https:\/\/e\.example\/x\.png\) leaves the site/);
});

test('check-dist refuses a script tag with no src, with two, or with one outside /assets/ (the check can fail)', () => {
  const files = { 'assets/a.js': 'export {};\n' };
  for (const tag of ['<script></script>', '<script src="/assets/a.js" src="/assets/a.js"></script>', '<script src="/other/a.js"></script>']) {
    assert.throws(() => checkDist(withBody('script-src', tag, files)), /a <script> without one src under \/assets\//, tag);
  }
  checkDist(withBody('script-src-ok', '<script src="/assets/a.js"></script>', files));
});

test('check-dist refuses a script whose src is not in dist, and a stylesheet link to a missing file', () => {
  assert.throws(() => checkDist(withBody('ref-script', '<script src="/assets/gone.js"></script>')), /references \/assets\/gone\.js, which is not in dist/);
  assert.throws(() => checkDist(withBody('ref-css', '<link rel="stylesheet" href="/assets/gone.css?v=1">')), /references \/assets\/gone\.css, which is not in dist/);
});

test('check-dist ignores the query and fragment when it finds the file a page references', () => {
  const files = { 'assets/a.css': 'a{}', 'assets/a.js': 'export {};\n' };
  checkDist(withBody('ref-query', '<link rel="stylesheet" href="/assets/a.css?v=1#x"><script src="/assets/a.js#y"></script>', files));
});

test('check-dist refuses an unknown character reference in a URL attribute', () => {
  assert.throws(() => checkDist(withBody('unknown-ref', '<a href="/a&nbsp;b">x</a>')), /a script URL \(or an unknown character reference\) in href on <a>/);
});

test('check-dist lets an <a> leave the site but refuses any other element that names another origin', () => {
  checkDist(withBody('a-leaves', '<a href="https://e.example/">x</a>'));
  for (const tag of ['<img src="https://e.example/x.png">', '<link rel="icon" href="//e.example/i.png">', '<form action="http://e.example/"></form>', '<object data="https://e.example/o"></object>']) {
    assert.throws(() => checkDist(withBody('leaves', tag)), /names another origin/, tag);
  }
});

test('check-dist reports a tag the page does not close, prefixed with the file', () => {
  assert.throws(() => checkDist(withBody('unclosed', '<a href="x')), new RegExp(`${escaped(REQUIRED[0])}: a tag the page does not close`));
});

test('check-dist follows modules that import each other in a cycle without looping', () => {
  const files = { 'assets/a.js': "import './b.js';\n", 'assets/b.js': "import './a.js';\nimport './c.js';\n", 'assets/c.js': 'export {};\n' };
  checkDist(withBody('cycle', '<script src="/assets/a.js"></script><script src="/assets/b.js"></script>', files));
});

test('check-dist counts a module two scripts share once, and counts nothing for a data module a page imports', () => {
  const quarter = 40 * 1024;
  const pad = (head: string): string => head + '\n//' + 'x'.repeat(quarter - head.length - 3);
  const files = {
    'assets/a.js': pad("import './s.js';\nimport './d/data/t.js';"),
    'assets/b.js': pad("import './s.js';"),
    'assets/s.js': pad('export {};'),
    'assets/d/data/t.js': 'export default 1;\n',
  };
  // a + b + s once = 120 KiB, under the 128 KiB page limit; counted twice it would be 160 KiB.
  checkDist(withBody('shared-once', '<script src="/assets/a.js"></script><script src="/assets/b.js"></script>', files));
});
