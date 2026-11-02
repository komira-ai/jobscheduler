// The supply-chain policy of the site's own dependencies (and the Node version it is built with).
//
// Catches: a runtime dependency (the site ships none: its pages are static files and its script has no imports from
// packages), a range (^ or ~) or a tag where an exact version belongs, a lockfile entry that runs an install script,
// is fetched from anywhere but the npm registry, or carries no integrity hash, a Node version that is not the one
// pinned in .node-version, and the site steps of ci.yml building with another version or without --ignore-scripts. A drift in any
// of them changes what code runs in the build before anyone reviews it.

import assert from 'node:assert/strict';
import { test } from 'node:test';
import { existsSync } from 'node:fs';
import { join } from 'node:path';
import { ciWorkflow, jobText, SITE_COMMAND, siteRuns, siteStep, siteSteps } from './ci-site';
import { read, REPO, SITE, SITE_REL } from './support';

interface Lock {
  lockfileVersion: number;
  packages: Record<string, { version?: string; resolved?: string; integrity?: string; hasInstallScript?: boolean; link?: boolean }>;
}

function lockProblems(lock: Lock): string[] {
  const out: string[] = [];
  for (const [path, p] of Object.entries(lock.packages)) {
    if (path === '') continue;
    if (p.hasInstallScript) out.push(`${path}: runs an install script`);
    if (p.link === true || !p.resolved?.startsWith('https://registry.npmjs.org/')) out.push(`${path}: not resolved from registry.npmjs.org (${p.resolved ?? 'no resolved'})`);
    if (!p.integrity?.startsWith('sha512-')) out.push(`${path}: no sha512 integrity`);
  }
  return out;
}

function manifestProblems(pkg: { dependencies?: Record<string, string>; devDependencies?: Record<string, string> }): string[] {
  const out: string[] = [];
  if (pkg.dependencies && Object.keys(pkg.dependencies).length > 0) out.push('has runtime dependencies');
  for (const [name, v] of Object.entries(pkg.devDependencies ?? {})) if (!/^\d+\.\d+\.\d+$/.test(v)) out.push(`${name}: "${v}" is not an exact version`);
  return out;
}

const pkg = JSON.parse(read(SITE, 'package.json'));
const lock: Lock = JSON.parse(read(SITE, 'package-lock.json'));

test('package.json has no runtime dependencies and exact devDependencies', () => {
  assert.deepEqual(manifestProblems(pkg), []);
  assert.deepEqual(Object.keys(pkg.devDependencies).sort(), ['@types/node', 'typescript']);
});

test('engines pin the same Node as .node-version, and .npmrc makes a mismatch an error', () => {
  assert.equal(pkg.engines?.node, read(SITE, '.node-version').trim());
  assert.match(read(SITE, '.npmrc'), /^engine-strict=true$/m);
});

test('every lockfile entry is from the npm registry, hashed, and runs no install script', () => {
  assert.equal(lock.lockfileVersion, 3);
  assert.deepEqual(lockProblems(lock), []);
  assert.equal(lock.packages[''] !== undefined, true);
});

test('the lockfile is this package\'s: its name and its development packages are the manifest\'s', () => {
  const root = lock.packages[''] as unknown as { name?: string; devDependencies?: Record<string, string> };
  assert.equal(root.name, pkg.name);
  assert.deepEqual(root.devDependencies, pkg.devDependencies);
});

test('the policy checks can fail', () => {
  assert.deepEqual(manifestProblems({ dependencies: { x: '1.0.0' } }), ['has runtime dependencies']);
  assert.match(manifestProblems({ devDependencies: { a: '^1.2.3', b: '1.2.3' } }).join(), /a: "\^1.2.3" is not an exact version/);
  assert.match(lockProblems({ lockfileVersion: 3, packages: { 'node_modules/a': { resolved: 'https://example.org/a.tgz', integrity: 'sha512-x' } } }).join(), /not resolved from registry/);
  assert.match(lockProblems({ lockfileVersion: 3, packages: { 'node_modules/a': { resolved: 'https://registry.npmjs.org/a', integrity: 'sha1-x' } } }).join(), /no sha512 integrity/);
  assert.match(lockProblems({ lockfileVersion: 3, packages: { 'node_modules/a': { resolved: 'https://registry.npmjs.org/a', integrity: 'sha512-x', hasInstallScript: true } } }).join(), /runs an install script/);
});

