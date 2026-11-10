// The public-repo scan over the built output.
//
// Catches: text that must not ship in a public repository's site: a machine address, a home directory path, a host
// that is not a public one this site links to, a cloud account's resource name, a build-farm path, an internal host
// name, an internal repository name, a role, a commit SHA, or a date from before the project went public. The scan
// reads every built file. It spells no internal host and no role: those are kept as SHA-256 digests, and the tests of
// those rules use made-up names through injected sets. The date rule compares dates against a cutoff the scan takes
// as a parameter, and its tests use only dates on or after the real cutoff. It reads a date in each spelling a page or a
// test is likely to use (2026-10-01, 2026/10/01, October 1, 2026, 1 October 2026, Oct 2026), and it is also run over every
// file of the site's own tree, tests and fixtures included, so no early date is committed in any of them. A bare year
// is not read as a date: "2000 m" is a quantity.

import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readdirSync, readFileSync } from 'node:fs';
import { join } from 'node:path';
import { test } from 'node:test';
import { listFiles } from '../tools/check-dist';
import { buildReal, copyTree, damage, SITE } from './support';

/** The hosts a page may name in a URL: the project's own, its apt repository, GitHub, the SVG namespace. */
const PUBLIC_HOSTS = new Set(['jobscheduler.dev', 'kbf.dev', 'packages.komira.ai', 'github.com', 'www.w3.org']);
/** Placeholders and loopback the install page uses as examples. */
const EXAMPLE_HOSTS = new Set(['host', 'localhost', '127.0.0.1']);
/** Addresses a page may print: loopback and the documentation ranges. */
const EXAMPLE_ADDRESS = /^(?:127\.0\.0\.[01]|0\.0\.0\.0|192\.0\.2\.\d+|198\.51\.100\.\d+|203\.0\.113\.\d+)$/;

/**
 * SHA-256 digests (lowercase hex) of the names a page may not carry: the machine names of the fleet the project is built on.
 * The names are kept as digests so that this public file does not spell them.
 */
const INTERNAL_NAME_DIGESTS: ReadonlySet<string> = new Set<string>([
  // The whole names of one numbered shape, ten of them (shape followed by 00 to 09).
  '390d87d849b5818395fa0522a03f873b4dd1892c6dc2d02274fa9e1c5e7db26a',
  '70997b5d616f4da45090f863e47f048b7773464ffaf60575d03e613d22f8da10',
  '0f5aa9d8fdf7cd7e3d4ab8a39648513c7b06a98d8e081b591989b70839b5460c',
  '879fdcb78de039afea292497cf56033b30d351e9896e1cd37d04aae392af0576',
  'd52c6ab21a194785901db1c28f45c638bc9b1a4c18eb1d30f4b59ba9ab440b63',
  '927737f5ef57e4f6e4e3d39126f6f70d2ca434a241b63b70f0f9a38b89596360',
  '7ebda8e19caa08f4977a57e81244a48774cc181299e2ec76161017e608c8e588',
  '6fe039a3c056fe992495e12b605233da82a31392643dbd363623c0d90fffc379',
  '0ab2cfa1499fe2262dbc76f0df6dd63c4a18cb519bd9424baff0361bae54308c',
  '235eff94783530f47f3e08d8b1daa82780689002440fa70907629b4f88f9e79c',
  // The stems of three more shapes: one a prefix before a hyphen, two machine families that take a number.
  'ae238ad51a2cd16ee5edfa12986a516d39bc9d80eb04c8b7e894650a869cbcb2',
  '0f96571912ed33a1cd735cd2e659e289788af087ad8ec2b6ddaf5b661ce9591a',
  '28ce5c91a5190fc5abed868345152b60fe8d70d9c2a19340d76c012b00d25038',
]);

const sha256 = (s: string): string => createHash('sha256').update(s).digest('hex');

/** What a token is looked up as: lowercase, for each hyphen-prefix of it (one segment, two, ... the whole), that prefix whole and without its trailing digits. */
function nameCandidates(token: string): string[] {
  const segments = token.toLowerCase().split('-');
  return segments.flatMap((_, i) => {
    const prefix = segments.slice(0, i + 1).join('-');
    return [prefix, prefix.replace(/\d+$/, '')];
  });
}

/** The findings for `text`: each token (a run of letters, digits and hyphens) one of whose candidates has its digest in `deny`. The finding gives the line and the digest, not the name. */
function hashedNames(text: string, deny: ReadonlySet<string>): string[] {
  const found: string[] = [];
  text.split('\n').forEach((line, i) => {
    for (const token of line.split(/[^A-Za-z0-9-]+/)) {
      const hit = nameCandidates(token).map(sha256).find((d) => deny.has(d));
      if (token !== '' && hit !== undefined) found.push(`an internal host name (line ${i + 1}, sha256 ${hit.slice(0, 12)})`);
    }
  });
  return found;
}

/** The cutoff of the date rule: the day the project went public. A date earlier than this is a finding. */
const PUBLIC_FROM = '2026-09-01';

