// The decision of whether ci.yml runs the site steps: the script of the step `id: site` of the `lint` job, taken from
// the workflow text and run by bash against small git repositories, with site/ci-paths copied in.
//
// Catches: a push to main that skips the site, a pull request that changes a listed path (the site, a document a
// claim reads) and does not run it, an unrelated Rust change that does run it, a path that only shares a prefix with a
// directory entry, a diff or a list that cannot be read and does not fail closed, and a workflow that stops reading
// site/ci-paths. The script is read from ci.yml, so the one tested is the one that runs.

import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { copyFileSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { test } from 'node:test';
import { ciPathEntries, ciWorkflow, siteScript } from './ci-site';
import { scratch, SITE } from './support';

const ENV = { PATH: process.env.PATH ?? '', GIT_CONFIG_GLOBAL: '/dev/null', GIT_CONFIG_SYSTEM: '/dev/null', GIT_AUTHOR_NAME: 't', GIT_AUTHOR_EMAIL: 't@example.com', GIT_COMMITTER_NAME: 't', GIT_COMMITTER_EMAIL: 't@example.com' };

function git(dir: string, ...args: string[]): void {
  const r = spawnSync('git', args, { cwd: dir, env: ENV, encoding: 'utf8' });
  assert.equal(r.status, 0, `git ${args.join(' ')}: ${r.stderr}`);
}

function put(dir: string, file: string, text: string): void {
  mkdirSync(dirname(join(dir, file)), { recursive: true });
  writeFileSync(join(dir, file), text);
}

interface Options {
  /** The text of site/ci-paths in the repository; the real one when absent. */
  list?: string | null;
  /** The base ref the script diffs against (a ref `origin/<base>` exists only for 'main'). */
  base?: string;
  event?: string;
  script?: string;
}

/** Runs the decision script in a repository whose pull request changes `changed`, and returns the value of `run=`. */
function decide(name: string, changed: readonly string[], opts: Options = {}): string {
  const dir = scratch(`ci-site-${name}`);
  git(dir, 'init', '-q', '-b', 'main');
  put(dir, 'README.md', 'x\n');
  put(dir, 'crates/js-sched/src/lib.rs', 'fn a() {}\n');
  if (opts.list !== null) {
    if (opts.list === undefined) {
      mkdirSync(join(dir, 'site'), { recursive: true });
      copyFileSync(join(SITE, 'ci-paths'), join(dir, 'site', 'ci-paths'));
    } else put(dir, 'site/ci-paths', opts.list);
  }
  git(dir, 'add', '.');
  git(dir, 'commit', '-q', '-m', 'base');
  git(dir, 'update-ref', 'refs/remotes/origin/main', 'HEAD');
  git(dir, 'checkout', '-q', '-b', 'pr');
  for (const f of changed) put(dir, f, `changed ${f}\n`);
  git(dir, 'add', '-A');
  git(dir, 'commit', '-q', '--allow-empty', '-m', 'head');
  const out = join(dir, 'github_output');
  const r = spawnSync('bash', ['-e', '-c', opts.script ?? siteScript(ciWorkflow())], {
    cwd: dir,
    env: { ...ENV, EVENT: opts.event ?? 'pull_request', BASE: opts.base ?? 'main', GITHUB_OUTPUT: out },
    encoding: 'utf8',
  });
  assert.equal(r.status, 0, `the script failed: ${r.stderr}`);
  const lines = readFileSync(out, 'utf8').trim().split('\n');
  assert.equal(lines.length, 1, lines.join('|'));
  rmSync(dir, { recursive: true, force: true });
  return lines[0].replace(/^run=/, '');
}

test('the script is the one in ci.yml and reads the list in site/ci-paths', () => {
  const script = siteScript(ciWorkflow());
  assert.ok(script.length > 200, 'the script was found');
  assert.match(script, /site\/ci-paths/);
  assert.ok(ciPathEntries(readFileSync(join(SITE, 'ci-paths'), 'utf8')).length >= 20);
});

test('a push to main always runs the site steps', () => {
  assert.equal(decide('push', ['crates/js-sched/src/lib.rs'], { event: 'push' }), 'true');
  assert.equal(decide('push-nothing', [], { event: 'push' }), 'true');
});

test('a pull request that changes the site or a file under a listed directory runs the steps', () => {
  assert.equal(decide('site', ['site/content/index.html']), 'true');
  assert.equal(decide('docs', ['docs/design/placement-by-load.md']), 'true');
  assert.equal(decide('crate', ['crates/js-sched/src/lib.rs']), 'true');
  assert.equal(decide('deep', ['packaging/js-agent/debian/control']), 'true');
});

test('a pull request that changes a listed file runs the steps, also with other changes', () => {
  assert.equal(decide('readme', ['README.md']), 'true');
  assert.equal(decide('ci', ['.github/workflows/ci.yml']), 'true');
  assert.equal(decide('mixed', ['crates/js-registry/tests/a.rs', 'LICENSE']), 'true');
});

test('a pull request that changes only unrelated files skips the steps', () => {
  assert.equal(decide('rust', ['crates/js-sched/tests/a.rs', 'crates/js-core/src/lib.rs']), 'false');
  assert.equal(decide('lock', ['Cargo.lock']), 'false');
  assert.equal(decide('empty', []), 'false');
});

test('a path that only shares a prefix with a directory entry or a file entry does not run the steps', () => {
  assert.equal(decide('prefix-dir', ['docs-old/a.md', 'site-extra/a.md', 'crates/js-sched/srcs/a.rs']), 'false');
  assert.equal(decide('prefix-file', ['README.mdx', 'LICENSE-APACHE']), 'false');
});

test('comments and blank lines of the list are ignored, and trailing blanks of an entry too', () => {
  assert.equal(decide('comment', ['# nope'], { list: '# nope\n\n  \nCargo.toml   \n' }), 'false');
  assert.equal(decide('trailing', ['Cargo.toml'], { list: '# c\n\nCargo.toml   \n' }), 'true');
  assert.equal(decide('own-list', ['site/a.html'], { list: 'docs/\n' }), 'false');
});

test('a diff that cannot be computed runs the steps', () => {
  assert.equal(decide('no-base', ['crates/js-core/src/lib.rs'], { base: 'gone' }), 'true');
});

test('a list that is missing or has no entries runs the steps', () => {
  assert.equal(decide('no-list', ['crates/js-core/src/lib.rs'], { list: null }), 'true');
  assert.equal(decide('empty-list', ['crates/js-core/src/lib.rs'], { list: '# nothing\n\n' }), 'true');
});

test('a script that reads another list, or never matches a directory entry, gives other answers (the check can fail)', () => {
  const real = siteScript(ciWorkflow());
  const other = real.replace('site/ci-paths', 'site/other-paths');
  assert.notEqual(other, real);
  assert.equal(decide('real-skip', ['crates/js-core/src/lib.rs']), 'false');
  assert.equal(decide('other-list', ['crates/js-core/src/lib.rs'], { script: other }), 'true');
  const noDirs = real.replace(/\[ "\$\{file#"\$path"\}" != "\$file" \] && run=true/, ':');
  assert.notEqual(noDirs, real);
  assert.equal(decide('dirs-real', ['site/a.html']), 'true');
  assert.equal(decide('dirs-broken', ['site/a.html'], { script: noDirs }), 'false');
});