/** What is wrong with the site steps of ci.yml: the Node version file, the directory, the install, and the steps' order. */
function siteStepProblems(steps: string): string[] {
  const out: string[] = [];
  if (!steps.includes(`node-version-file: ${SITE_REL}/.node-version\n`)) out.push('does not read .node-version of the site');
  if (/^\s*node-version:/m.test(steps)) out.push('a literal node-version would shadow .node-version');
  if (!steps.includes('run: npm ci --ignore-scripts --no-audit --no-fund\n')) out.push('does not install with --ignore-scripts');
  const cmds = [...steps.matchAll(/^ {8}run: (npm .*)$/gm)].map((m) => m[1]);
  if (cmds.join('|') !== 'npm ci --ignore-scripts --no-audit --no-fund|npm run typecheck|npm test|npm run build') out.push(`runs ${cmds.join(', ')}`);
  const runs = (steps.match(/^ {8}run: npm .*$/gm) ?? []).length;
  const dirs = (steps.match(new RegExp(`^ {8}working-directory: ${SITE_REL}$`, 'gm')) ?? []).length;
  if (dirs !== runs) out.push(`${dirs} of ${runs} npm steps run in the site directory`);
  return out;
}

test('Node is pinned to one exact version, and the site steps of ci.yml build with that file, scripts off', () => {
  assert.match(read(SITE, '.node-version'), /^\d+\.\d+\.\d+\n$/);
  assert.equal(read(SITE, '.node-version'), '24.21.0\n');
  assert.deepEqual(siteStepProblems(siteSteps(ciWorkflow())), []);
});

test('the site-step checks can fail', () => {
  const steps = siteSteps(ciWorkflow());
  assert.ok(steps.length > 0);
  assert.deepEqual(siteStepProblems(''), ['does not read .node-version of the site', 'does not install with --ignore-scripts', 'runs ']);
  assert.match(siteStepProblems(steps.replace('node-version-file: site/.node-version', 'node-version: 24')).join(), /does not read \.node-version/);
  assert.match(siteStepProblems(steps.replace('node-version-file: site/.node-version', 'node-version-file: site/.node-version\n          node-version: 24')).join(), /shadow/);
  assert.match(siteStepProblems(steps.replace(' --ignore-scripts', '')).join(), /does not install with --ignore-scripts/);
  assert.match(siteStepProblems(steps.replace('run: npm test\n', 'run: npm run build\n')).join(), /runs npm ci/);
  assert.match(siteStepProblems(steps.replace('working-directory: site\n        run: npm test', 'run: npm test')).join(), /3 of 4 npm steps/);
});

/** What is wrong with the step `id: site`: it is the one `cargo xtask site-changed` command reading site/ci-paths, fed the event and the base branch, with no shell of its own. */
function siteDecisionProblems(step: string, runs: readonly string[]): string[] {
  const out: string[] = [];
  if (runs.length !== 1 || runs[0] !== SITE_COMMAND) out.push(`runs ${JSON.stringify(runs)}, not the one command ${SITE_COMMAND}`);
  if (/^\s*run: [|>]/m.test(step)) out.push('has a multi-line script');
  if (!step.includes('EVENT: ${{ github.event_name }}\n')) out.push('does not pass the event name');
  if (!step.includes('BASE: ${{ github.base_ref }}\n')) out.push('does not pass the base branch');
  return out;
}

test('the step that decides whether the site steps run is the one cargo xtask command reading site/ci-paths', () => {
  const yml = ciWorkflow();
  assert.deepEqual(siteDecisionProblems(siteStep(yml), siteRuns(yml)), []);
  assert.ok(existsSync(join(REPO, 'crates', 'xtask', 'src', 'site_changed.rs')), 'the verb exists');
  assert.ok(read(REPO, 'crates', 'xtask', 'src', 'lib.rs').includes('SiteChanged(site_changed::Args)'), 'the verb is wired');
});

