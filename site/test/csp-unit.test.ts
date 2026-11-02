// The policy parser and the resource check of tools/csp.ts, driven with small hand-written pages: which directive
// governs each kind of tag, the fallback to default-src, and the two directives that do not fall back.

import assert from 'node:assert/strict';
import { writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { test } from 'node:test';
import { parsePolicy, policyFromSource, policyViolations, sourcesFor } from '../tools/csp';
import { scratch } from './support';

let n = 0;
function violationsOf(policy: string, body: string): string[] {
  const dir = scratch(`csp-unit-${++n}`);
  writeFileSync(join(dir, 'index.html'), `<!doctype html>\n<html><body>${body}</body></html>\n`);
  return policyViolations(parsePolicy(policy), dir);
}

test('policyFromSource joins the directives of the array whether they are single or double quoted', () => {
  const mixed = `export const CONTENT_SECURITY_POLICY = [\n  "default-src 'none'",\n  'img-src data:',\n].join('; ');`;
  assert.equal(policyFromSource(mixed), "default-src 'none'; img-src data:");
  const plain = `const CONTENT_SECURITY_POLICY = ["default-src 'none'", "img-src 'self'"].join('; ');`;
  assert.equal(policyFromSource(plain), "default-src 'none'; img-src 'self'");
});

test('policyFromSource refuses a source with no CONTENT_SECURITY_POLICY array (the check can fail)', () => {
  assert.throws(() => policyFromSource('export const OTHER = [];'), /CONTENT_SECURITY_POLICY .* not found/);
});

test('parsePolicy lower-cases directive names, keeps the sources in order and skips empty directives', () => {
  const p = parsePolicy("Default-Src 'none';; IMG-SRC 'self' https://a.example ;");
  assert.deepEqual([...p.keys()], ['default-src', 'img-src']);
  assert.deepEqual(p.get('img-src'), ["'self'", 'https://a.example']);
});

test('sourcesFor gives the directive its own sources, else default-src, else nothing', () => {
  const p = parsePolicy("default-src 'self'; img-src 'none'");
  assert.deepEqual(sourcesFor(p, 'img-src'), ["'none'"]);
  assert.deepEqual(sourcesFor(p, 'script-src'), ["'self'"]);
  assert.deepEqual(sourcesFor(parsePolicy("img-src 'self'"), 'script-src'), []);
});

// [a tag that loads something, the directive that governs it]
const GOVERNED: [string, string][] = [
  ['<script src="/a.js"></script>', 'script-src'],
  ['<link rel="stylesheet" href="/a.css">', 'style-src'],
  ['<link rel="STYLESHEET preload" href="/a.css">', 'style-src'],
  ['<link rel="modulepreload" href="/a.js">', 'script-src'],
  ['<link rel="icon" href="/a.png">', 'img-src'],
  ['<link rel="apple-touch-icon" href="/a.png">', 'img-src'],
  ['<link rel="preload" href="/a.bin">', 'default-src'],
  ['<link href="/a.bin">', 'default-src'],
  ['<img src="/a.png">', 'img-src'],
  ['<image href="/a.png">', 'img-src'],
  ['<iframe src="/a"></iframe>', 'frame-src'],
  ['<frame src="/a">', 'frame-src'],
  ['<object data="/a"></object>', 'object-src'],
  ['<embed src="/a">', 'object-src'],
  ['<audio src="/a"></audio>', 'media-src'],
  ['<video src="/a"></video>', 'media-src'],
  ['<source src="/a">', 'media-src'],
  ['<track src="/a">', 'media-src'],
  ['<form action="/a"></form>', 'form-action'],
  ['<base href="/a/">', 'base-uri'],
];

for (const [tag, directive] of GOVERNED) {
  const name = tag.slice(0, tag.indexOf('>') + 1);
  test(`${name} is governed by ${directive}: refused when that directive lacks 'self', allowed when it has it (the check can fail)`, () => {
    // A tag governed by default-src itself is tested with that one directive, so no policy repeats a directive.
    const [refused, allowed] = directive === 'default-src'
      ? ["default-src 'none'; img-src 'self'", "default-src 'self'; img-src 'none'"]
      : [`default-src 'self'; ${directive} 'none'`, `default-src 'none'; ${directive} 'self'`];
    assert.equal(violationsOf(refused, tag).length, 1);
    assert.deepEqual(violationsOf(allowed, tag), []);
  });
}

test('a violation names the file, the tag, the directive and the sources that do not allow self', () => {
  assert.deepEqual(violationsOf("default-src 'none'", '<img src="/a.png">'), ["index.html: <img> is governed by img-src ('none'), which does not allow 'self'"]);
});

test('a directive missing from a policy with no default-src is reported as governed by nothing', () => {
  assert.deepEqual(violationsOf("img-src 'self'", '<script src="/a.js"></script>'), ["index.html: <script> is governed by script-src (nothing), which does not allow 'self'"]);
});

test('a fetch directive falls back to default-src', () => {
  assert.deepEqual(violationsOf("default-src 'self'", '<img src="/a.png"><iframe src="/a"></iframe>'), []);
  assert.equal(violationsOf("default-src 'none'", '<img src="/a.png"><iframe src="/a"></iframe>').length, 2);
});

test('base-uri and form-action do not fall back to default-src: absent means anything, so they are reported (the check can fail)', () => {
  assert.deepEqual(violationsOf("default-src 'self'", '<base href="/a/">'), ["index.html: <base> is governed by base-uri (*), which does not allow 'self'"]);
  assert.deepEqual(violationsOf("default-src 'self'", '<form action="/a"></form>'), ["index.html: <form> is governed by form-action (*), which does not allow 'self'"]);
  assert.deepEqual(violationsOf("default-src 'none'; base-uri 'self'; form-action 'self'", '<base href="/a/"><form action="/a"></form>'), []);
});

test('a canonical or alternate link, an anchor and other tags that load nothing are not checked', () => {
  const body = '<link rel="canonical" href="https://x.example/"><link rel="alternate" href="/b"><a href="/c">c</a><p>text</p>';
  assert.deepEqual(violationsOf("default-src 'none'", body), []);
});

test('only .html files are checked', () => {
  const dir = scratch(`csp-unit-${++n}`);
  writeFileSync(join(dir, 'x.txt'), '<img src="/a.png">');
  assert.deepEqual(policyViolations(parsePolicy("default-src 'none'"), dir), []);
});
