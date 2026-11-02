// The recorded demonstration on the landing page.
//
// Catches: a demo block on the landing page that is no longer the committed recording (the page edited by hand, or the
// recording replaced and the page not), a demo in the wrong place (it follows the install block and comes before "What
// it does"), a demo with no link to the install page, a client command in the recording that `js` does not take (a verb,
// a flag or an argument that crates/js-cli/src/lib.rs does not document, or a flag it requires and the command leaves
// out), a server flag that crates/js-lease-server/src/lib.rs does not document, and a script that does not reproduce
// the recording. The recording itself is the output of site/tools/demo.sh; no test runs it, because it builds the
// workspace.

import assert from 'node:assert/strict';
import { readFileSync, statSync } from 'node:fs';
import { join } from 'node:path';
import { test } from 'node:test';
import { REPO, SITE } from './support';

const transcript = readFileSync(join(SITE, 'test', 'demo.transcript'), 'utf8');
const indexHtml = readFileSync(join(SITE, 'content', 'index.html'), 'utf8');
const cliDocs = readFileSync(join(REPO, 'crates', 'js-cli', 'src', 'lib.rs'), 'utf8');
const serverDocs = readFileSync(join(REPO, 'crates', 'js-lease-server', 'src', 'lib.rs'), 'utf8');

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
}

/** The command shapes of the ```text block of the js-cli module documentation (a shape continues on indented lines). */
function documentedShapes(docs: string): Shape[] {
  const block = /\/\/! ```text\n([\s\S]*?)\/\/! ```/.exec(docs);
  if (!block) throw new Error('crates/js-cli/src/lib.rs has no ```text block');
  const entries: string[] = [];
  for (const raw of block[1].split('\n')) {
    const line = raw.replace(/^\/\/! ?/, '');
    if (line.startsWith('js ')) entries.push(line);
    else if (line.trim() !== '' && entries.length > 0) entries[entries.length - 1] += ' ' + line.trim();
  }
  const shapes = entries.map((entry) => {
    const head = entry.split(' -- ')[0].replace('<the flags of acquire>', ' ');
    // Optional parts are in [brackets]; what is left is required.
    const optional = head.match(/\[[^\]]*\]/g) ?? [];
    const requiredText = optional.reduce((text, o) => text.replace(o, ' '), head);
    const flagsOf = (text: string): string[] => text.match(/--[a-z][a-z-]*/g) ?? [];
    // A word that follows a flag is that flag's value, not a command word.
    const commandWords: string[] = [];
    const tokens = requiredText.split(/\s+/).filter((w) => w !== '');
    for (let i = 0; i < tokens.length; i++) {
      if (tokens[i].startsWith('--')) {
        if (tokens[i + 1] !== undefined && !tokens[i + 1].startsWith('--')) i++;
      } else if (!tokens[i].startsWith('<')) commandWords.push(tokens[i]);
    }
    return {
      words: commandWords,
      positionals: tokens.filter((t, i) => t.startsWith('<') && !(i > 0 && tokens[i - 1].startsWith('--'))).length,
      required: new Set(flagsOf(requiredText)),
      flags: new Set(flagsOf(head)),
    };
  });
  // `run` takes the flags of `acquire`.
  const acquire = shapes.find((s) => s.words.join(' ') === 'js lease acquire');
  const run = shapes.find((s) => s.words.join(' ') === 'js lease run');
  if (acquire && run) {
    for (const f of acquire.flags) run.flags.add(f);
    for (const f of acquire.required) run.required.add(f);
  }
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
    if (!globals.has(f) && !shape.flags.has(f)) problems.push(`${command}: ${f} is not a documented flag of "${words.join(' ')}"`);
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
  const edited = indexHtml.replace(/(<section class="band" id="demo">[\s\S]*?lease )(1-1)/, '$1' + '9-9');
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

test('the recording shows the server start, the host file, a lease acquired, listed, released and the list again', () => {
  assert.equal(serverCommands.length, 1);
  assert.match(transcript, /^# hosts\.toml/m);
  assert.deepEqual(
    clientCommands.map((c) => c.replace(/^js --server \S+ --token-file \S+ lease /, '').replace(/ .*/, '')),
    ['acquire', 'list', 'release', 'list'],
  );
  const acquired = /^lease (\S+) host (\S+) expires (\d+)$/m.exec(transcript);
  assert.ok(acquired, 'acquire printed lease <id> host <host> expires <ms>');
  assert.match(transcript, new RegExp(`^${acquired[1]} held host ${acquired[2]} expires ${acquired[3]}$`, 'm'));
  assert.match(transcript, new RegExp(`^lease ${acquired[1]} freed$`, 'm'));
  assert.ok(clientCommands.some((c) => c.endsWith(`lease release ${acquired[1]}`)));
});

test('every client command in the recording is a js command shape that crates/js-cli/src/lib.rs documents', () => {
  assert.ok(clientCommands.length >= 4);
  for (const c of clientCommands) assert.deepEqual(clientProblems(c, cliDocs), [], c);
});

test('a client command with an unknown verb, an unknown flag, a missing required flag, a missing id or a missing server is refused (the check can fail)', () => {
  const base = 'js --server http://127.0.0.1:7300 --token-file client-token';
  assert.match(clientProblems(`${base} lease grab --class gpu-host`, cliDocs).join('\n'), /no documented command "js lease grab"/);
  assert.match(clientProblems(`${base} lease acquire --class gpu-host --max-wall 1h --colour red`, cliDocs).join('\n'), /--colour is not a documented flag/);
  assert.match(clientProblems(`${base} lease acquire --class gpu-host`, cliDocs).join('\n'), /requires --max-wall/);
  assert.match(clientProblems(`${base} lease release`, cliDocs).join('\n'), /takes 1 argument\(s\), given 0/);
  assert.match(clientProblems(`${base} lease list extra`, cliDocs).join('\n'), /takes 0 argument\(s\), given 1/);
  assert.match(clientProblems('js --token-file client-token lease list', cliDocs).join('\n'), /--server is required/);
  assert.match(clientProblems('js --server http://127.0.0.1:7300 lease list', cliDocs).join('\n'), /--token-file is required/);
  assert.deepEqual(clientProblems(`${base} lease run --class gpu-host --max-wall 1h`, cliDocs), []);
  assert.deepEqual(clientProblems(`${base} lease release 1-1`, cliDocs), []);
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

test('site/tools/demo.sh is executable, builds the two programs, and runs each client verb of the recording', () => {
  const path = join(SITE, 'tools', 'demo.sh');
  assert.ok((statSync(path).mode & 0o111) !== 0, 'demo.sh is executable');
  const script = readFileSync(path, 'utf8');
  assert.match(script, /cargo build --release -p js-lease-server -p js-cli/);
  assert.match(script, /--boot initialise/);
  for (const verb of ['acquire', 'list', 'release']) assert.match(script, new RegExp(`lease ${verb}`), verb);
  assert.match(script, /kill "\$server_pid"/);
});
