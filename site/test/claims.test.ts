// The statements of the pages, held to the repository's code and documents.
//
// Catches: a page that says something the source no longer says, and a statement that something is not built after the
// code built it. Every row below pairs a phrase of a page (content/<name>.html) with the phrase of a source file it
// rests on (comment markers dropped and whitespace collapsed, so a wrapped line reads as one). When the source changes,
// the test fails and names the page and the phrase to revisit; when a page is reworded, it fails until the row follows.
// The site lives in this repository so that these files are read as they are, not copied.
//
// Also held here: the "not built" and "not available" statements (each is a row), the keys of the pool file the install
// page shows, the whole-node keys the concepts page lists, the archive names of the install page against the release
// document both ways (every archive it lists is named, and the count is its count), the paths the source notes name,
// and the host setup script the install page names.

import assert from 'node:assert/strict';
import { existsSync, readdirSync } from 'node:fs';
import { join } from 'node:path';
import { test } from 'node:test';
import { PAGES } from '../tools/pages';
import { read, REPO, SITE } from './support';

type Row = readonly [page: string, shown: string, file: string, says: string];

/** [page, a phrase of the page, a source file, the phrase of that file the page rests on]. */
const ROWS: readonly Row[] = [
  ['index', 'jobscheduler is a generic scheduler for jobs and compute', 'README.md', 'A generic job and compute scheduler'],
  ['index', 'tells the server what the host offers', 'crates/js-agent/src/lib.rs', 'tells a lease server what the machine offers'],
  ['index', 'through Docker, Podman or crun', 'crates/js-agent/src/lib.rs', '`--runtime <docker\\|crun>`'],
  ['index', 'native jobs on a Mac', 'crates/js-agent/src/lib.rs', 'with it a Mac agent runs native jobs'],
  ['index', 'The lease server calls it only under <code>--placement js-sched</code>', 'crates/js-lease-server/src/lib.rs', '`--placement registry\\|js-sched`'],
  ['index', 'to pick among the hosts with room by how tightly a lease fits, and not by load', 'docs/design/placement-by-load.md', 'the host the lease fits most tightly is chosen, not the first by id'],
  ['index', 'to pick among the hosts with room by how tightly a lease fits, and not by load', 'docs/design/placement-by-load.md', 'no measurements and no curves'],
  ['index', 'only under <code>--placement js-sched</code>; the default is <code>registry</code>', 'crates/js-lease-server/src/config.rs', '`registry` (the default)'],
  ['index', 'its client gets the host only when nothing else is on it', 'crates/js-lease-server/src/lib.rs', 'it is listed `pending` until its host\'s agent reports it ready'],
  ['index', 'built, not reached: the lease server sends no request id', 'crates/js-lease-server/src/lib.rs', 'Nothing in this server sends a request id on a lease request yet'],
  ['index', 'not yet reached through the lease server', 'crates/js-lease-server/src/lib.rs', 'so no reservation is made through the HTTP routes'],
  ['index', 'the Debian and macOS packages are built, and not published', '.github/workflows/js-agent-deb.yml', 'no release, no package repository, no registry'],
  ['index', 'the Debian and macOS packages are built, and not published', '.github/workflows/js-agent-macos-pkg.yml', 'It publishes nothing: the packages and their SHA256SUMS are workflow artifacts for seven days'],
  ['concepts', '<code>600s</code> by default, at most <code>3600s</code>', 'crates/js-lease-server/src/lib.rs', '`--default-ttl`, `--max-ttl` | `600s` and `3600s` unless given'],
  ['concepts', 'a renewal never extends the lease past its acquisition plus that time', 'docs/design/lease-limits.md', 'a renewal never extends it past its acquisition plus the max wall time'],
  ['concepts', 'loses the lease at the end of its TTL, never later than its maximum wall time', 'docs/design/lease-limits.md', 'loses its lease at the end of its TTL, never later than its max wall time'],
  ['concepts', 'finds its own by the labels it sent before it asks again', 'crates/js-lease-server/src/lib.rs', 'without first listing (`GET /v1/leases`) and finding its lease by the labels it sent'],
  ['concepts', 'An idempotency key, which would make a repeated acquire return the lease it made, is not built', 'crates/js-lease-server/src/lib.rs', 'An idempotency key, so that a repeated acquire returns the lease it made, is a follow-up and not built here'],
  ['concepts', 'there is one client token and so one trust domain', 'crates/js-lease-server/src/lib.rs', 'The server is single-tenant: there is one token'],
  ['concepts', 'A token for each tenant is not built', 'crates/js-lease-server/src/lib.rs', 'Per-tenant leases'],
  ['concepts', 'A token for each tenant is not built', 'crates/js-lease-server/src/lib.rs', 'are a follow-up and not built here'],
  ['concepts', 'The provider kinds compiled in are <code>static</code> and <code>lambdalabs</code>', 'crates/js-lease-server/src/lib.rs', 'the provider kinds a `[[pool]]` of the configuration file may name, compiled in (`static` and `lambdalabs`)'],
  ['concepts', 'is <code>plan</code>, which launches nothing, unless the file says <code>live</code>', 'crates/js-lease-server/src/lib.rs', '`launch` is `plan` unless the file says `live`: a pool in `plan` launches nothing'],
  ['concepts', 'A pool books that share of each host\'s memory, rounded down', 'crates/js-lease-server/src/lib.rs', '`memory_schedulable_percent` (1 to 100, default 100: the share of `[shape]` `memory_bytes` the pool books, rounded down'],
  ['concepts', 'is not served by this server yet', 'crates/js-lease-server/src/agent_pool.rs', 'a mixed pool (a class that books part of a host) is not served by this server yet'],
  ['concepts', 'The shipped classes are <code>whole-node</code> and <code>gpu-host</code>, which stays as an alias', 'crates/js-lease-server/src/agent_pool.rs', 'a shipped class (`whole-node`, or `gpu-host`, kept as an alias)'],
  ['concepts', 'It books every dimension of the host (CPU, memory, disk, all GPUs, all VM slots)', 'docs/design/whole-node.md', 'It books every dimension of the host (CPU, memory, disk, all GPUs, all `vms`)'],
  ['concepts', 'the post-job checks are clean and the host is not tainted', 'crates/js-agent/src/exclusive.rs', 'no run held, the post-job checks clean once more, the host not tainted'],
  ['concepts', 'A queued request is given exactly one host, by draining it', 'docs/design/whole-node.md', 'A queued whole-node request is given exactly one host by draining it'],
  ['concepts', 'then the fewest leases, then the lowest host id', 'crates/js-whole/src/cost.rs', 'then the fewest leases, then the lowest host id'],
  ['concepts', 'A lease of this class is always answered 202', 'crates/js-lease-server/src/lib.rs', 'a `whole-node` lease is always 202'],
  ['concepts', 'ten minutes unless the pool file sets it', 'crates/js-lease-server/src/lib.rs', 'is dropped if that takes longer than the pool\'s `prepare_timeout`, 600 s unless the pool file sets it'],
  ['concepts', '<code>gpu-host</code> books a whole host without them and refuses them', 'crates/js-lease-server/src/agent_pool.rs', '`gpu-host` books a whole host (`BookAll`) and refuses them'],
  ['concepts', 'A dedicated pool (class <code>whole-node</code>) lets every host be whole', 'crates/js-whole/src/config.rs', 'every host may be whole, none is kept for ordinary work'],
  ['concepts', 'The defaults plan a worker to 1.1 times its CPU (<code>--placement-cpu-slack</code>) and 1.0 times its memory', 'crates/js-sched/src/lib.rs', 'planned to 1.1x of the worker\'s CPU (`--placement-cpu-slack`) and 1.0x of its memory'],
  ['concepts', 'A worker with no usable reading is charged by predicted cost alone', 'crates/js-sched/src/lib.rs', 'a worker with no usable reading is charged by predicted cost alone'],
  ['concepts', 'and not a supported mode', 'crates/js-sched/src/lib.rs', 'it is not a supported mode'],
  ['concepts', '<code>round_up_256MiB(max(1.2 * M + 128 MiB, held, oom_floor))</code>', 'crates/js-sizing/src/lib.rs', '`round_up_256MiB(max(1.2 * M + 128 MiB, held, oom_floor))`'],
  ['concepts', 'and at least 1 GiB', 'crates/js-sizing/src/lib.rs', 'at least 1 GiB'],
  ['concepts', '<code>round_up_250m(P90(cpu_usage / wall))</code>, clamped to 250 m and the node\'s cores', 'crates/js-sizing/src/lib.rs', '`round_up_250m(P90(cpu_usage / wall))`, clamped to `[250 m, node cpus]`'],
  ['concepts', 'A booking at or above the memory of the largest matching live node owns the host', 'crates/js-sizing/src/lib.rs', 'A booking at or above the memory of the largest matching live node ([`Bounds::host_memory`]) owns the host'],
  ['concepts', '<code>js-lease-server</code> calls the scheduler only under <code>--placement js-sched</code>', 'crates/js-lease-server/src/lib.rs', '`--placement registry\\|js-sched` | who picks the host of a lease on a host that registered itself, `registry` unless given'],
  ['concepts', 'the default is <code>registry</code>, which books the first free host by id', 'crates/js-lease-server/src/lib.rs', '`registry` unless given. `registry` books the first free host by id'],
  ['concepts', 'builds a scheduler over the hosts that have room for the lease and books the one it names', 'crates/js-lease-server/src/lib.rs', 'have room beside the leases already there, and are bookable) and books the one it names'],
  ['concepts', 'for a class that books part of a host, the host the lease fits most tightly', 'crates/js-lease-server/src/lib.rs', 'for a class that books part of a host, the host the lease fits most tightly'],
  ['concepts', 'a lease that books a whole host is not scored and takes the first free host by id', 'crates/js-lease-server/src/lib.rs', 'a lease that books a whole host is not scored and takes the first free host by id'],
  ['concepts', 'The leases on those hosts are charged flat, with no measurements and no curves', 'docs/design/placement-by-load.md', 'recovered as flat-charged leases, no measurements and no curves'],
  ['concepts', 'the placement step would let leases of such a class share a host', 'crates/js-lease-server/src/lib.rs', 'a class that books part of a host is then leasable, and its leases share a host that has room'],
  ['concepts', 'but no pool file can name one yet', 'crates/js-lease-server/src/file.rs', 'No class a file can name books part of a host today'],
  ['concepts', 'The registry reserves a host for a request that carries a request id', 'crates/js-registry/src/registry/start.rs', 'otherwise it waits, if it has an id'],
  ['concepts', 'nothing in the lease server sends one yet, so no reservation is made through its routes', 'crates/js-lease-server/src/lib.rs', 'Nothing in this server sends a request id on a lease request yet, so no reservation is made through the HTTP routes'],
  ['concepts', 'the drain cost: the host\'s tail, the latest end of its leases', 'crates/js-whole/src/cost.rs', 'smallest first: the host\'s tail (see [`tail`]) minus `now`, then the fewest leases, then the lowest host id'],
  ['concepts', 'each ending at its estimate cut to its max-wall bound, or at the bound when it has no estimate', 'crates/js-whole/src/cost.rs', 'The end of one lease: its estimate cut to its max-wall bound when it has one, else the bound'],
  ['concepts', 'three launch or boot failures in a row trip it back to <code>plan</code>', 'crates/js-lease-server/src/lib.rs', 'failures in a row (3, from 1 to 100) the brake trips the pool to `plan`'],
  ['concepts', 'only an operator\'s reset clears the trip', 'crates/js-lease-server/src/lib.rs', 'Time does not end the trip'],
  ['concepts', 'only an operator\'s reset clears the trip', 'crates/js-lease-server/src/lib.rs', 'clears the count, the trip and the cooldown'],
  ['concepts', 'A host that goes silent keeps its lease and reads <code>Unknown</code> until it is heard again', 'crates/js-lease-server/src/lib.rs', 'A host that goes silent keeps its lease and reads `Unknown` until it is heard again'],
  ['concepts', 'The lease table frees a lease only when the provider is observed without it', 'docs/design/lease-limits.md', 'freeing a lease only when the provider is observed without it'],
  ['concepts', '<code>JS_LEASE_ID</code> and <code>JS_LEASE_HOST</code>', 'crates/js-cli/src/lib.rs', '`JS_LEASE_ID` and `JS_LEASE_HOST`'],
  ['install', 'There is no lease server for macOS', 'docs/release.md', 'there is no macOS `js-lease-server`'],
  ['install', 'not codesigned or notarized', 'docs/release.md', 'are not codesigned or notarized'],
  ['install', 'That the macOS archives come out byte-identical when a partly published release is re-run', 'docs/release.md', 'Not yet proven: that the Mac archives come out byte-identical'],
  ['install', 'the SHA-256 of the ten archives', 'docs/release.md', 'SHA-256 of the ten archives'],
  ['install', 'the MDM that enrolls Macs as compute hosts, set up on one Linux host', 'crates/js-mdm/src/lib.rs', 'js-mdm is jobscheduler\'s MDM for enrolling Macs as compute hosts'],
  ['install', 'the bundle tool <code>-p \'js-bundle-*-x86_64-unknown-linux-musl.tar.gz\'</code>', 'docs/release.md', 'for the bundle tool `-p \'js-bundle-*-x86_64-unknown-linux-musl.tar.gz\'`'],
  ['install', 'no published installer', '.github/workflows/js-agent-macos-pkg.yml', 'It publishes nothing'],
  ['install', 'The macOS installer package of the agent, in <code>packaging/js-agent-macos</code>', 'packaging/js-agent-macos/README.md', 'A signed installer package (`js-agent-<version>-aarch64-apple-darwin.pkg`)'],
  ['install', 'which publishes nothing either', '.github/workflows/js-agent-macos-pkg.yml', 'It publishes nothing: the packages and their SHA256SUMS are workflow artifacts for seven days'],
  ['install', 'Every push to <code>main</code> that changes code that ships is released as the next version', 'docs/release.md', 'every push to `main` that changes code that ships is released as the next version'],
  ['install', 'That workflow publishes no release, no package repository and no registry', '.github/workflows/js-agent-deb.yml', 'no release, no package repository, no registry'],
  ['install', 'workflow artifact kept for seven days', '.github/workflows/js-agent-deb.yml', 'workflow artifact for seven days'],
  ['install', '<code>--boot initialise</code>, which may create the state file', 'crates/js-lease-server/src/lib.rs', 'required: `initialise` may create the file, `resume` needs it'],
  ['install', 'The default listen address is <code>127.0.0.1:7300</code>; off the loopback the server needs both', 'crates/js-lease-server/src/lib.rs', 'default `127.0.0.1:7300`; off the loopback it needs both TLS flags'],
  ['install', 'at least 32 bytes', 'crates/js-lease-server/src/lib.rs', 'holding one token of printable ASCII of at least 32 bytes'],
  ['install', 'a regular file owned by the server\'s user, mode 0600 or 0400', 'crates/js-lease-server/src/lib.rs', 'a regular file owned by the server\'s user, mode 0600 or 0400'],
  ['install', 'A start the server refuses prints the flag at fault and exits with status 2', 'crates/js-lease-server/src/lib.rs', 'A start that is refused prints the flag at fault and exits with status 2'],
  ['install', 'one per line, each as strong as the client token, in an owner-only file', 'crates/js-lease-server/src/lib.rs', 'one per line, each as strong as `--token-file`\'s, in an owner-only file'],
  ['install', 'write the pool\'s join token to <code>/etc/js-agent/token</code>, mode 0600', 'packaging/js-agent/js-agent.service', 'write the pool\'s join token to /etc/js-agent/token, mode 0600'],
  ['install', 'mount a disk of its own at <code>/var/lib/js-agent/scratch</code>', 'packaging/js-agent/js-agent.service', 'mount a disk of its own at /var/lib/js-agent/scratch'],
  ['install', 'the shipped <code>--server</code> is a placeholder the agent refuses', 'packaging/js-agent/js-agent.service', 'the shipped --server is a placeholder the agent refuses'],
  ['install', 'A missing mount fails silently.', 'packaging/js-agent/js-agent.service', 'With the mount of step 3 missing this exits 0 and starts nothing, silently'],
  ['install', 'refuses to start under <code>ProtectSystem=</code>, <code>PrivateMounts=</code> or <code>PrivatePIDs=</code>', 'packaging/js-agent/js-agent.service', 'it refuses to start under ProtectSystem=, PrivateMounts= or PrivatePIDs='],
  ['install', 'the agent refuses a state root on the same disk as the scratch root', 'packaging/js-agent/js-agent.service', 'The agent refuses a state root on the same disk as the scratch root'],
  ['install', 'It exits 2 for a wrong flag or file, a job engine it cannot use, or a server that refuses the host for good', 'packaging/js-agent/js-agent.service', '2 for a wrong flag or file, a job engine it cannot use, or a server that refuses the host for good'],
  ['install', 'the unit restarts it every 30 seconds', 'packaging/js-agent/js-agent.service', 'RestartSec=30'],
  ['install', '<code>/usr/bin/js-agent</code>', 'packaging/js-agent/js-agent.service', 'ExecStart=/usr/bin/js-agent'],
  ['install', '<code>http://</code> is accepted only for <code>localhost</code>, <code>127.0.0.0/8</code> and <code>::1</code>', 'crates/js-agent/src/lib.rs', '`http://` only for `localhost`, `127.0.0.0/8` and `::1`'],
  ['install', 'sets a host up for it and installs it, and the unit is not enabled or started', 'packaging/js-agent/crun_host_setup.sh', 'the unit is NOT enabled or started'],
  ['install', 'it is refused at start', 'crates/js-agent/src/lib.rs', 'not available yet: refused at start'],
  ['install', '<code>--wait-pending</code>, 60 seconds by default', 'crates/js-cli/src/lib.rs', 'for at most `--wait-pending` (default 60s)'],
  ['install', 'the client looks at the list every two seconds', 'crates/js-cli/src/lib.rs', 'is looked at in the list every two seconds'],
  ['install', '<code>acquire</code> prints <code>lease &lt;id&gt; host &lt;host&gt; expires &lt;ms&gt;</code>', 'crates/js-cli/src/lib.rs', '`acquire` prints `lease <id> host <host> expires <ms>`'],
  ['kbf', 'kbf is the first user of jobscheduler', 'docs/design.md', 'First user: kbf'],
  ['kbf', 'it names every <code>js-*</code> crate once, in its root <code>[workspace.dependencies]</code>, all at the same revision of this repository', 'docs/layering.md', 'names every `js-*` crate once, in its root `[workspace.dependencies]`, all at the same revision of this repository'],
  ['kbf', 'a bump is a deliberate pull request there', 'docs/layering.md', 'a bump is a deliberate pull request there'],
  ['kbf', 'kbf keeps its name as a shim that re-exports the crate here', 'docs/layering.md', 'keeps the crate\'s name as a shim (`pub use js_x::*`)'],
  ['kbf', 'This repository never depends on kbf', 'docs/layering.md', 'This repository never depends on the other one'],
  ['kbf', '<code>deny.toml</code> bans kbf\'s crates by name', 'docs/layering.md', '`deny.toml` also bans kbf\'s crates by name'],
  ['kbf', 'several frontends may share one store', 'crates/js-server-core/src/lib.rs', 'several frontends may share one store'],
  ['kbf', 'a record is committed before anything that depends on it happens', 'crates/js-server-core/src/lib.rs', 'a record is committed, and fed back as committed, before anything that depends on it happens'],
  ['kbf', 'generic half of the kbf daemon', 'docs/design.md', '`js-agent` is the generic half of the kbf daemon'],
  ['kbf', 'only the <code>legacy</code> one is built into a running server', 'docs/design/kbf-today.md', 'only `legacy` is built into a running server'],
  ['kbf', 'nothing in kbf runs <code>js-agent</code>', 'docs/design/kbf-today.md', 'Nothing in kbf runs `js-agent`'],
  ['kbf', 'no lease client of jobscheduler was in use', 'docs/design/kbf-today.md', 'no lease client of js is on the farm'],
  ['kbf', 'marked "design only"', 'docs/design/kbf-on-js-agent.md', 'Status: design only'],
  ['kbf', 'nothing in it is built or approved to run', 'docs/design/kbf-on-js-agent.md', 'Nothing here is built or approved to run'],
  ['kbf', '<code>--executor</code>', 'docs/design/kbf-on-js-agent.md', '`--executor` (`js-agent` or `kbf-daemon`, default `kbf-daemon`) as the rollback switch'],
  ['kbf', '<code>/v1/leases/{id}/runs</code>', 'docs/design/kbf-on-js-agent.md', '`POST/GET/DELETE /v1/leases/{id}/runs`'],
  ['kbf', 'runs are in memory only: a restart forgets them', 'crates/js-lease-server/src/run_api.rs', 'Runs are in memory only. A restart forgets them'],
  ['kbf', 'jobscheduler gets only generic fields', 'docs/design/kbf-on-js-agent.md', 'js gets only generic fields'],
];

