// The recorded demonstration on the landing page.
//
// Catches: a demo block on the landing page that is no longer the committed recording (the page edited by hand, or the
// recording replaced and the page not), a demo in the wrong place (it follows the install block and comes before "What
// it does"), a demo with no link to the install page, a client command in the recording that `js` does not take (a verb,
// a flag or an argument that crates/js-cli/src/lib.rs does not document, or a flag it requires and the command leaves
// out), a server flag that crates/js-lease-server/src/lib.rs does not document, and a recording whose commands are no
// longer the ones `cargo xtask site-demo` (crates/xtask/src/site_demo.rs) runs. The recording itself is the output of
// that command; no site test runs it, because it builds the workspace. The Rust test
// `committed_transcript_shows_the_commands_this_verb_runs` (crates/xtask/src/site_demo/tests.rs) reads this transcript and
// compares every `$` line with the commands the Rust code builds, so a command changed in one place and not the other
// fails there; the tests here hold the other side: that the Rust source still has every word of every recorded command,
// and that the Rust test is still there and still reads this file.

import assert from 'node:assert/strict';
import { existsSync, readFileSync } from 'node:fs';
import { join } from 'node:path';
import { test } from 'node:test';
import { REPO, SITE } from './support';

const transcript = readFileSync(join(SITE, 'test', 'demo.transcript'), 'utf8');
const indexHtml = readFileSync(join(SITE, 'content', 'index.html'), 'utf8');
const cliDocs = readFileSync(join(REPO, 'crates', 'js-cli', 'src', 'lib.rs'), 'utf8');
const serverDocs = readFileSync(join(REPO, 'crates', 'js-lease-server', 'src', 'lib.rs'), 'utf8');
const demoSource = readFileSync(join(REPO, 'crates', 'xtask', 'src', 'site_demo.rs'), 'utf8');
const demoRustTest = readFileSync(join(REPO, 'crates', 'xtask', 'src', 'site_demo', 'tests.rs'), 'utf8');
const xtaskDocs = readFileSync(join(REPO, 'crates', 'xtask', 'src', 'lib.rs'), 'utf8');

/** The text of the demo block of a page: the <pre> inside the section with id "demo", markup removed and entities read. */
function demoText(html: string): string {
  const section = /<section class="band" id="demo">([\s\S]*?)<\/section>/.exec(html);
  if (!section) throw new Error('the page has no <section class="band" id="demo">');
  const pre = /<div class="code transcript"><pre>([\s\S]*?)<\/pre><\/div>/.exec(section[1]);
  if (!pre) throw new Error('the demo section has no <div class="code transcript"><pre> block');
  return pre[1]
    .replace(/<[^>]*>/g, '')
    .replace(/&lt;/g, '<')
    .replace(/&gt;/g, '>')
    .replace(/&amp;/g, '&');
}

/** What the documentation of a command line allows. */
interface Shape {
  words: string[];
  positionals: number;
  required: Set<string>;
  flags: Set<string>;
  /** The shape ends in "...": the flags it lists are some of the command's, and `js <command> --help` has the rest. */
  open: boolean;
}

/**
 * The command shapes of the ```text blocks of the js-cli module documentation (a shape continues on indented lines, and
 * a line may hold two shapes, set apart by more than one space).
 */
