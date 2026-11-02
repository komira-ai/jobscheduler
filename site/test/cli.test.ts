// The command-line entries of the build (tools/build.ts) and of the golden manifest (tools/manifest.ts).
//
// Catches: a build that writes somewhere other than site/dist or the directory it was given, or reports the wrong file
// count or asset id; a manifest command that writes the manifest anywhere but test/golden.manifest or the path it was
// given, or writes one that differs from the manifest of the same build; and a compiled file that, run by node, does
// nothing (its entry guard), reports success after a failed build, or runs the command when it is only imported.

import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { cpSync, existsSync, mkdirSync, readFileSync } from 'node:fs';
import { join } from 'node:path';
import { test } from 'node:test';
import { main as buildMain } from '../tools/build';
import { listFiles } from '../tools/check-dist';
import { main as manifestMain, manifestOf } from '../tools/manifest';
import { scratch, SITE } from './support';

/** A site root of its own in a scratch directory: the compiled browser modules, the stylesheets and the copy. */
function siteCopy(name: string): string {
  const site = join(scratch(name), 'site');
  mkdirSync(join(site, 'test'), { recursive: true });
  for (const d of ['static', 'content']) cpSync(join(SITE, d), join(site, d), { recursive: true });
  cpSync(join(SITE, 'build', 'browser'), join(site, 'build', 'browser'), { recursive: true });
  return site;
}

const run = (file: string, args: string[], cwd: string) =>
  spawnSync(process.execPath, [join(SITE, 'build', 'tools', 'tools', file), ...args], { cwd, encoding: 'utf8' });

test('main builds into dist of the site without an argument, and reports the file count and the asset id', () => {
  const site = siteCopy('cli-default');
  let said = '';
  buildMain(site, [], { write: (s) => void (said += s) });
  const files = listFiles(join(site, 'dist'));
  const id = /assets\/([0-9a-f]{16})\//.exec(files.join('\n'))?.[1];
  assert.ok(id, 'the build wrote hashed assets');
  assert.equal(said, `dist: ${files.length} files, assets/${id}/\n`);
  assert.ok(files.includes('index.html'));
});

test('main builds into the directory it is given, and leaves site/dist alone (the check can fail)', () => {
  const site = siteCopy('cli-given');
  const target = join(site, 'elsewhere');
  buildMain(site, [target], { write: () => undefined });
  assert.ok(existsSync(join(target, 'index.html')));
  assert.equal(existsSync(join(site, 'dist')), false);
});

test('node build.js <dir> builds into <dir> relative to the working directory and prints the report', () => {
  const cwd = scratch('cli-spawn-build');
  const r = run('build.js', ['out'], cwd);
  assert.equal(r.status, 0, r.stderr);
  const files = listFiles(join(cwd, 'out'));
  assert.match(r.stdout, new RegExp(`^dist: ${files.length} files, assets/[0-9a-f]{16}/\n$`));
  assert.ok(files.includes('404.html'));
});

test('node build.js exits non-zero and prints no report when the output directory cannot be made (the check can fail)', () => {
  const cwd = scratch('cli-spawn-bad');
  // package.json is a file, so nothing can be created below it.
  const r = run('build.js', [join(SITE, 'package.json', 'out')], cwd);
  assert.notEqual(r.status, 0);
  assert.equal(r.stdout, '');
  // Negative: the same command with a usable directory succeeds.
  assert.equal(run('build.js', ['ok'], cwd).status, 0);
});

test('importing the compiled build and manifest modules runs no command', () => {
  for (const file of ['build.js', 'manifest.js']) {
    const cwd = scratch(`cli-import-${file}`);
    const r = spawnSync(process.execPath, ['-e', `require(${JSON.stringify(join(SITE, 'build', 'tools', 'tools', file))})`], { cwd, encoding: 'utf8' });
    assert.equal(r.status, 0, r.stderr);
    assert.equal(r.stdout, '');
    assert.deepEqual(listFiles(cwd), []);
  }
});

test('manifest main writes the manifest of its build to test/golden.manifest of the site without an argument', () => {
  const site = siteCopy('cli-manifest-default');
  manifestMain(site, []);
  const written = readFileSync(join(site, 'test', 'golden.manifest'), 'utf8');
  assert.equal(written, manifestOf(join(site, 'build', 'golden-dist')));
  assert.equal(written, readFileSync(join(SITE, 'test', 'golden.manifest'), 'utf8'), 'the same sources give the committed manifest');
});

test('node manifest.js <file> writes the manifest to <file> and leaves the committed one alone (the check can fail)', () => {
  const cwd = scratch('cli-spawn-manifest');
  const before = readFileSync(join(SITE, 'test', 'golden.manifest'), 'utf8');
  const r = run('manifest.js', ['m.txt'], cwd);
  assert.equal(r.status, 0, r.stderr);
  assert.equal(readFileSync(join(cwd, 'm.txt'), 'utf8'), before);
  assert.equal(readFileSync(join(SITE, 'test', 'golden.manifest'), 'utf8'), before);
});