/** What a source says: comment markers (`//!`, `///`, `//`, `#`) dropped from the start of each line, whitespace collapsed. */
export function flat(text: string): string {
  return text
    .split('\n')
    .map((l) => l.replace(/^\s*(?:\/\/[!/]?|#)\s?/, ''))
    .join(' ')
    .replace(/\s+/g, ' ');
}

const page = (name: string): string => read(SITE, 'content', `${name}.html`);
const source = (file: string): string => read(REPO, file);

/** The rows that no longer hold: the page lost its phrase, or the source lost its own. Each problem names the page to change. */
export function unheld(rows: readonly Row[], src: (file: string) => string, html: (name: string) => string): string[] {
  const out: string[] = [];
  for (const [name, shown, file, says] of rows) {
    if (!html(name).includes(shown)) out.push(`content/${name}.html no longer says: ${shown}`);
    if (!flat(src(file)).includes(says)) out.push(`${file} no longer says "${says}": revisit content/${name}.html ("${shown}")`);
  }
  return out;
}

test('every statement of a page is still what its source says', () => {
  assert.deepEqual(unheld(ROWS, source, page), []);
});

test('every page but the 404 has rows, so the table cannot go empty unnoticed', () => {
  for (const p of PAGES.filter((x) => x.source !== '404')) {
    assert.ok(ROWS.filter((r) => r[0] === p.source).length >= 3, `${p.source} has fewer than 3 rows`);
  }
});

test('the check can fail: a changed source phrase and a reworded page are each reported with the page to change', () => {
  const real = (f: string): string => source(f);
  const changedSource = (f: string): string => (f === 'docs/release.md' ? real(f).replace('are not codesigned or notarized', 'are signed') : real(f));
  assert.match(unheld(ROWS, changedSource, page).join('\n'), /docs\/release\.md no longer says "are not codesigned or notarized": revisit content\/install\.html/);
  const rewordedPage = (n: string): string => (n === 'kbf' ? page(n).replace('marked "design only"', 'marked "in progress"') : page(n));
  assert.match(unheld(ROWS, real, rewordedPage).join('\n'), /content\/kbf\.html no longer says: marked "design only"/);
});

test('flat() drops comment markers, joins wrapped lines and collapses spaces', () => {
  assert.equal(flat('//! a\n//!   b\n/// c\n# d\ne  f'), 'a b c d e f');
});

test('numbers the pages give are the constants of the code', () => {
  // The cold CPU of a size estimate, 2000 m, and the page's "2000 m for a key with no CPU sample".
  assert.match(source('crates/js-sizing/src/summary.rs'), /pub const COLD_CPU_MILLIS: u32 = 2000;/);
  assert.match(page('concepts'), /2000 m for a key with no CPU sample/);
  // The prepare timeout, ten minutes, and the page's "ten minutes".
  assert.match(source('crates/js-whole/src/config.rs'), /pub const DEFAULT_PREPARE_TIMEOUT_MS: u64 = 600_000;/);
  // The CPU slack, 1.1 (100 thousandths), and the memory slack, none.
  assert.match(source('crates/js-sched/src/admission/config.rs'), /s_cpu_permille: 100,/);
  assert.match(source('crates/js-sched/src/admission/config.rs'), /flat_memory_slack_permille: Some\(0\),/);
  // The token's least length, 32 bytes, and the page's "at least 32 bytes".
  assert.match(source('crates/js-lease-server/src/config.rs'), /pub const MIN_TOKEN_BYTES: usize = 32;/);
  // The default of --listen is in the table of the flags; the page prints it.
  assert.match(page('install'), /127\.0\.0\.1:7300/);
});

test('the project is pre-1.0, as the home page says', () => {
  assert.match(page('index'), /Early development/);
  assert.match(source('Cargo.toml'), /^version = "0\./m);
});

test('the whole-node keys the concepts page lists are the keys the pool file reads', () => {
  const listed = [...page('concepts').matchAll(/<code>([a-z_]+)<\/code>/g)].map((m) => m[1]);
  const keys = ['max_whole_node', 'max_draining', 'max_whole_node_per_consumer', 'keep_for_ordinary', 'allow_last_capable', 'drain_after', 'drain_deadline', 'reserve_backoff', 'max_reserve_attempts', 'prepare_timeout', 'max_replace', 'regain_grace'];
  const pool = source('crates/js-lease-server/src/agent_pool.rs');
  for (const key of keys) {
    assert.ok(listed.includes(key), `the concepts page no longer lists ${key}`);
    assert.ok(pool.includes(`    ${key}: Option<`), `agent_pool.rs no longer reads ${key}: update content/concepts.html`);
  }
});

test('the pool file the install page shows uses keys the pool file reads', () => {
  const pool = source('crates/js-lease-server/src/agent_pool.rs');
  for (const key of ['name', 'class', 'max_machines', 'shape', 'cpu_millis', 'memory_bytes']) {
    assert.ok(pool.includes(`    ${key}: `), `agent_pool.rs no longer reads ${key}: update content/install.html`);
  }
  const shown = page('install');
  for (const line of ['name = "hosts"', 'class = "whole-node"', 'max_machines = 4', '[shape]', 'cpu_millis = 8000', 'memory_bytes = 17179869184']) {
    assert.ok(shown.includes(line), `the install page lost ${line}`);
  }
});

test('the archives the install page names are in the release document', () => {
  const release = source('docs/release.md');
  const named = [...page('install').matchAll(/<code>(js[a-z-]*-X\.Y\.Z-[a-z0-9_-]+\.tar\.gz)<\/code>/g)].map((m) => m[1]);
  assert.ok(named.length >= 6, `${named.length} archive names`);
  for (const name of named) assert.ok(release.includes(name), `docs/release.md does not list ${name}`);
});

/** The archives of the "Assets" table of docs/release.md, in its order. */
export function releasedArchives(release: string): string[] {
  return [...release.matchAll(/^\| `(js[a-z-]*-X\.Y\.Z-[a-z0-9_-]+\.tar\.gz)` \|/gm)].map((m) => m[1]);
}

const COUNT_WORDS = ['zero', 'one', 'two', 'three', 'four', 'five', 'six', 'seven', 'eight', 'nine', 'ten', 'eleven', 'twelve', 'thirteen', 'fourteen', 'fifteen'];

/** What the install page's archive table misses or miscounts against docs/release.md: an archive released and not named, or a count that is not the number released. */
export function archiveGaps(release: string, install: string): string[] {
  const released = releasedArchives(release);
  const out = released.filter((name) => !install.includes(`<code>${name}</code>`)).map((name) => `the install page does not name ${name}`);
  const said = /the SHA-256 of the ([a-z]+) archives/.exec(install)?.[1];
  if (said !== COUNT_WORDS[released.length]) out.push(`the install page says "${said}" archives, and docs/release.md lists ${released.length}`);
  return out;
}

test('the install page names every archive the release document lists, and counts them right', () => {
  const release = source('docs/release.md');
  assert.ok(releasedArchives(release).length >= 6, `${releasedArchives(release).length} archives in the release document`);
  assert.deepEqual(archiveGaps(release, page('install')), []);
});

test('an archive left off the install page, or a wrong count, is reported (the check can fail)', () => {
  const release = source('docs/release.md');
  const name = 'js-mdm-X.Y.Z-x86_64-unknown-linux-musl.tar.gz';
  const dropped = page('install').replace(new RegExp(`<tr><td><code>${name.replace(/\./g, '\\.')}</code>.*?</tr>`), '');
  assert.match(archiveGaps(release, dropped).join('\n'), new RegExp(`does not name ${name.replace(/\./g, '\\.')}`));
  const added = release.replace('| `SHA256SUMS` |', '| `js-new-X.Y.Z-x86_64-unknown-linux-musl.tar.gz` | a new tool |\n| `SHA256SUMS` |');
  assert.match(archiveGaps(added, page('install')).join('\n'), /does not name js-new-X\.Y\.Z/);
  assert.match(archiveGaps(added, page('install')).join('\n'), /docs\/release\.md lists 11/);
});

test('the verification commands of the install page are those of the release document', () => {
  const release = source('docs/release.md');
  const shown = page('install');
  for (const part of [
    'gh release download vX.Y.Z -R komira-ai/jobscheduler',
    'sha256sum -c --ignore-missing SHA256SUMS',
    'gh attestation verify js-lease-server-X.Y.Z-x86_64-unknown-linux-musl.tar.gz -R komira-ai/jobscheduler',
    '--signer-workflow komira-ai/jobscheduler/.github/workflows/release.yml',
    '--source-ref refs/heads/main',
    '--source-digest "$(git rev-parse vX.Y.Z^{commit})"',
  ]) {
    assert.ok(release.includes(part), `docs/release.md lost: ${part}`);
    assert.ok(shown.includes(part), `the install page lost: ${part}`);
  }
});

/** Every file of the repository except what a build leaves behind, as '/'-separated paths from the root. */
function repoFiles(dir = REPO, prefix = ''): string[] {
  const out: string[] = [];
  for (const e of readdirSync(dir, { withFileTypes: true })) {
    if (['.git', 'node_modules', 'target', 'build', 'dist'].includes(e.name)) continue;
    if (e.isDirectory()) out.push(...repoFiles(join(dir, e.name), `${prefix}${e.name}/`));
    else out.push(`${prefix}${e.name}`);
  }
  return out;
}

test('the host setup script the install page names is in the repository', () => {
  assert.match(page('install'), /<code>packaging\/js-agent\/crun_host_setup.sh<\/code> sets a host up/);
  assert.deepEqual(repoFiles().filter((f) => f.endsWith('crun_host_setup.sh')), ['packaging/js-agent/crun_host_setup.sh']);
});

test('every page but the 404 has a source note, and every file a source note names exists', () => {
  const checked: string[] = [];
  for (const p of PAGES.filter((x) => x.source !== '404')) {
    const notes = [...page(p.source).matchAll(/<p class="source-note">([\s\S]*?)<\/p>/g)].map((m) => m[1]);
    assert.ok(notes.length >= 1, `${p.source} has no source note`);
    for (const note of notes) {
      for (const m of note.matchAll(/<code>([^<]+)<\/code>/g)) {
        const token = m[1];
        if (!/\/|\.(?:md|rs|toml|ya?ml|service|conf)$/.test(token)) continue;
        assert.ok(existsSync(join(REPO, token)), `${p.source}: the source note names ${token}, which is not in the repository`);
        checked.push(token);
      }
    }
  }
  assert.ok(checked.length >= 20, `${checked.length} files checked`);
});

test('a source note naming a missing file would be reported (the check can fail)', () => {
  assert.equal(existsSync(join(REPO, 'crates/js-lease-server/src/nope.rs')), false);
});
