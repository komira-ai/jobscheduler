// The public-repo scan over the built output.
//
// Catches: text that must not ship in a public repository's site: a machine address, a home directory path, a host
// that is not a public one this site links to, a cloud account's resource name, a build-farm path, a role, a commit
// SHA, or a date from before the project went public. The scan reads every built file. It names no internal host and
// no internal repository: it looks for the kinds of thing that leak (an address, a path, a host that is not on the
// short list below), not for particular names.

import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { test } from 'node:test';
import { listFiles } from '../tools/check-dist';
import { buildReal, copyTree, damage } from './support';

/** The hosts a page may name in a URL: the project's own, GitHub, the SVG namespace. */
const PUBLIC_HOSTS = new Set(['jobscheduler.dev', 'kbf.dev', 'github.com', 'www.w3.org']);
/** Placeholders and loopback the install page uses as examples. */
const EXAMPLE_HOSTS = new Set(['host', 'localhost', '127.0.0.1']);
/** Addresses a page may print: loopback and the documentation ranges. */
const EXAMPLE_ADDRESS = /^(?:127\.0\.0\.[01]|0\.0\.0\.0|192\.0\.2\.\d+|198\.51\.100\.\d+|203\.0\.113\.\d+)$/;

function scanText(text: string, code = false): string[] {
  const found: string[] = [];
  for (const m of text.matchAll(/\b(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})\b/g)) {
    const parts = [m[1], m[2], m[3], m[4]].map(Number);
    if (parts.every((p) => p <= 255) && !EXAMPLE_ADDRESS.test(m[0])) found.push(`an address, ${m[0]}`);
  }
  for (const m of text.matchAll(/(?<![.\/-])\/(?:home|Users|root)\/[A-Za-z0-9._-]+/g)) found.push(`a home path, ${m[0]}`);
  for (const m of text.matchAll(/\bbuck-out\/\S*/g)) found.push(`a build-farm path, ${m[0]}`);
  for (const m of text.matchAll(/[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+\.[A-Za-z]{2,}/g)) found.push(`an email address or user@host, ${m[0]}`);
  for (const m of text.matchAll(/\b[a-z][a-z0-9+.-]*:\/\/([A-Za-z0-9.-]+)/g)) {
    const host = m[1];
    if (!PUBLIC_HOSTS.has(host) && !EXAMPLE_HOSTS.has(host)) found.push(`a host, ${host}`);
  }
  // Program text is skipped here: `c.local` is a property of an object, not a host.
  for (const m of code ? [] : text.matchAll(/\b[\w.-]+\.(?:internal|local|lan|corp|amazonaws\.com|cloudfront\.net)\b/g)) found.push(`a private or cloud host name, ${m[0]}`);
  for (const m of text.matchAll(/arn:aws[\w-]*:\S+/g)) found.push(`a cloud resource name, ${m[0]}`);
  for (const m of text.matchAll(/\b(20(?:[01]\d|2[0-5])-\d\d-\d\d|2026-0[1-8]-\d\d)\b/g)) found.push(`a date before the project was public, ${m[1]}`);
  if (/\bCEO\b/.test(text)) found.push('an internal role');
  for (const m of text.matchAll(/\b[0-9a-f]{40}\b/g)) found.push(`a commit SHA, ${m[0]}`);
  return found;
}

const dist = buildReal('hygiene');
const scanTree = (dir: string): string[] => listFiles(dir).flatMap((f) => scanText(readFileSync(join(dir, f), 'utf8'), /^assets\/[0-9a-f]+\/[a-z]+\.js$/.test(f)).map((p) => `${f}: ${p}`));

test('no built file carries an address, a home path, a non-public host, a role, a SHA or an early date', () => {
  assert.deepEqual(scanTree(dist), []);
});

test('the scan finds each kind of leak (it can fail)', () => {
  const ip = ['10', '1', '2', '3'].join('.');
  const home = '/ho' + 'me/alice';
  const cases: [string, RegExp][] = [
    [`ssh ${ip}`, /an address/],
    [`cd ${home}/src`, /a home path/],
    ['in buck-' + 'out/v2/tmp/x', /a build-farm path/],
    ['mail ' + 'a@b' + '.example', /an email address/],
    ['see https://build-7.example.org/x', /a host, build-7\.example\.org/],
    ['at ip-1-2-3-4.compute.internal', /a private or cloud host name/],
    ['arn:aws:iam::123456789012:role/x', /a cloud resource name/],
    ['on ' + '2026-0' + '3-14 we', /a date before/],
    ['the C' + 'EO said', /an internal role/],
    ['at ' + 'abcdef0123456789abcdef0123456789abcdef01', /a commit SHA/],
  ];
  for (const [text, why] of cases) assert.match(scanText(text).join('\n'), why, text);
  assert.deepEqual(scanText('https://github.com/komira-ai/jobscheduler and http://127.0.0.1:7300 and 127.0.0.0/8 and 2026-10-01'), []);
});

test('a leak in the page copy is found in the built tree', () => {
  const t = copyTree(dist, 'hygiene-damaged');
  damage(t, 'index.html', '<h1', `<p>${['10', '9', '8', '7'].join('.')}</p><h1`);
  assert.match(scanTree(t).join('\n'), /index\.html: an address/);
});