test('the decision-step checks can fail', () => {
  const yml = ciWorkflow();
  const step = siteStep(yml);
  const runs = siteRuns(yml);
  assert.match(siteDecisionProblems('', []).join(), /^runs \[\]/);
  assert.match(siteDecisionProblems(step, [runs[0].replace('site/ci-paths', 'site/other')]).join(), /not the one command/);
  assert.match(siteDecisionProblems(step, [...runs, 'echo run=true']).join(), /not the one command/);
  assert.match(siteDecisionProblems(step.replace(/run: .*/, 'run: |\n          bash x'), runs).join(), /multi-line script/);
  assert.match(siteDecisionProblems(step.replace('github.event_name', 'x'), runs).join(), /event name/);
  assert.match(siteDecisionProblems(step.replace('github.base_ref', 'x'), runs).join(), /base branch/);
});

test('the site steps follow the repository\'s rules: no permission at the top or added, pinned actions, no deploy', () => {
  const wf = ciWorkflow();
  assert.match(wf, /^permissions: \{\}$/m);
  const lint = jobText(wf, 'lint');
  assert.match(lint, /^ {4}permissions:\n {6}contents: read\n/m);
  assert.equal((lint.match(/permissions:/g) ?? []).length, 1, 'the lint job adds no permission for the site');
  const uses = wf.split('\n').map((l) => l.trim()).filter((l) => l.startsWith('- uses:') || l.startsWith('uses:'));
  assert.ok(uses.length >= 2, `${uses.length} uses: lines`);
  for (const line of uses) assert.match(line, /^(?:- )?uses: [\w.-]+\/[\w./-]+@[0-9a-f]{40} # v\d+\.\d+\.\d+$/, line);
  assert.match(lint, /uses: actions\/setup-node@[0-9a-f]{40} # v\d+\.\d+\.\d+\n/);
  assert.match(lint, /persist-credentials: false/);
  const steps = siteSteps(wf).replace(/^\s*#.*$/gm, '');
  assert.equal(/id-token|environment:|aws |s3 |publish/i.test(steps), false, 'the site steps build and test; they do not publish');
});

test('the site workflow is gone: its steps are in ci.yml and nothing else runs the site', () => {
  assert.equal(existsSync(join(REPO, '.github', 'workflows', 'jobscheduler-site.yml')), false);
  assert.equal(jobText(ciWorkflow(), 'nope'), '');
  assert.equal(siteSteps('jobs:\n  lint:\n    steps:\n      - run: x\n'), '');
  assert.deepEqual(siteRuns('jobs:\n  lint:\n    steps:\n      - id: site\n        if: x\n'), []);
  assert.equal(siteStep('jobs:\n  lint:\n    steps:\n      - run: x\n'), '\n');
});

/** What is wrong with the test script: coverage is measured on the compiled tools and sources, and fails the run below 99%. */
function testScriptProblems(script: string): string[] {
  const out: string[] = [];
  if (!script.includes('--experimental-test-coverage')) out.push('does not measure coverage');
  for (const kind of ['lines', 'branches']) {
    const m = new RegExp(`--test-coverage-${kind}=(\\d+)(?!\\d)`).exec(script);
    if (!m) out.push(`no --test-coverage-${kind} threshold`);
    else if (Number(m[1]) < 99) out.push(`--test-coverage-${kind}=${m[1]} is below 99`);
  }
  if (!script.includes("--test-coverage-exclude='build/tools/test/**'")) out.push('does not exclude the tests from the measurement');
  return out;
}

test('npm test measures coverage and fails below 99% of lines and of branches', () => {
  assert.deepEqual(testScriptProblems(pkg.scripts.test), []);
});

test('the coverage threshold check can fail', () => {
  const ok = pkg.scripts.test as string;
  assert.match(testScriptProblems(ok.replace('--experimental-test-coverage', '')).join(), /does not measure coverage/);
  assert.match(testScriptProblems(ok.replace('--test-coverage-lines=99', '')).join(), /no --test-coverage-lines threshold/);
  assert.match(testScriptProblems(ok.replace('--test-coverage-branches=99', '--test-coverage-branches=90')).join(), /branches=90 is below 99/);
  assert.match(testScriptProblems(ok.replace("--test-coverage-exclude='build/tools/test/**'", '')).join(), /does not exclude the tests/);
});
