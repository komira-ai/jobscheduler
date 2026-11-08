// The copy buttons of the command blocks (src/copy-core.ts, driven here with a fake document, clipboard and timer).
//
// Catches: a copied command that carries the prompt marks or the comments, loses a line of a multiline command or gains
// blank lines; a refused clipboard write that gives no feedback (or leaves the "Copied" state); a page without a
// clipboard that gets buttons that cannot work; clicks whose writes settle out of order, or whose timers fire late,
// that leave the button in the state of an earlier click; a command block of a real page whose copied text changes
// without the table of expected texts following; a transcript (the recorded demo) that gets a Copy button; and the built script no longer being the module the pages load.

import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { test } from 'node:test';
import { commandText, DONE_MS, enhance, type CopyBlock, type CopyButton, type CopyClipboard, type CopyDocument } from '../src/copy-core';
import { decodeAttr, readTags } from '../tools/check-dist';
import { buildReal, read, SITE } from './support';

// ---- a minimal fake DOM -----------------------------------------------------------------------------------------

/** A text run of a <pre>, with the class of the <span> around it ('' for text outside any span). */
interface Run {
  cls: string;
  text: string;
}

const ENTITIES: Record<string, string> = { '&lt;': '<', '&gt;': '>', '&amp;': '&', '&quot;': '"', '&#39;': "'" };
const decode = (s: string): string => s.replace(/&(?:lt|gt|amp|quot|#39);/g, (e) => ENTITIES[e]);

/** The runs of the inside of a <pre> as the content/*.html write it: text, <span class="x">text</span>, <code> around all. */
function runsOf(inner: string): Run[] {
  const body = inner.replace(/^\s*<code>/, '').replace(/<\/code>\s*$/, '');
  const runs: Run[] = [];
  let at = 0;
  for (const m of body.matchAll(/<span class="([^"]*)">([\s\S]*?)<\/span>/g)) {
    if (m.index > at) runs.push({ cls: '', text: decode(body.slice(at, m.index)) });
    runs.push({ cls: m[1], text: decode(m[2]) });
    at = m.index + m[0].length;
  }
  if (at < body.length) runs.push({ cls: '', text: decode(body.slice(at)) });
  return runs;
}

class FakePre {
  readonly runs: Run[];
  constructor(runs: Run[]) {
    this.runs = runs;
  }
  cloneNode(): FakePre {
    return new FakePre(this.runs.map((r) => ({ ...r })));
  }
  get textContent(): string {
    return this.runs.map((r) => r.text).join('');
  }
  querySelectorAll(selector: string): { remove(): void }[] {
    const wanted = selector.split(',').map((s) => s.trim().replace(/^\./, ''));
    return this.runs
      .filter((r) => wanted.includes(r.cls))
      .map((r) => ({
        remove: () => {
          this.runs.splice(this.runs.indexOf(r), 1);
        },
      }));
  }
}

class FakeButton implements CopyButton {
  type = '';
  className = '';
  textContent: string | null = null;
  readonly classes = new Set<string>();
  readonly classList = { add: (n: string) => void this.classes.add(n), remove: (n: string) => void this.classes.delete(n) };
  private listener: (() => void) | undefined;
  addEventListener(_type: 'click', listener: () => void): void {
    this.listener = listener;
  }
  click(): void {
    (this.listener as () => void)();
  }
}

class FakeBlock implements CopyBlock {
  readonly buttons: FakeButton[] = [];
  readonly classList: { contains(name: string): boolean };
  private readonly pre: FakePre | null;
  constructor(pre: FakePre | null, classes: string[] = []) {
    this.pre = pre;
    this.classList = { contains: (n) => classes.includes(n) };
  }
  querySelector(): FakePre | null {
    return this.pre;
  }
  appendChild(b: CopyButton): void {
    this.buttons.push(b as FakeButton);
  }
}

class FakeDoc implements CopyDocument {
  readonly blocks: FakeBlock[];
  constructor(blocks: FakeBlock[]) {
    this.blocks = blocks;
  }
  createElement(): FakeButton {
    return new FakeButton();
  }
  querySelectorAll(): FakeBlock[] {
    return this.blocks;
  }
}

/** A clipboard whose writes the test settles by hand, in any order. */
class ManualClipboard implements CopyClipboard {
  readonly written: string[] = [];
  private readonly settle: { ok: () => void; fail: () => void }[] = [];
  writeText(text: string): Promise<void> {
    this.written.push(text);
    return new Promise((ok, fail) => this.settle.push({ ok: () => ok(), fail: () => fail(new Error('denied')) }));
  }
  resolve(i: number): Promise<void> {
    this.settle[i].ok();
    return tick();
  }
  reject(i: number): Promise<void> {
    this.settle[i].fail();
    return tick();
  }
}

/** Lets the promise reactions run. */
const tick = (): Promise<void> => new Promise((r) => setImmediate(r));

class ManualTimer {
  readonly pending: { fn: () => void; ms: number }[] = [];
  readonly set = (fn: () => void, ms: number): number => this.pending.push({ fn, ms });
  fire(i: number): void {
    this.pending[i].fn();
  }
}

/** One page with one block of the given <pre> inner markup, enhanced; returns its button and the fakes. */
function page(inner: string, clipboard: CopyClipboard | undefined = new ManualClipboard()) {
  const block = new FakeBlock(new FakePre(runsOf(inner)));
  const timer = new ManualTimer();
  enhance(new FakeDoc([block]), clipboard, timer.set);
  return { block, button: block.buttons[0], clipboard: clipboard as ManualClipboard, timer };
}

const INSTALL = '<span class="p">$</span> sha256sum -c --ignore-missing SHA256SUMS\n<span class="p">$</span> js lease acquire --max-wall 2h';

// ---- what is copied ---------------------------------------------------------------------------------------------

test('a successful copy writes exactly the commands, one per line, without the prompt marks', async () => {
  const { button, clipboard } = page(INSTALL);
  button.click();
  await clipboard.resolve(0);
  assert.deepEqual(clipboard.written, ['sha256sum -c --ignore-missing SHA256SUMS\njs lease acquire --max-wall 2h\n']);
});

test('a multiline command with continuation lines is copied whole, and its inner indentation is only trimmed at the line start', async () => {
  const { button, clipboard } = page(
    '<span class="p">$</span> js-lease-server --listen 127.0.0.1:7300 \\\n    --state-file /var/lib/x.toml \\\n    --boot initialise',
  );
  button.click();
  await clipboard.resolve(0);
  assert.deepEqual(clipboard.written, ['js-lease-server --listen 127.0.0.1:7300 \\\n--state-file /var/lib/x.toml \\\n--boot initialise\n']);
});

test('the comment spans, the p spans and the prompt spans are removed and the entities are decoded', async () => {
  const { button, clipboard } = page(
    '<span class="c"># the pool</span>\n<span class="prompt">$ </span>js --token-file ~/.t lease acquire &gt; out &amp;&amp; true',
  );
  button.click();
  await clipboard.resolve(0);
  assert.deepEqual(clipboard.written, ['js --token-file ~/.t lease acquire > out && true\n']);
});

test('blank lines left by removed comments are collapsed and a plain block is copied as it reads', async () => {
  const { button, clipboard } = page('<span class="c"># one</span>\n\n\nls -l\n\n<span class="c"># two</span>\npwd');
  button.click();
  await clipboard.resolve(0);
  assert.deepEqual(clipboard.written, ['ls -l\npwd\n']);
});

test('the copied text is computed from a clone, so the block on the page keeps its prompts (the check can fail)', () => {
  const pre = new FakePre(runsOf(INSTALL));
  assert.match(commandText(pre), /^sha256sum/);
  assert.match(pre.textContent, /^\$ sha256sum/);
  // Negative: text with the marks left in is what a script that did not remove them would write.
  assert.notEqual(commandText(pre), pre.textContent.trim() + '\n');
});

test('every command block of the real pages copies without a prompt mark or a comment', () => {
  let blocks = 0;
  for (const page of ['index', 'concepts', 'install']) {
    for (const f of discover(contentOf(page))) {
      if (f.pre === null) continue;
      blocks++;
      const text = commandText(new FakePre(runsOf(f.pre)));
      assert.ok(text.endsWith('\n') && !text.endsWith('\n\n'), `${page}: ends with one newline`);
      for (const line of text.trimEnd().split('\n')) {
        assert.ok(!/^\s|^[$#]\s/.test(line), `${page}: "${line}" starts with a prompt, a comment or a space`);
      }
    }
  }
  assert.ok(blocks >= 5, `found only ${blocks} command blocks in the page copy`);
});

const contentOf = (pageName: string): string => readFileSync(join(SITE, 'content', `${pageName}.html`), 'utf8');

/** A start or end tag of a page with where it starts. */
interface Mark {
  open: boolean;
  name: string;
  attrs: [string, string][];
  at: number;
}

/** Every start and end tag of a page in order, the start tags read by the site's own tag reader (check-dist.readTags). */
function marks(html: string): Mark[] {
  assert.deepEqual(readTags(html).problems, [], 'the page is read without problems');
  return [...html.matchAll(/<(\/?)([a-zA-Z][^\s/>]*)/g)].map((m) => {
    const open = m[1] === '';
    // A start tag read from its own '<' onwards is the first tag the reader returns.
    const attrs = open ? readTags(html.slice(m.index)).tags[0][1] : [];
    return { open, name: m[2].toLowerCase(), attrs, at: m.index };
  });
}

/** The class tokens of a tag (its first class attribute, as in the browser), however the value is quoted. */
function classesOf(attrs: [string, string][]): string[] {
  const raw = attrs.find(([k]) => k === 'class')?.[1] ?? '';
  return (decodeAttr(raw) ?? raw).split(/\s+/).filter((c) => c !== '');
}

/** One element the browser's '.cmd, .code' selector matches: its classes and the inside of its first <pre> (null: none). */
interface Found {
  cls: string[];
  pre: string | null;
}

/** Every element of any name whose class has the token cmd or code, in page order, with the content of its first <pre>. */
function discover(html: string): Found[] {
  const all = marks(html);
  const found: Found[] = [];
  all.forEach((m, i) => {
    const cls = classesOf(m.attrs);
    if (!m.open || !cls.some((c) => c === 'cmd' || c === 'code')) return;
    let depth = 1;
    let end = html.length;
    for (const n of all.slice(i + 1)) {
      if (n.name !== m.name) continue;
      depth += n.open ? 1 : -1;
      if (depth === 0) {
        end = n.at;
        break;
      }
    }
    const pre = all.slice(i + 1).find((n) => n.open && n.name === 'pre' && n.at < end);
    let inner: string | null = null;
    if (pre) {
      const from = html.indexOf('>', pre.at) + 1;
      inner = html.slice(from, html.indexOf('</pre', from));
    }
    found.push({ cls, pre: inner });
  });
  return found;
}

/** How many elements of a page the browser's '.cmd, .code' selector matches, however the tag is written. */
const blockCount = (html: string): number => discover(html).length;

/** The blocks of a page the selector matches, as fakes (a block with no <pre> is one too: it gets no button). */
function realBlocks(pageName: string, html: string = contentOf(pageName)): { cls: string[]; block: FakeBlock }[] {
  return discover(html).map((f) => ({ cls: f.cls, block: new FakeBlock(f.pre === null ? null : new FakePre(runsOf(f.pre)), f.cls) }));
}

/** Enhances the blocks, clicks each button and returns the text each block wrote (null: no button). */
async function copiedFrom(blocks: FakeBlock[]): Promise<(string | null)[]> {
  const clipboard = new ManualClipboard();
  enhance(new FakeDoc(blocks), clipboard, new ManualTimer().set);
  const out: (string | null)[] = [];
  for (const b of blocks) {
    if (b.buttons.length === 0) {
      out.push(null);
      continue;
    }
    const at = clipboard.written.length;
    b.buttons[0].click();
    await clipboard.resolve(at);
    out.push(clipboard.written[at]);
  }
  return out;
}

/** The complete clipboard text of each block of a page, in page order (null: a transcript, which gets no button). */
const EXPECTED: Record<string, (string | null)[]> = {
  index: ["gh release download vX.Y.Z -R komira-ai/jobscheduler -p 'js-lease-server-*-x86_64-unknown-linux-musl.tar.gz' -p SHA256SUMS\n", null],
  concepts: [
    [
      'js lease acquire --class <class> --max-wall <dur> [--label k=v ...]',
      'js lease release <id>',
      'js lease list',
      '',
    ].join('\n'),
  ],
  install: [
    [
      "gh release download vX.Y.Z -R komira-ai/jobscheduler -p 'js-lease-server-*-x86_64-unknown-linux-musl.tar.gz' -p SHA256SUMS",
      'sha256sum -c --ignore-missing SHA256SUMS',
      'gh attestation verify js-lease-server-X.Y.Z-x86_64-unknown-linux-musl.tar.gz -R komira-ai/jobscheduler \\',
      '--signer-workflow komira-ai/jobscheduler/.github/workflows/artifacts.yml \\',
      '--source-ref refs/heads/main \\',
      '--source-digest "$(git rev-parse vX.Y.Z^{commit})"',
      '',
    ].join('\n'),
    ['name = "hosts"', 'class = "whole-node"', 'max_machines = 4', '[shape]', 'cpu_millis = 8000', 'memory_bytes = 17179869184', ''].join('\n'),
    [
      'js-lease-server --listen 127.0.0.1:7300 --state-file /var/lib/jobscheduler/leases.toml --boot initialise \\',
      '--token-file /etc/jobscheduler/client-token \\',
      '--agent-pool-config /etc/jobscheduler/agent-pool.toml \\',
      '--agent-token-file /etc/jobscheduler/join-tokens',
      '',
    ].join('\n'),
    [
      'js --server http://127.0.0.1:7300 --token-file ~/.js-token lease acquire --class whole-node --max-wall 2h',
      'js --server http://127.0.0.1:7300 --token-file ~/.js-token lease list',
      '',
    ].join('\n'),
  ],
};

for (const pageName of Object.keys(EXPECTED)) {
  test(`every block of ${pageName}.html that gets a button copies exactly the text in the table, and a transcript gets none`, async () => {
    const real = realBlocks(pageName);
    assert.equal(real.length, blockCount(contentOf(pageName)));
    const copied = await copiedFrom(real.map((r) => r.block));
    // A block that gets a button has an entry in the table and the other way round, so an added block fails until the table follows.
    assert.equal(copied.length, EXPECTED[pageName].length, `${pageName}: the page and the table list different blocks`);
    assert.deepEqual(copied, EXPECTED[pageName]);
    real.forEach((r, i) => assert.equal(r.cls.includes('transcript'), EXPECTED[pageName][i] === null, `${pageName} block ${i}: a transcript iff no button`));
  });
}

test('the demo of the landing page is a transcript with no Copy button', async () => {
  const demo = realBlocks('index').filter((b) => b.cls.includes('transcript'));
  assert.equal(demo.length, 1);
  assert.deepEqual(demo[0].cls, ['code', 'transcript']);
  await copiedFrom([demo[0].block]);
  assert.equal(demo[0].block.buttons.length, 0);
});

test('a changed copied text, or a transcript given a button, is caught (the check can fail)', async () => {
  // Negative: the real install page with one command changed no longer copies the table's text.
  const install = contentOf('install');
  const edited = install.replace('lease list</pre>', 'lease list --all</pre>');
  assert.notEqual(edited, install, 'the edit lands');
  assert.notDeepEqual(await copiedFrom(realBlocks('install', edited).map((r) => r.block)), EXPECTED.install);
  // Negative: a block written another way (another class first, an attribute) is still discovered, with the same text.
  const reshaped = install.replace('<div class="code"><pre><span class="p">$</span> js --server', '<div id="lease" class="wide code" ><pre><span class="p">$</span> js --server');
  assert.notEqual(reshaped, install, 'the reshape lands');
  assert.equal(blockCount(reshaped), blockCount(install));
  assert.deepEqual(await copiedFrom(realBlocks('install', reshaped).map((r) => r.block)), EXPECTED.install);
  // Negative: a transcript gets no button, and the same block unmarked does, so the table tells them apart.
  const marked = new FakeBlock(new FakePre(runsOf(INSTALL)), ['code', 'transcript']);
  const plain = new FakeBlock(new FakePre(runsOf(INSTALL)), ['code']);
  assert.deepEqual(await copiedFrom([marked]), [null]);
  assert.notDeepEqual(await copiedFrom([plain]), [null]);
  assert.equal(marked.buttons.length, 0);
  assert.equal(plain.buttons.length, 1);
});

test('blocks written another way are discovered, counted and copied, so the table must follow (the check can fail)', async () => {
  const pre = '<pre><span class="p">$</span> ls -l</pre>';
  const base = `<p>x</p><div class="code">${pre}</div>`;
  assert.equal(blockCount(base), 1);
  const variants: Record<string, string> = {
    'a single-quoted class': `<div class='code'>${pre}</div>`,
    'an unquoted class': `<div class=code>${pre}</div>`,
    'another element': `<section class="code">${pre}</section>`,
    'a second token': `<div class="wide  cmd">${pre}</div>`,
    'an upper-case tag and attribute': `<SECTION CLASS="code">${pre}</SECTION>`,
  };
  for (const [what, block] of Object.entries(variants)) {
    const html = base + block;
    assert.equal(blockCount(html), 2, `${what} is discovered`);
    const copied = await copiedFrom(realBlocks('x', html).map((r) => r.block));
    // The table of the page would need a second entry: with one entry the page and the table differ.
    assert.deepEqual(copied, ['ls -l\n', 'ls -l\n'], `${what} copies its text`);
    assert.notDeepEqual(copied, ['ls -l\n'], `${what}: a table of one entry fails`);
  }
  // A selected element with no <pre> inside is counted but gets no button; a <pre> after it is not its own.
  const bare = `<div class="code"><p>no pre</p></div><div class="other">${pre}</div>`;
  assert.equal(blockCount(bare), 1);
  assert.deepEqual(await copiedFrom(realBlocks('x', bare).map((r) => r.block)), [null]);
  // Not selected: a class that only contains the word; a nested element before the pre does not hide it.
  assert.equal(blockCount(`<div class="codec">${pre}</div><div class="x-cmd">${pre}</div>`), 0);
  const nested = `<div class="code"><div>inner</div>${pre}</div>`;
  assert.deepEqual(await copiedFrom(realBlocks('x', nested).map((r) => r.block)), ['ls -l\n']);
});

// ---- feedback ---------------------------------------------------------------------------------------------------

test('a block whose text content is null copies as one empty line, not as the word null', () => {
  const pre = { cloneNode: () => ({ textContent: null, querySelectorAll: () => [] }) };
  assert.equal(commandText(pre), '\n');
});

test('a block gets one Copy button, and a block without a pre gets none', () => {
  const withPre = new FakeBlock(new FakePre(runsOf('ls')));
  const without = new FakeBlock(null);
  enhance(new FakeDoc([withPre, without]), new ManualClipboard(), new ManualTimer().set);
  assert.equal(withPre.buttons.length, 1);
  assert.equal(withPre.buttons[0].textContent, 'Copy');
  assert.equal(withPre.buttons[0].type, 'button');
  assert.equal(withPre.buttons[0].className, 'copy');
  assert.equal(without.buttons.length, 0);
});

test('a successful copy shows Copied until the timer resets the label to Copy after the stated time', async () => {
  const { button, clipboard, timer } = page(INSTALL);
  button.click();
  assert.equal(button.textContent, 'Copy', 'nothing changes before the write settles');
  await clipboard.resolve(0);
  assert.equal(button.textContent, 'Copied');
  assert.ok(button.classes.has('done'));
  assert.equal(timer.pending.length, 1);
  assert.equal(timer.pending[0].ms, DONE_MS);
  timer.fire(0);
  assert.equal(button.textContent, 'Copy');
  assert.ok(!button.classes.has('done'));
});

test('a rejected write gives the failure feedback and never shows Copied (the check can fail)', async () => {
  const { button, clipboard, timer } = page(INSTALL);
  button.click();
  await clipboard.reject(0);
  assert.equal(button.textContent, 'Press Ctrl+C');
  assert.ok(!button.classes.has('done'));
  assert.equal(timer.pending.length, 0);
  // Negative: the same page with a write that succeeds does not show the failure text.
  const ok = page(INSTALL);
  ok.button.click();
  await ok.clipboard.resolve(0);
  assert.notEqual(ok.button.textContent, 'Press Ctrl+C');
});

test('a write that throws at once is a failed copy, not an unhandled error', async () => {
  const throwing: CopyClipboard = {
    writeText() {
      throw new Error('blocked');
    },
  };
  const { button } = page(INSTALL, throwing);
  button.click();
  await tick();
  assert.equal(button.textContent, 'Press Ctrl+C');
});

// ---- progressive enhancement ------------------------------------------------------------------------------------

test('without a clipboard no button is added and the page is left as it is (the check can fail)', () => {
  const block = new FakeBlock(new FakePre(runsOf(INSTALL)));
  const before = block.querySelector()?.textContent;
  enhance(new FakeDoc([block]), undefined, new ManualTimer().set);
  assert.equal(block.buttons.length, 0);
  assert.equal(block.querySelector()?.textContent, before);
  // Negative: with a clipboard the same page does get its button.
  const { block: enhanced } = page(INSTALL);
  assert.equal(enhanced.buttons.length, 1);
});

// ---- repeated clicks --------------------------------------------------------------------------------------------

test('two clicks whose writes settle out of order end in the state of the last click', async () => {
  // The first write fails late, after the second has succeeded: the button must stay Copied.
  const a = page(INSTALL);
  a.button.click();
  a.button.click();
  await a.clipboard.resolve(1);
  await a.clipboard.reject(0);
  assert.equal(a.button.textContent, 'Copied');
  assert.ok(a.button.classes.has('done'));
  // The first write succeeds late, after the second has failed: the button must stay on the failure text.
  const b = page(INSTALL);
  b.button.click();
  b.button.click();
  await b.clipboard.reject(1);
  await b.clipboard.resolve(0);
  assert.equal(b.button.textContent, 'Press Ctrl+C');
  assert.ok(!b.button.classes.has('done'));
  assert.equal(b.timer.pending.length, 0);
});

test('an earlier click\'s timer does not reset the label of a later click, and the later timer does (the check can fail)', async () => {
  const { button, clipboard, timer } = page(INSTALL);
  button.click();
  await clipboard.resolve(0);
  button.click();
  await clipboard.resolve(1);
  assert.equal(timer.pending.length, 2);
  timer.fire(0);
  assert.equal(button.textContent, 'Copied', 'the first click\'s timer is stale');
  assert.ok(button.classes.has('done'));
  timer.fire(1);
  assert.equal(button.textContent, 'Copy');
  assert.ok(!button.classes.has('done'));
});

test('a failure after a success replaces the Copied state, and each click writes the commands again', async () => {
  const { button, clipboard } = page(INSTALL);
  button.click();
  await clipboard.resolve(0);
  button.click();
  await clipboard.reject(1);
  assert.equal(button.textContent, 'Press Ctrl+C');
  assert.ok(!button.classes.has('done'));
  assert.equal(clipboard.written.length, 2);
  assert.equal(clipboard.written[0], clipboard.written[1]);
});

// ---- the built script -------------------------------------------------------------------------------------------

test('the built copy script imports the logic module with an extension a browser can load, and both ship', () => {
  const dist = buildReal('copy');
  const assets = readFileSync(join(dist, 'index.html'), 'utf8').match(/assets\/([^/"]+)\/copy\.js/);
  assert.ok(assets, 'index.html loads assets/<id>/copy.js');
  const script = read(dist, 'assets', assets[1], 'copy.js');
  assert.match(script, /from\s*['"]\.\/copy-core\.js['"]/);
  assert.ok(read(dist, 'assets', assets[1], 'copy-core.js').includes('Press Ctrl+C'));
});

// ---- the browser entry ------------------------------------------------------------------------------------------

/**
 * Loads the compiled src/copy.ts afresh with the given globals installed (the page reads them again at each click, so
 * they stay until the returned function restores the originals).
 */
function loadEntry(globals: { document: unknown; navigator: unknown; setTimeout: unknown }): () => void {
  const g = globalThis as unknown as Record<string, unknown>;
  const names = ['document', 'navigator', 'setTimeout'] as const;
  const saved = names.map((n) => Object.getOwnPropertyDescriptor(g, n));
  const file = require.resolve('../src/copy');
  const restore = (): void => {
    names.forEach((n, i) => (saved[i] ? Object.defineProperty(g, n, saved[i]) : Reflect.deleteProperty(g, n)));
    Reflect.deleteProperty(require.cache, file);
  };
  try {
    for (const n of names) Object.defineProperty(g, n, { value: globals[n], configurable: true, writable: true });
    Reflect.deleteProperty(require.cache, file);
    require(file);
  } catch (e) {
    restore();
    throw e;
  }
  return restore;
}

test('the browser entry adds a Copy button to each command block of the page, and a click writes the commands and arms the page timer', async () => {
  const block = new FakeBlock(new FakePre(runsOf(INSTALL)));
  const clipboard = new ManualClipboard();
  const timers: { fn: () => void; ms: number }[] = [];
  const restore = loadEntry({
    document: new FakeDoc([block]),
    navigator: { clipboard },
    setTimeout: (fn: () => void, ms: number) => timers.push({ fn, ms }),
  });
  try {
    assert.equal(block.buttons.length, 1);
    assert.equal(block.buttons[0].textContent, 'Copy');
    block.buttons[0].click();
    await clipboard.resolve(0);
    assert.deepEqual(clipboard.written, ['sha256sum -c --ignore-missing SHA256SUMS\njs lease acquire --max-wall 2h\n']);
    assert.equal(block.buttons[0].textContent, 'Copied');
    assert.deepEqual(timers.map((t) => t.ms), [DONE_MS], 'the reset runs on the page\'s own setTimeout');
    timers[0].fn();
    assert.equal(block.buttons[0].textContent, 'Copy');
  } finally {
    restore();
  }
});

test('the browser entry does nothing on a page without a clipboard (the check can fail)', () => {
  const none = new FakeBlock(new FakePre(runsOf(INSTALL)));
  loadEntry({ document: new FakeDoc([none]), navigator: {}, setTimeout: () => 0 })();
  assert.equal(none.buttons.length, 0);
  // Negative: the same page with a clipboard is enhanced.
  const some = new FakeBlock(new FakePre(runsOf(INSTALL)));
  loadEntry({ document: new FakeDoc([some]), navigator: { clipboard: new ManualClipboard() }, setTimeout: () => 0 })();
  assert.equal(some.buttons.length, 1);
});
