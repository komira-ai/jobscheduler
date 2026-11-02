// The logic of the copy buttons (src/copy.ts wires it to the page). It takes the document, the clipboard and the timer
// as arguments, so the tests drive it with fakes; the interfaces below are the few members it uses, which the real
// DOM, navigator.clipboard and setTimeout have.
// No inline style, no markup from strings.

export interface CopyClipboard {
  writeText(text: string): Promise<void>;
}
export type CopyTimer = (fn: () => void, ms: number) => unknown;
export interface CopyButton {
  type: string;
  className: string;
  textContent: string | null;
  classList: { add(name: string): void; remove(name: string): void };
  addEventListener(type: 'click', listener: () => void): void;
}
export interface CopyClone {
  textContent: string | null;
  querySelectorAll(selector: string): Iterable<{ remove(): void }>;
}
export interface CopyPre {
  cloneNode(deep: boolean): CopyClone;
}
export interface CopyBlock {
  querySelector(selector: 'pre'): CopyPre | null;
  appendChild(button: CopyButton): unknown;
}
export interface CopyDocument {
  createElement(tag: 'button'): CopyButton;
  querySelectorAll(selector: string): Iterable<CopyBlock>;
}

export const DONE_MS = 1500;

/** The commands of a block, not the prompt marks or the comments: one command per line, a final newline. */
export function commandText(pre: CopyPre): string {
  const clone = pre.cloneNode(true);
  for (const n of clone.querySelectorAll('.p, .c, .prompt')) n.remove();
  return (clone.textContent ?? '').replace(/^[ \t]+/gm, '').replace(/\n{2,}/g, '\n').trim() + '\n';
}

function addCopyButton(block: CopyBlock, doc: CopyDocument, clipboard: CopyClipboard, timer: CopyTimer): void {
  const pre = block.querySelector('pre');
  if (!pre) return;
  const button = doc.createElement('button');
  button.type = 'button';
  button.className = 'copy';
  button.textContent = 'Copy';
  // Only the latest click may change the button: an earlier click that settles late, or its timer, would otherwise
  // overwrite the state of the click the reader last made.
  let latest = 0;
  button.addEventListener('click', () => {
    const click = ++latest;
    const text = commandText(pre);
    new Promise<void>((resolve) => resolve(clipboard.writeText(text))).then(
      () => {
        if (click !== latest) return;
        button.textContent = 'Copied';
        button.classList.add('done');
        timer(() => {
          if (click !== latest) return;
          button.textContent = 'Copy';
          button.classList.remove('done');
        }, DONE_MS);
      },
      () => {
        if (click !== latest) return;
        button.classList.remove('done');
        button.textContent = 'Press Ctrl+C';
      },
    );
  });
  block.appendChild(button);
}

/** Adds a Copy button to each command block. Without a clipboard the page is left as it is. */
export function enhance(doc: CopyDocument, clipboard: CopyClipboard | undefined, timer: CopyTimer): void {
  if (!clipboard) return;
  for (const block of doc.querySelectorAll('.cmd, .code')) addCopyButton(block, doc, clipboard, timer);
}
