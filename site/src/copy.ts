// Copy buttons for command blocks (.cmd and .code, not a .transcript). Progressive enhancement: the pages read the same without it.
// The logic is in copy-core.ts; this wires it to the page.

import { enhance, type CopyClipboard, type CopyDocument } from './copy-core.js';

enhance(document as unknown as CopyDocument, navigator.clipboard as CopyClipboard | undefined, (fn, ms) => setTimeout(fn, ms));