/** SHA-256 digests (lowercase hex) of the roles a page may not name, each the digest of the role's lowercase spelling; a token is compared whole and case-insensitively. */
const ROLE_DIGESTS: ReadonlySet<string> = new Set<string>(['31d123108dba5fd36075ab4f89b130a495b15278b38e8a1a756bc43926c43e2e']);

function scanText(text: string, code = false, deny: ReadonlySet<string> = INTERNAL_NAME_DIGESTS, cutoff: string = PUBLIC_FROM, roles: ReadonlySet<string> = ROLE_DIGESTS): string[] {
  const found: string[] = hashedNames(text, deny);
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
  for (const m of text.matchAll(new RegExp('komira-' + 'internal', 'g'))) found.push(`an internal repository name, ${m[0]}`);
  for (const d of earlyDates(text, cutoff)) found.push(`a date before the project was public, ${d}`);
  for (const token of text.split(/[^A-Za-z0-9]+/)) if (token !== '' && roles.has(sha256(token.toLowerCase()))) found.push(`an internal role (sha256 ${sha256(token.toLowerCase()).slice(0, 12)})`);
  for (const m of text.matchAll(/\b[0-9a-f]{40}\b/g)) found.push(`a commit SHA, ${m[0]}`);
  return found;
}

const MONTHS = ['january', 'february', 'march', 'april', 'may', 'june', 'july', 'august', 'september', 'october', 'november', 'december'];
const MONTH = '(January|February|March|April|May|June|July|August|September|October|November|December|Jan|Feb|Mar|Apr|Jun|Jul|Aug|Sep|Sept|Oct|Nov|Dec)';
const two = (n: number): string => String(n).padStart(2, '0');
/** The month number (1 to 12) of a month's name or its abbreviation. */
const monthOf = (name: string): number => MONTHS.findIndex((m) => m.startsWith(name.toLowerCase().slice(0, 3))) + 1;
/** The last day of a month, as YYYY-MM-DD: a month with no day named is early only if all of it is. */
const monthEnd = (y: number, m: number): string => `${y}-${two(m)}-${two(new Date(Date.UTC(y, m, 0)).getUTCDate())}`;

/** The dates in `text` earlier than `cutoff` (YYYY-MM-DD), as written. Read: YYYY-MM-DD, YYYY/MM/DD, "Month D, YYYY", "D Month YYYY" and "Month YYYY" (names or their three-letter forms). */
export function earlyDates(text: string, cutoff: string): string[] {
  const out: string[] = [];
  for (const m of text.matchAll(/\b(\d{4})[-/](0[1-9]|1[0-2])[-/](0[1-9]|[12]\d|3[01])\b/g)) if (`${m[1]}-${m[2]}-${m[3]}` < cutoff) out.push(m[0]);
  const named = new RegExp(`\\b(?:${MONTH}\\.? (\\d{1,2}),? (\\d{4})|(\\d{1,2}) ${MONTH}\\.?,? (\\d{4})|${MONTH}\\.?,? (\\d{4}))\\b`, 'g');
  for (const m of text.matchAll(named)) {
    const [month, day, year] = m[1] ? [m[1], m[2], m[3]] : m[5] ? [m[5], m[4], m[6]] : [m[7], undefined, m[8]];
    const y = Number(year);
    const at = day === undefined ? monthEnd(y, monthOf(month!)) : `${y}-${two(monthOf(month!))}-${two(Number(day))}`;
    if (at < cutoff) out.push(m[0]);
  }
  return out;
}

/** Every file of the site's own tree, as paths from the site root: what is committed, without what npm and the build leave. */
function siteFiles(dir = SITE, prefix = ''): string[] {
  return readdirSync(join(dir), { withFileTypes: true }).flatMap((e) => {
    if (['node_modules', 'build', 'dist'].includes(e.name)) return [];
    return e.isDirectory() ? siteFiles(join(dir, e.name), `${prefix}${e.name}/`) : [`${prefix}${e.name}`];
  });
}

test('no file of the site, tests and fixtures included, carries a date before the project went public, in any spelling', () => {
  const files = siteFiles();
  assert.ok(files.includes('test/hygiene.test.ts') && files.includes('content/index.html'), 'the walk reaches the tests and the pages');
  assert.deepEqual(files.flatMap((f) => earlyDates(readFileSync(join(SITE, f), 'utf8'), PUBLIC_FROM).map((d) => `${f}: ${d}`)), []);
});

test('each spelling of a date is read and compared with the cutoff, and a month counts whole (the check can fail)', () => {
  const cutoff = '2026-10-15';
  assert.deepEqual(earlyDates('2026-10-01, 2026/10/01, October 1, 2026, Oct 1 2026, 1 October 2026, 1 Oct. 2026 and September 2026', cutoff),
    ['2026-10-01', '2026/10/01', 'October 1, 2026', 'Oct 1 2026', '1 October 2026', '1 Oct. 2026', 'September 2026']);
  assert.deepEqual(earlyDates('2026-10-15, October 15, 2026, 20 Oct 2026, October 2026, Nov 2026', cutoff), []);
  // Not dates: a quantity, a version, and a month name with no year.
  assert.deepEqual(earlyDates('2000 m, version 2026.9.1, in October', cutoff), []);
  // The site walk sees a planted early spelling.
  assert.deepEqual(earlyDates('see Sept 3, 2026', '2026-10-15'), ['Sept 3, 2026']);
});

