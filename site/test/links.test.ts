// The link checker over the built HTML.
//
// Catches: a page that links to a page, asset or #fragment that is not in the build: a renamed route, a deleted
// heading or id, a typo, a link spelled '/x' where the router serves only '/x/', an asset id that went stale. A
// broken link on a deployed site is a 404 for a visitor and nothing in the build would say so.

import assert from 'node:assert/strict';
import { test } from 'node:test';
import { checkLinks, fileOf } from '../tools/links';
import { mkdirSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { buildReal, copyTree, damage, scratch } from './support';

const dist = buildReal('links');

test('every internal link, asset and #fragment of the built site resolves', () => {
  assert.deepEqual(checkLinks(dist), []);
});

test('the checker has something to check: the router resolves the routes the pages name', () => {
  const have = new Set(['index.html', 'install/index.html']);
  assert.equal(fileOf('/', have), 'index.html');
  assert.equal(fileOf('/install/', have), 'install/index.html');
  assert.equal(fileOf('/install', have), null, 'the router does not serve /x for /x/index.html');
  assert.equal(fileOf('/nope/', have), null);
});

test('a link to a page that is not built is reported', () => {
  const t = copyTree(dist, 'links-page');
  damage(t, 'index.html', 'href="/install/"', 'href="/installs/"');
  const problems = checkLinks(t);
  assert.equal(problems.length > 0, true);
  assert.match(problems.join('\n'), /index\.html: <a href="\/installs\/"> names no file/);
});

test('a link without the trailing slash is reported', () => {
  const t = copyTree(dist, 'links-slash');
  damage(t, 'index.html', 'href="/install/"', 'href="/install"');
  assert.match(checkLinks(t).join('\n'), /names no file/);
});

test('a #fragment whose heading was deleted is reported', () => {
  const t = copyTree(dist, 'links-fragment');
  damage(t, 'index.html', 'id="status"', 'id="state"');
  assert.match(checkLinks(t).join('\n'), /names no id #status on index\.html/);
  const u = copyTree(dist, 'links-fragment-concepts');
  damage(u, 'concepts/index.html', 'id="placement"', 'id="place"');
  assert.match(checkLinks(u).join('\n'), /names no id #placement on concepts\/index\.html/);
});

test('a stylesheet or script at an asset id that is not built is reported', () => {
  const t = copyTree(dist, 'links-asset');
  damage(t, 'install/index.html', /\/assets\/[0-9a-f]+\/theme\.css/, '/assets/0000000000000000/theme.css');
  assert.match(checkLinks(t).join('\n'), /install\/index\.html: <link href="\/assets\/0+\/theme\.css"> names no file/);
});

test('fileOf resolves a directory path to its index.html and a plain path to itself, and nothing else', () => {
  const have = new Set(['index.html', 'a/index.html', 'a.css', 'b/c.js']);
  assert.equal(fileOf('/', have), 'index.html');
  assert.equal(fileOf('/a/', have), 'a/index.html');
  assert.equal(fileOf('/a.css', have), 'a.css');
  assert.equal(fileOf('/b/../a/', have), 'a/index.html');
  assert.equal(fileOf('/b/c.js', have), 'b/c.js');
  assert.equal(fileOf('/a', have), null);
  assert.equal(fileOf('/missing/', have), null);
});

test('a fragment-only link resolves against its own page, and a relative link against its own directory', () => {
  const dir = scratch('links-relative');
  mkdirSync(join(dir, 'a'));
  mkdirSync(join(dir, 'b'));
  writeFileSync(join(dir, 'a', 'index.html'), '<!doctype html>\n<h2 id="top">t</h2><a href="#top">up</a><a href="#nope">x</a><a href="../b/?q=1#bid">b</a><a href="">self</a>\n');
  writeFileSync(join(dir, 'b', 'index.html'), '<!doctype html>\n<h2 id="bid">b</h2>\n');
  assert.deepEqual(checkLinks(dir), ['a/index.html: <a href="#nope"> names no id #nope on a/index.html']);
});