function documentedShapes(docs: string): Shape[] {
  const blocks = [...docs.matchAll(/\/\/! ```text\n([\s\S]*?)\/\/! ```/g)];
  if (blocks.length === 0) throw new Error('crates/js-cli/src/lib.rs has no ```text block');
  const entries: string[] = [];
  for (const block of blocks) {
    for (const raw of block[1].split('\n')) {
      const line = raw.replace(/^\/\/! ?/, '');
      if (line.startsWith('js ')) entries.push(...line.split(/\s{2,}(?=js )/));
      else if (line.trim() !== '' && entries.length > 0) entries[entries.length - 1] += ' ' + line.trim();
    }
  }
  const shapes = entries.map((entry) => {
    const head = entry.split(' -- ')[0].replace('<the flags of job submit>', ' ');
    // Optional parts are in [brackets]; what is left is required.
    const optional = head.match(/\[[^\]]*\]/g) ?? [];
    const requiredText = optional.reduce((text, o) => text.replace(o, ' '), head);
    const flagsOf = (text: string): string[] => text.match(/--[a-z][a-z0-9.-]*/g) ?? [];
    // A word that follows a flag is that flag's value, not a command word.
    const commandWords: string[] = [];
    const tokens = requiredText.split(/\s+/).filter((w) => w !== '');
    for (let i = 0; i < tokens.length; i++) {
      if (tokens[i].startsWith('--')) {
        if (tokens[i + 1] !== undefined && !tokens[i + 1].startsWith('--')) i++;
      } else if (!tokens[i].startsWith('<') && tokens[i] !== '...') commandWords.push(tokens[i]);
    }
    return {
      words: commandWords,
      positionals: tokens.filter((t, i) => t.startsWith('<') && !(i > 0 && tokens[i - 1].startsWith('--'))).length,
      required: new Set(flagsOf(requiredText)),
      flags: new Set(flagsOf(head)),
      open: tokens[tokens.length - 1] === '...',
    };
  });
  return shapes;
}

/** The global flags of the flags table of the module documentation: [flag, takes a value]. */
function globalFlags(docs: string): Map<string, boolean> {
  const flags = new Map<string, boolean>();
  for (const m of docs.matchAll(/^\/\/! \| `(--[a-z-]+)( <[a-z]+>)?` \|/gm)) flags.set(m[1], m[2] !== undefined);
  return flags;
}

/** Why a client command is not a documented shape; an empty list when it is. */
function clientProblems(command: string, docs: string): string[] {
  const globals = globalFlags(docs);
  const shapes = documentedShapes(docs);
  const tokens = command.split(/\s+/).filter((t) => t !== '');
  const problems: string[] = [];
  if (tokens[0] !== 'js') return [`not a js command: ${command}`];
  const used = new Set<string>();
  const words: string[] = ['js'];
  let positionals = 0;
  for (let i = 1; i < tokens.length; i++) {
    const t = tokens[i];
    if (t.startsWith('--')) {
      used.add(t);
      if (globals.get(t) === true || (!globals.has(t) && tokens[i + 1] !== undefined && !tokens[i + 1].startsWith('--'))) i++;
    } else if (words.length < 3) words.push(t);
    else positionals++;
  }
  for (const need of ['--server', '--token-file']) {
    if (globals.has(need) && !used.has(need)) problems.push(`${command}: the global flag ${need} is required and missing`);
  }
  const shape = shapes.find((s) => s.words.join(' ') === words.join(' '));
  if (!shape) return [...problems, `${command}: no documented command "${words.join(' ')}"`];
  for (const f of used) {
    if (!globals.has(f) && !shape.open && !shape.flags.has(f)) problems.push(`${command}: ${f} is not a documented flag of "${words.join(' ')}"`);
  }
  for (const f of shape.required) if (!used.has(f)) problems.push(`${command}: "${words.join(' ')}" requires ${f}`);
  if (positionals !== shape.positionals) problems.push(`${command}: "${words.join(' ')}" takes ${shape.positionals} argument(s), given ${positionals}`);
  return problems;
}

/** The commands of a transcript: the text after each "$ " prompt. */
const commandsOf = (text: string): string[] => text.split('\n').filter((l) => l.startsWith('$ ')).map((l) => l.slice(2));

