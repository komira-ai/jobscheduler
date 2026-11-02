// The resource budgets check-dist enforces: one JavaScript module, one stylesheet, and what one page loads together.
//
// Catches: a budget that is off by one (a file exactly at its limit refused, or one byte over accepted), a module that
// several of a page's modules import counted more than once (or not at all) in the page's JavaScript, and a page's CSS
// that counts only one of the stylesheets it links. The real site's assets are far below every limit, so without these
// tests nothing would show that a limit rejects anything. Each over-the-limit test is the "(the check can fail)" of the
// exact-limit test beside it: it goes red if the limit check is removed.

import assert from 'node:assert/strict';
import { readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { test } from 'node:test';
import { checkDist, JS_MAX_BYTES, PAGE_CSS_MAX_BYTES, PAGE_JS_MAX_BYTES, SITE_CSS_MAX_BYTES, THEME_CSS_MAX_BYTES } from '../tools/check-dist';
import { assetDir, buildReal, copyTree } from './support';

const dist = buildReal('budget');

/** `head` followed by comment filler, exactly `bytes` bytes long (all ASCII). */
function sized(head: string, bytes: number, comment: 'js' | 'css'): string {
  const [open, close] = comment === 'js' ? ['\n//', ''] : ['\n/*', '*/'];
  const fill = bytes - head.length - open.length - close.length;
  assert.ok(fill >= 0, `${head.length} bytes of head do not fit in ${bytes}`);
  return head + open + 'x'.repeat(fill) + close;
}

/** index.html with the stylesheets and scripts it loads replaced by exactly these (paths under the asset directory). */
function loading(t: string, css: string[], js: string[]): void {
  const id = assetDir(t);
  const page = join(t, 'index.html');
  const html = readFileSync(page, 'utf8');
  const tags = [
    ...css.map((c) => `<link rel="stylesheet" href="/${id}/${c}">`),
    ...js.map((s) => `<script type="module" src="/${id}/${s}"></script>`),
  ].join('');
  const next = html.replace(/<link rel="stylesheet"[^>]*>/g, '').replace(/<script\b[^>]*><\/script>/g, '').replace('</head>', `${tags}</head>`);
  assert.notEqual(next, html);
  writeFileSync(page, next);
}

function put(t: string, file: string, text: string): void {
  writeFileSync(join(t, assetDir(t), file), text);
}

/** A copy of the site whose index.html loads only these stylesheets, each of the given size. */
function withCss(name: string, sheets: Record<string, number>): string {
  const t = copyTree(dist, name);
  for (const [file, bytes] of Object.entries(sheets)) put(t, file, sized('a{}', bytes, 'css'));
  loading(t, Object.keys(sheets), []);
  return t;
}

/** A copy of the site whose index.html loads only the first module given (name -> [imports, bytes]). */
function withJs(name: string, modules: Record<string, [string[], number]>): string {
  const t = copyTree(dist, name);
  for (const [file, [imports, bytes]] of Object.entries(modules)) {
    put(t, file, sized(imports.map((i) => `import './${i}';`).join('\n'), bytes, 'js'));
  }
  loading(t, [], [Object.keys(modules)[0]]);
  return t;
}

test('a JavaScript module of exactly the per-module limit passes check-dist', () => {
  checkDist(withJs('budget-js-exact', { 'entry.js': [[], JS_MAX_BYTES] }));
});

test('a JavaScript module one byte over the per-module limit is refused (the check can fail)', () => {
  const t = withJs('budget-js-over', { 'entry.js': [[], JS_MAX_BYTES + 1] });
  assert.throws(() => checkDist(t), new RegExp(`entry\\.js is ${JS_MAX_BYTES + 1} bytes; the limit for one module is ${JS_MAX_BYTES}`));
});

test('the theme stylesheet of exactly its limit passes and one byte over is refused (the check can fail)', () => {
  checkDist(withCss('budget-theme-exact', { 'theme.css': THEME_CSS_MAX_BYTES }));
  const t = withCss('budget-theme-over', { 'theme.css': THEME_CSS_MAX_BYTES + 1 });
  assert.throws(() => checkDist(t), new RegExp(`theme\\.css is ${THEME_CSS_MAX_BYTES + 1} bytes; the limit for theme\\.css is ${THEME_CSS_MAX_BYTES}`));
});

test('the site stylesheet of exactly its limit passes and one byte over is refused (the check can fail)', () => {
  checkDist(withCss('budget-site-exact', { 'site.css': SITE_CSS_MAX_BYTES }));
  const t = withCss('budget-site-over', { 'site.css': SITE_CSS_MAX_BYTES + 1 });
  assert.throws(() => checkDist(t), new RegExp(`site\\.css is ${SITE_CSS_MAX_BYTES + 1} bytes; the limit for site\\.css is ${SITE_CSS_MAX_BYTES}`));
});

test('a page linking stylesheets that total exactly the page CSS limit passes check-dist', () => {
  assert.equal(THEME_CSS_MAX_BYTES + SITE_CSS_MAX_BYTES, PAGE_CSS_MAX_BYTES);
  checkDist(withCss('budget-css-exact', { 'theme.css': THEME_CSS_MAX_BYTES, 'site.css': SITE_CSS_MAX_BYTES }));
});

test('a page linking stylesheets one byte over the page CSS limit is refused though each is within its own (the check can fail)', () => {
  const t = withCss('budget-css-over', { 'theme.css': THEME_CSS_MAX_BYTES, 'site.css': SITE_CSS_MAX_BYTES - 7, 'extra.css': 8 });
  assert.throws(() => checkDist(t), new RegExp(`index\\.html links ${PAGE_CSS_MAX_BYTES + 1} bytes of CSS; the limit for one page is ${PAGE_CSS_MAX_BYTES}`));
});

test('a page counts the CSS of every stylesheet it links, and only those it links (the check can fail)', () => {
  const third = Math.floor(PAGE_CSS_MAX_BYTES / 3) + 1; // each within its own limit; the three together over the page's
  const t = withCss('budget-css-accumulate', { 'a.css': third, 'b.css': third, 'c.css': third });
  assert.throws(() => checkDist(t), new RegExp(`index\\.html links ${3 * third} bytes of CSS`));
  // Two of them are within the page's budget.
  checkDist(withCss('budget-css-two', { 'a.css': third, 'b.css': third }));
});

// entry.js imports b.js and shared.js, b.js imports shared.js: shared.js is named twice and loaded once by a browser.
const SHARED = (entry: number, b: number, shared: number): Record<string, [string[], number]> => ({
  'entry.js': [['b.js', 'shared.js'], entry],
  'b.js': [['shared.js'], b],
  'shared.js': [[], shared],
});

test('a page whose modules total exactly the page JavaScript limit passes, a module imported by two of them counted once', () => {
  // 32K + 32K + 64K = 128K; counted twice, shared.js would make it 192K and the page would be refused.
  const half = PAGE_JS_MAX_BYTES / 4;
  assert.equal(2 * half + JS_MAX_BYTES, PAGE_JS_MAX_BYTES);
  checkDist(withJs('budget-js-page-exact', SHARED(half, half, JS_MAX_BYTES)));
});

test('a page whose modules total one byte over the page JavaScript limit is refused though each is within its own (the check can fail)', () => {
  const half = PAGE_JS_MAX_BYTES / 4;
  const t = withJs('budget-js-page-over', SHARED(half, half + 1, JS_MAX_BYTES));
  assert.throws(() => checkDist(t), new RegExp(`index\\.html loads ${PAGE_JS_MAX_BYTES + 1} bytes of JavaScript; the limit for one page is ${PAGE_JS_MAX_BYTES}`));
});

test('a page counts the modules its entry imports, not only the entry (the check can fail)', () => {
  const t = withJs('budget-js-imported', { 'entry.js': [['b.js', 'c.js'], 100], 'b.js': [[], JS_MAX_BYTES], 'c.js': [[], JS_MAX_BYTES] });
  assert.throws(() => checkDist(t), /index\.html loads \d+ bytes of JavaScript/);
});
