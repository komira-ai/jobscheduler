// The manifest of a built tree: every file's path and sha256, one per line, sorted. test/golden.manifest is its
// committed copy; `npm run golden` rewrites it from a fresh build after a change meant to alter the output.

import { createHash } from 'crypto';
import { readFileSync, writeFileSync } from 'fs';
import { join, resolve } from 'path';
import { buildSite } from './build';
import { listFiles } from './check-dist';

export function manifestOf(dir: string): string {
  return listFiles(dir).map((f) => `${createHash('sha256').update(readFileSync(join(dir, f))).digest('hex')}  ${f}\n`).join('');
}

/**
 * The command line: builds the site under `site` into site/build/golden-dist and writes its manifest to `argv[0]`, or to
 * site/test/golden.manifest without one.
 */
export function main(site: string, argv: readonly string[]): void {
  const out = join(site, 'build', 'golden-dist');
  buildSite({ browserDir: join(site, 'build', 'browser'), staticDir: join(site, 'static'), contentDir: join(site, 'content'), outDir: out });
  writeFileSync(argv[0] === undefined ? join(site, 'test', 'golden.manifest') : resolve(argv[0]), manifestOf(out));
}

if (require.main === module) main(resolve(__dirname, '..', '..', '..'), process.argv.slice(2));