/** Why a server command line names a flag the server documentation does not: the flags of its table. */
function serverProblems(command: string, docs: string): string[] {
  const documented = new Set([...docs.matchAll(/^\/\/! \| `(--[a-z-]+)/gm)].map((m) => m[1]));
  return (command.match(/--[a-z][a-z-]*/g) ?? []).filter((f) => !documented.has(f)).map((f) => `${command}: ${f} is not a documented flag`);
}

const clientCommands = commandsOf(transcript).filter((c) => c.startsWith('js '));
const serverCommands = commandsOf(transcript).filter((c) => c.startsWith('js-lease-server '));

test('the demo block of the landing page is the committed recording, word for word', () => {
  assert.equal(demoText(indexHtml) + '\n', transcript);
});

test('a demo block that differs from the recording by one character is found (the check can fail)', () => {
  const edited = indexHtml.replace(/(<section class="band" id="demo">[\s\S]*?--spec\.needs\.gpus )(1)/, '$1' + '2');
  assert.notEqual(edited, indexHtml, 'the damage landed');
  assert.notEqual(demoText(edited) + '\n', transcript);
});

test('a page without a demo section, or a demo section without a code block, is refused (the check can fail)', () => {
  assert.throws(() => demoText(indexHtml.replace('id="demo"', 'id="other"')), /no <section class="band" id="demo">/);
  assert.throws(() => demoText(indexHtml.replace(/(id="demo">[\s\S]*?)<div class="code transcript">/, '$1<div>')), /no <div class="code transcript"><pre> block/);
});

test('the demo comes after the install block of the hero and before "What it does", and links to the install page', () => {
  const install = indexHtml.indexOf('class="install-block"');
  const demo = indexHtml.indexOf('id="demo"');
  const what = indexHtml.indexOf('id="what"');
  assert.ok(install >= 0 && demo > install && what > demo, `install ${install}, demo ${demo}, what ${what}`);
  const section = /<section class="band" id="demo">([\s\S]*?)<\/section>/.exec(indexHtml)![1];
  assert.match(section, /<a href="install\.html">/);
});

test('the recording shows the pool file, the server start, a job submitted and refused, and the list of jobs', () => {
  assert.equal(serverCommands.length, 1);
  assert.match(transcript, /^# pool\.toml/m);
  assert.match(serverCommands[0], / --agent-pool-config pool\.toml --job-listen 127\.0\.0\.1:7301 &$/);
  assert.deepEqual(
    clientCommands.map((c) => c.replace(/^js --server \S+ --token-file \S+ job /, '').replace(/ .*/, '')),
    ['submit', 'list'],
  );
  // No agent joins the pool: the job is refused for want of a host, and the list that follows is empty.
  const lines = transcript.split('\n');
  const submitted = lines.findIndex((l) => l.startsWith('$ js ') && l.includes(' job submit '));
  assert.equal(lines[submitted + 1], 'js: refused: no-capable-host: no host and no catalog could ever serve the job');
  assert.match(lines[submitted + 2], / job list$/);
  assert.deepEqual(lines.slice(submitted + 3), ['']);
});

test('every client command in the recording is a js command shape that crates/js-cli/src/lib.rs documents', () => {
  assert.ok(clientCommands.length >= 2);
  for (const c of clientCommands) assert.deepEqual(clientProblems(c, cliDocs), [], c);
});

test('a client command with an unknown verb, an unknown flag, a missing required flag, a missing id or a missing server is refused (the check can fail)', () => {
  const base = 'js --server http://127.0.0.1:7301 --token-file client-token';
  assert.match(clientProblems(`${base} job grab --job-id x`, cliDocs).join('\n'), /no documented command "js job grab"/);
  assert.match(clientProblems(`${base} job get --job-id x --colour red`, cliDocs).join('\n'), /--colour is not a documented flag/);
  assert.match(clientProblems(`${base} job submit --id x`, cliDocs).join('\n'), /requires --spec\.image/);
  assert.match(clientProblems(`${base} job get`, cliDocs).join('\n'), /requires --job-id/);
  assert.match(clientProblems(`${base} job list extra`, cliDocs).join('\n'), /takes 0 argument\(s\), given 1/);
  assert.match(clientProblems('js --token-file client-token job list', cliDocs).join('\n'), /--server is required/);
  assert.match(clientProblems('js --server http://127.0.0.1:7301 job list', cliDocs).join('\n'), /--token-file is required/);
  assert.match(clientProblems(`${base} lease list`, cliDocs).join('\n'), /no documented command "js lease list"/);
  assert.deepEqual(clientProblems(`${base} job cancel --job-id x`, cliDocs), []);
  assert.deepEqual(clientProblems(`${base} job submit --spec.image x --spec.needs.gpus 1`, cliDocs), []);
});

test('the server start in the recording names only flags that crates/js-lease-server/src/lib.rs documents', () => {
  for (const c of serverCommands) assert.deepEqual(serverProblems(c, serverDocs), [], c);
  assert.match(serverProblems('js-lease-server --turbo', serverDocs).join('\n'), /--turbo is not a documented flag/);
});

test('the recording keeps to the address 127.0.0.1 and to relative paths', () => {
  const addresses = [...transcript.matchAll(/\b\d{1,3}\.\d{1,3}\.\d{1,3}\.\d{1,3}\b/g)].map((m) => m[0]);
  assert.ok(addresses.length > 0);
  assert.ok(addresses.every((a) => a === '127.0.0.1'), addresses.join(' '));
  assert.doesNotMatch(transcript, /(^|[\s=])\/(home|Users|root|tmp|var)\//);
});

/** Comment text of a Rust file: the `//`, `//!` and `///` prefixes dropped and white space collapsed. */
const prose = (source: string): string => source.replace(/^\s*\/\/[!/]? ?/gm, '').replace(/\s+/g, ' ');

/** The words of a recorded `js` or `js-lease-server` command that the Rust source does not spell out as a string literal. */
function wordsMissingFromSource(command: string, source: string): string[] {
  const missing: string[] = [];
  for (const word of command.split(/\s+/).slice(1).filter((w) => w !== '' && w !== '&')) {
    if (source.includes(`"${word}"`)) continue;
    // The addresses are the default job port, which the source names once.
    const jobPort = /DEFAULT_JOB_PORT: u16 = 7301/.test(source);
    if ((word === 'http://127.0.0.1:7301' || word === '127.0.0.1:7301') && jobPort) continue;
    missing.push(word);
  }
  return missing;
}

test('crates/xtask/src/site_demo.rs spells out every word of every command in the recording', () => {
  const programs = commandsOf(transcript).filter((c) => /^js(-lease-server)? /.test(c));
  assert.equal(programs.length, 3);
  for (const c of programs) assert.deepEqual(wordsMissingFromSource(c, demoSource), [], c);
  for (const program of ['js-lease-server', 'js']) assert.match(demoSource, new RegExp(`"${program}"`), program);
  assert.match(demoSource, /DEFAULT_PORT: u16 = 7300/);
  assert.match(demoSource, /DEFAULT_JOB_PORT: u16 = 7301/);
  // The command that writes the token file is built from the token and the file name the source holds.
  const token = commandsOf(transcript).find((c) => c.startsWith('printf '))!;
  assert.match(token, /a-local-token-of-at-least-32-bytes/);
  assert.match(demoSource, /TOKEN: &str = "a-local-token-of-at-least-32-bytes"/);
});

test('a recorded command with a word the Rust source does not have is found (the check can fail)', () => {
  assert.deepEqual(wordsMissingFromSource('js job submit --id demo-1 --turbo', demoSource), ['--turbo']);
  assert.deepEqual(wordsMissingFromSource('js job list', 'no such source'), ['job', 'list']);
});

test('the command the page names is a verb of xtask, and the Rust test that holds the commands to this recording is there', () => {
  assert.match(prose(demoSource), /It builds the server and the client, starts the server on this machine with the job service on, asks the job service for a GPU job and lists the jobs/);
  assert.match(prose(xtaskDocs), /`site-demo \[--out <path>\]/);
  assert.match(demoRustTest, /fn committed_transcript_shows_the_commands_this_verb_runs\(\)/);
  assert.match(demoRustTest, /site\/test\/demo\.transcript/);
  assert.match(demoRustTest, /assert_eq!\(shown, commands\(DEFAULT_PORT, DEFAULT_JOB_PORT\)\)/);
  const section = /<section class="band" id="demo">([\s\S]*?)<\/section>/.exec(indexHtml)![1];
  assert.match(section, /<code>cargo xtask site-demo<\/code>/);
  assert.doesNotMatch(section, /demo\.sh/);
});

test('site/tools/demo.sh is gone: no shell script holds the recording logic', () => {
  assert.equal(existsSync(join(SITE, 'tools', 'demo.sh')), false);
});
