// The browser module graph the build ships (moduleGraph in tools/build.ts), on small synthetic inputs.
//
// Catches: a build that ships the wrong set of modules. A missing entry or a missing transitive import must stop the
// build with a message naming what is missing (and, for an import, who asked for it), not ship a page whose script
// fails to load; a cycle between modules must end, with each module shipped once; a module two others import must be
// shipped once, not twice; and a module no entry reaches must not ship at all. The real site has none of these cases
// today, so only synthetic graphs can show the code handles them.

import assert from 'node:assert/strict';
import { test } from 'node:test';
import { moduleGraph } from '../tools/build';

/** A file map from module path to source; `deps` become import lines. */
function graph(spec: Record<string, string[]>): Map<string, Buffer> {
  const files = new Map<string, Buffer>();
  for (const [path, deps] of Object.entries(spec)) {
    files.set(path, Buffer.from(deps.map((d) => `import { x } from '${d}';\n`).join('') + 'export const y = 1;\n'));
  }
  return files;
}

test('a missing entry is reported by name', () => {
  assert.throws(() => moduleGraph(graph({ 'a.js': [] }), ['a.js', 'gone.js']), /entry gone\.js is missing/);
});

test('a missing transitive import is reported with its importer and the import', () => {
  const js = graph({ 'main.js': ['./lib/a.js'], 'lib/a.js': ['./b.js'] });
  assert.throws(() => moduleGraph(js, ['main.js']), /lib\/a\.js imports lib\/b\.js, which was not compiled/);
});

test('a present import does not raise the missing-import error (the check can fail)', () => {
  const js = graph({ 'main.js': ['./lib/a.js'], 'lib/a.js': ['./b.js'], 'lib/b.js': [] });
  assert.deepEqual(moduleGraph(js, ['main.js']), ['lib/a.js', 'lib/b.js', 'main.js']);
});

test('a cycle terminates and each module in it is emitted once', () => {
  const js = graph({ 'a.js': ['./b.js'], 'b.js': ['./c.js'], 'c.js': ['./a.js', './b.js'] });
  assert.deepEqual(moduleGraph(js, ['a.js']), ['a.js', 'b.js', 'c.js']);
});

test('a module that imports itself is emitted once', () => {
  assert.deepEqual(moduleGraph(graph({ 'a.js': ['./a.js'] }), ['a.js']), ['a.js']);
});

test('a module imported by two others is emitted once', () => {
  const js = graph({ 'a.js': ['./shared.js', './b.js'], 'b.js': ['./shared.js'], 'shared.js': [] });
  const out = moduleGraph(js, ['a.js']);
  assert.deepEqual(out, ['a.js', 'b.js', 'shared.js']);
  assert.equal(out.filter((m) => m === 'shared.js').length, 1);
});

test('an entry that another entry imports is emitted once', () => {
  const js = graph({ 'a.js': ['./b.js'], 'b.js': [] });
  assert.deepEqual(moduleGraph(js, ['a.js', 'b.js']), ['a.js', 'b.js']);
});

test('a module no entry reaches is not emitted', () => {
  const js = graph({ 'a.js': ['./b.js'], 'b.js': [], 'orphan.js': ['./b.js'], 'lonely.js': [] });
  assert.deepEqual(moduleGraph(js, ['a.js']), ['a.js', 'b.js']);
});

test('a module an entry imports is emitted, so the orphan test is not vacuous (the check can fail)', () => {
  const js = graph({ 'a.js': ['./orphan.js'], 'orphan.js': [] });
  assert.deepEqual(moduleGraph(js, ['a.js']), ['a.js', 'orphan.js']);
});
