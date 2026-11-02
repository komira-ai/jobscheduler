// rewriteLinks: the page copy names pages as '<source>.html'; the built pages name the routes they are served at.

import assert from 'node:assert/strict';
import { test } from 'node:test';
import { PAGES, rewriteLinks } from '../tools/pages';

test('a link to a page by its source file becomes the route, with or without a #fragment', () => {
  assert.equal(rewriteLinks('<a href="install.html">i</a>', PAGES), '<a href="/install/">i</a>');
  assert.equal(rewriteLinks('<a href="concepts.html#leases">c</a>', PAGES), '<a href="/concepts/#leases">c</a>');
});

test('a link to an .html file that is no page is an error (the check can fail)', () => {
  assert.throws(() => rewriteLinks('<a href="nope.html">x</a>', PAGES), /a link to nope\.html, which is no page/);
});

test('links that are not bare page names are left as they are', () => {
  const html = '<a href="/install/">a</a><a href="https://x.example/y.html">b</a><a href="#top">c</a>';
  assert.equal(rewriteLinks(html, PAGES), html);
});
