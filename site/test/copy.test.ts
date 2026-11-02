// The copy buttons of the command blocks (src/copy-core.ts, driven here with a fake document, clipboard and timer).
//
// Catches: a copied command that carries the prompt marks or the comments, loses a line of a multiline command or gains
// blank lines; a refused clipboard write that gives no feedback (or leaves the "Copied" state); a page without a
// clipboard that gets buttons that cannot work; clicks whose writes settle out of order, or whose timers fire late,
// that leave the button in the state of an earlier click; and the built script no longer being the module the pages load.

import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { test } from 'node:test';
import { commandText, DONE_MS, enhance, type CopyBlock, type CopyButton, type CopyClipboard, type CopyDocument } from '../src/copy-core';
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
  private readonly pre: FakePre | null;
  constructor(pre: FakePre | null) {
    this.pre = pre;
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
    const html = readFileSync(join(SITE, 'content', `${page}.html`), 'utf8');
    for (const m of html.matchAll(/<div class="(?:cmd|code)\b[^"]*"><pre>([\s\S]*?)<\/pre>/g)) {
      blocks++;
      const text = commandText(new FakePre(runsOf(m[1])));
      assert.ok(text.endsWith('\n') && !text.endsWith('\n\n'), `${page}: ends with one newline`);
      for (const line of text.trimEnd().split('\n')) {
        assert.ok(!/^\s|^[$#]\s/.test(line), `${page}: "${line}" starts with a prompt, a comment or a space`);
      }
    }
  }
  assert.ok(blocks >= 5, `found only ${blocks} command blocks in the page copy`);
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