const dist = buildReal('hygiene');
const scanTree = (dir: string, deny: ReadonlySet<string> = INTERNAL_NAME_DIGESTS): string[] => listFiles(dir).flatMap((f) => scanText(readFileSync(join(dir, f), 'utf8'), /^assets\/[0-9a-f]+\/[a-z]+\.js$/.test(f), deny).map((p) => `${f}: ${p}`));

test('no built file carries an address, a home path, a non-public host, a role, a SHA, an internal name or an early date', () => {
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
    ['komira-' + 'internal#12', /an internal repository name/],
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

/** A deny set of made-up names (never the real set), one for each form a candidate takes. */
const TEST_DENY: ReadonlySet<string> = new Set(['zebra-07', 'quokka', 'narwhal'].map(sha256));

test('a name in a deny set is refused on a page by its whole form, by its form without trailing digits and by its first segment', () => {
  const refused = (page: string): string => scanText(page, false, TEST_DENY).join('\n');
  assert.match(refused('<p>run it on zebra-07 today</p>'), /internal host name/);
  assert.match(refused('<p>run it on ZEBRA-07 today</p>'), /internal host name/);
  assert.match(refused('<p>run it on quokka9 today</p>'), /internal host name/);
  assert.match(refused('<p>run it on narwhal-edge-3 today</p>'), /internal host name/);
  assert.match(refused('<code>narwhal-4</code>'), /internal host name/);
  assert.deepEqual(scanText('<p>zebra-08, quokkas and narwhals</p>', false, TEST_DENY), []);
});

test('a name in a deny set that a built page carries is found by the tree scan (the check can fail)', () => {
  const t = copyTree(dist, 'hygiene-damaged-name');
  damage(t, 'index.html', '<h1', '<p>on zebra-07 only</p><h1');
  assert.match(scanTree(t, TEST_DENY).join('\n'), /index\.html: .*internal host name/);
  assert.deepEqual(scanTree(dist, TEST_DENY), []);
});

test('the real deny set and the real role set are not empty and hold only SHA-256 digests in lowercase hex', () => {
  assert.ok(INTERNAL_NAME_DIGESTS.size > 0);
  assert.ok(ROLE_DIGESTS.size > 0);
  for (const d of [...INTERNAL_NAME_DIGESTS, ...ROLE_DIGESTS]) assert.match(d, /^[0-9a-f]{64}$/);
});

test('the words node, nodes and node.js are not refused', () => {
  assert.deepEqual(scanText('<p>a node, three nodes, node.js, Node and the nodes-per-rack setting</p>'), []);
});

test('a name in a deny set followed by more hyphenated words is refused, and a longer name that only starts like it is not', () => {
  const refused = (page: string): string => scanText(page, false, TEST_DENY).join('\n');
  assert.match(refused('<p>on zebra-07-edge</p>'), /internal host name/);
  assert.match(refused('<p>on Zebra-07-a-b.</p>'), /internal host name/);
  assert.match(refused('<p>on quokka9-x-y</p>'), /internal host name/);
  assert.match(refused('<p>on narwhal-a-b-c</p>'), /internal host name/);
  assert.deepEqual(scanText('<p>zebra-070-edge, quokkas-per-rack, narwhals-a-b, zebra-0-b</p>', false, TEST_DENY), []);
});

test('a date earlier than the cutoff is refused and a date on or after it is not, whatever the cutoff (the check can fail)', () => {
  const dates = (text: string, cutoff: string): string[] => scanText(text, false, INTERNAL_NAME_DIGESTS, cutoff).filter((f) => /a date before/.test(f));
  assert.deepEqual(dates('on 2026-10-01 we', '2026-10-15'), ['a date before the project was public, 2026-10-01']);
  assert.deepEqual(dates('on 2026-10-15 and 2026-10-20 we', '2026-10-15'), []);
  assert.deepEqual(dates('on 2026-10-01 we', '2026-09-01'), []);
  assert.equal(dates('in 2027-01-05 and 2026-12-31', '2027-01-01').length, 1);
});

test('the real cutoff is the day the project went public', () => {
  assert.equal(PUBLIC_FROM, '2026-09-01');
  assert.deepEqual(scanText('on 2026-09-01 and 2026-10-01'), []);
});

test('a role in a role set is refused as a whole token in any case, and a word that only contains it is not (the check can fail)', () => {
  const roles: ReadonlySet<string> = new Set(['wombat'].map(sha256));
  const refused = (page: string): string[] => scanText(page, false, INTERNAL_NAME_DIGESTS, PUBLIC_FROM, roles).filter((f) => /internal role/.test(f));
  assert.equal(refused('<p>the wombat said</p>').length, 1);
  assert.equal(refused('<p>the WOMBAT said</p>').length, 1);
  assert.equal(refused('<p>wombats and combat and wombat-like</p>').length, 1);
  assert.deepEqual(refused('<p>wombats and combat</p>'), []);
});
