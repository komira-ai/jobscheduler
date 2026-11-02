// Shared by the tests that look at how ci.yml runs the site: the `lint` job's site steps, their decision command, and
// site/ci-paths, the one list of the paths whose change runs them.

import { readFileSync } from 'fs';
import { join } from 'path';
import { REPO, SITE } from './support';

/** The text of .github/workflows/ci.yml. */
export function ciWorkflow(): string {
  return readFileSync(join(REPO, '.github', 'workflows', 'ci.yml'), 'utf8');
}

/** The job `name` of a workflow: from its header line to the line before the next job (or the end). */
export function jobText(yml: string, name: string): string {
  const lines = yml.split('\n');
  const start = lines.findIndex((l) => l === `  ${name}:`);
  if (start < 0) return '';
  const end = lines.findIndex((l, i) => i > start && /^ {2}\S/.test(l) && !l.startsWith('  #'));
  return lines.slice(start, end < 0 ? undefined : end).join('\n') + '\n';
}

/** The steps of the `lint` job that belong to the site: from the step `id: site` (the decision) to the end of the job. */
export function siteSteps(yml: string): string {
  const lint = jobText(yml, 'lint');
  const at = lint.indexOf('      - id: site\n');
  return at < 0 ? '' : lint.slice(at);
}

/** The step `id: site` of the `lint` job: from its `- id: site` line to the line before the next step. */
export function siteStep(yml: string): string {
  const lines = siteSteps(yml).split('\n');
  const end = lines.findIndex((l, i) => i > 0 && l.startsWith('      - '));
  return lines.slice(0, end < 0 ? undefined : end).join('\n').trimEnd() + '\n';
}

/** The `run:` lines of the step `id: site`, each with its text after `run: `. */
export function siteRuns(yml: string): string[] {
  return siteStep(yml).split('\n').filter((l) => l.trim().startsWith('run:')).map((l) => l.trim().slice('run:'.length).trim());
}

/** The one command the step `id: site` runs: the decision is `cargo xtask site-changed` (crates/xtask/src/site_changed.rs), which reads site/ci-paths. */
export const SITE_COMMAND = 'cargo xtask site-changed --event "$EVENT" --base "$BASE" --paths site/ci-paths --github-output "$GITHUB_OUTPUT"';

/** The entries of a ci-paths file: its lines without comments and blanks, trimmed. */
export function ciPathEntries(text: string): string[] {
  return text.split('\n').map((l) => l.trim()).filter((l) => l !== '' && !l.startsWith('#'));
}

/** The entries of site/ci-paths. */
export function ciPaths(): string[] {
  return ciPathEntries(readFileSync(join(SITE, 'ci-paths'), 'utf8'));
}

/** Whether an entry of ci-paths matches a '/'-separated file path: a directory prefix ending in '/' matches every file under it, anything else matches that file only. */
export function pathMatches(entry: string, file: string): boolean {
  return entry.endsWith('/') ? file.startsWith(entry) : file === entry;
}
