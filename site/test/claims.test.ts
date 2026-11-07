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
import { ciPaths, ciWorkflow, pathMatches, siteRuns, siteSteps } from './ci-site';
import { read, REPO, SITE } from './support';

type Row = readonly [page: string, shown: string, file: string, says: string];

/** [page, a phrase of the page, a source file, the phrase of that file the page rests on]. */
const ROWS: readonly Row[] = [
  ['index', 'jobscheduler is a generic scheduler for jobs and compute', 'README.md', 'A generic job and compute scheduler'],
  ['index', 'through Docker, Podman or crun', 'crates/js-agent/src/lib.rs', '`--runtime <docker\\|crun\\|none>`'],
  ['index', '(through Docker, Podman or crun) and native jobs on a Mac', 'crates/js-agent/src/lib.rs', 'with it a Mac agent runs native jobs'],
  ['index', 'only under <code>--placement js-sched</code>; the default is <code>registry</code></span></td>', 'crates/js-lease-server/src/config.rs', '`registry` (the default)'],
  ['index', 'the Debian and macOS packages are built, and not published</span></td>', '.github/workflows/js-agent-deb.yml', 'no release, no package repository, no registry'],
  ['index', 'the Debian and macOS packages are built, and not published</span></td>', '.github/workflows/js-agent-macos-pkg.yml', 'It publishes nothing: the packages and their SHA256SUMS are workflow artifacts for seven days'],
  ['index', 'jobscheduler is a scheduler for jobs and compute', 'README.md', 'A generic job and compute scheduler'],
  ['index', 'Open source, Apache 2.0', 'LICENSE', 'Apache License Version 2.0'],
  ['index', 'Open source, Apache 2.0', 'Cargo.toml', 'license = "Apache-2.0"'],
  ['index', 'open source, Apache 2.0', 'LICENSE', 'Apache License Version 2.0'],
  ['index', 'open source, Apache 2.0', 'Cargo.toml', 'license = "Apache-2.0"'],
  ['index', '<strong>Install</strong> <span class="tag tag-ok">works today</span> <span class="tag tag-muted">Linux x86_64 and arm64, and Apple silicon, from a release</span>', 'docs/release.md', '`x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl` (`js-lease-server`, `js`, `js-agent`, `js-bundle`, `js-mdm`, for each of the two Linux architectures)'],
  ['index', '<strong>Install</strong> <span class="tag tag-ok">works today</span> <span class="tag tag-muted">Linux x86_64 and arm64, and Apple silicon, from a release</span>', 'docs/release.md', 'the `js` binary for Apple silicon Macs'],
  ['index', '<strong>Install</strong> <span class="tag tag-ok">works today</span> <span class="tag tag-muted">Linux x86_64 and arm64, and Apple silicon, from a release</span>', 'docs/release.md', '`publish` creates the GitHub release for `vX.Y.Z`'],
  ['index', 'Every push to <code>main</code> that changes code that ships is released, with a checksum file and a build attestation', 'docs/release.md', 'every push to `main` that changes code that ships is released as the next version'],
  ['index', 'Every push to <code>main</code> that changes code that ships is released, with a checksum file and a build attestation', 'docs/release.md', '`SHA256SUMS` over the archives, the packages and the index'],
  ['index', 'Every push to <code>main</code> that changes code that ships is released, with a checksum file and a build attestation', 'docs/release.md', '`attest` attests every archive, package and the index with build provenance'],
  ['index', '; TLS</td><td><span class="tag tag-ok">works today</span></td>', 'crates/js-lease-server/src/lib.rs', '`--tls-cert`, `--tls-key` | PEM files; both or neither'],
  ['index', '<td>Agents that register with a join token and run', 'crates/js-agent/src/lib.rs', 'the pool\'s join token'],
  ['index', 'container jobs on Linux or native jobs on a Mac</td><td><span class="tag tag-ok">works today</span></td>', 'crates/js-agent/src/lib.rs', 'the `docker` or `podman` program that runs container jobs'],
  ['index', 'container jobs on Linux or native jobs on a Mac</td><td><span class="tag tag-ok">works today</span></td>', 'crates/js-agent/src/lib.rs', 'with it a Mac agent runs native jobs'],
  ['index', '<td>Pools of class <code>whole-node</code> or <code>gpu-host</code></td><td><span class="tag tag-ok">works today</span></td>', 'crates/js-pool-static/src/lib.rs', 'The pool serves `gpu-host`'],
  ['index', '<td>Pools of class <code>whole-node</code> or <code>gpu-host</code></td><td><span class="tag tag-ok">works today</span></td>', 'crates/js-pool-static/src/lib.rs', 'such as `whole-node`'],
  ['index', 'launches nothing until the pool file says <code>live</code></span></td>', 'crates/js-lease-server/src/lib.rs', '`launch` is `plan` unless the file says `live`: a pool in `plan` launches nothing'],
  ['index', 'Placement by load, in the <code>js-sched</code> crate', 'crates/js-sched/src/lib.rs', 'admission by measured and predicted load'],
  ['index', 'A pool that books part of a host', 'crates/js-lease-server/src/agent_pool.rs', 'a mixed pool (a class that books part of a host) is not served by this server yet'],
  ['index', 'not built: refused when the pool file loads', 'crates/js-lease-server/src/file/section.rs', 'Refuses a class that books part of a host: this server serves whole hosts only.'],
  ['index', 'A lease server for macOS', 'docs/release.md', 'there is no macOS `js-lease-server`'],
  ['index', 'none: Linux x86_64 and arm64 only', 'docs/release.md', '`x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl` (`js-lease-server`, `js`, `js-agent`, `js-bundle`, `js-mdm`, for each of the two Linux architectures)'],
  ['index', '<td>A published package or image of the agent</td><td><span class="tag tag-planned">none:', '.github/workflows/js-agent-deb.yml', 'no release, no package repository, no registry'],
  ['index', 'kbf running its actions on the agent', 'docs/design/kbf-on-js-agent.md', 'Status: design only'],
  ['index', 'design only', 'docs/design/kbf-on-js-agent.md', 'Status: design only'],
  ['index', 'An agent on every host', 'crates/js-agent/src/lib.rs', 'runs on a machine, tells a lease server what the machine offers'],
  ['index', '<code>js-agent</code> offers the host\'s CPU, memory and GPUs', 'crates/js-agent/src/lib.rs', 'tells a lease server what the machine offers'],
  ['index', '<code>js-agent</code> offers the host\'s CPU, memory and GPUs', 'crates/js-agent/src/lib.rs', 'The CPU count (`getconf _NPROCESSORS_ONLN`), memory (`/proc/meminfo`, or `sysctl -n hw.memsize` on a Mac)'],
  ['index', 'heartbeats, and runs container jobs on Linux', 'crates/js-agent/src/lib.rs', 'keeps telling it that the machine is alive'],
  ['index', 'heartbeats, and runs container jobs on Linux', 'crates/js-agent/src/lib.rs', 'the absolute path of the `docker` or `podman` program that runs container jobs'],
  ['index', 'A whole node, quiet, when asked', 'crates/js-types/src/class.rs', 'A whole host: `BookAll` plus a reservation, a quiet gate and caps.'],
  ['index', 'Placement by measured and predicted load', 'crates/js-sched/src/lib.rs', 'admission by measured and predicted load'],
  ['index', '<span class="pill">Early development</span>', 'Cargo.toml', 'version = "0.1.0"'],
  ['index', '<td>An elastic pool on Lambda Labs</td><td><span class="tag tag-ok">built</span> <span class="tag tag-muted">', 'crates/js-provider-lambdalabs/src/lib.rs', 'launching and terminating GPU instances under a cost guard'],
  ['index', '<td>An elastic pool on Lambda Labs</td><td><span class="tag tag-ok">built</span> <span class="tag tag-muted">', 'crates/js-lease-server/src/lib.rs', 'A pool whose provider launches machines (an elastic pool)'],
  ['index', 'a bearer token', 'crates/js-lease-server/src/api.rs', 'Refuses a request that lacks `Authorization: Bearer <token>`'],
  ['index', 'heartbeats, and runs container jobs on Linux', 'crates/js-agent/src/lib.rs', 'Container jobs are Linux-only (the checks read `/proc`)'],
  ['index', '<code>js-agent</code> offers the host\'s CPU, memory and GPUs', 'crates/js-agent/src/lib.rs', 'the GPUs: `nvidia-smi` on Linux'],
  ['concepts', '<code>/healthz</code> needs none, and neither do <code>/v1/pools</code> and <code>/status</code>', 'crates/js-lease-server/src/api.rs', 'let open = Router::new() .route("/healthz", get(healthz)) .route("/v1/pools", get(pools)) .route("/status", get(status_page))'],
  ['concepts', 'which are served only when the server listens on a private address, and only to a private peer', 'crates/js-lease-server/src/lib.rs', 'no token, on `server.listen` only, and not served (404) when that is not a private address, nor to a peer that is not a private address'],
  ['concepts', 'which are served only when the server listens on a private address, and only to a private peer', 'crates/js-lease-server/src/api/status_gate_tests.rs', 'a status route served over the router answers a private peer with 200 and a public peer'],
  ['concepts', 'An agent\'s routes take a join token instead', 'crates/js-lease-server/src/lib.rs', 'machine\'s agent calls two routes with a join token'],
  ['concepts', 'The provider kinds compiled in are <code>static</code> and <code>lambdalabs</code>', 'crates/js-lease-server/src/lib.rs', 'the provider kinds a `[[pool]]` of the configuration file may name, compiled in (`static` and `lambdalabs`)'],
  ['concepts', 'is <code>plan</code>, which launches nothing, unless the file says <code>live</code>;', 'crates/js-lease-server/src/lib.rs', '`launch` is `plan` unless the file says `live`: a pool in `plan` launches nothing'],
  ['concepts', 'A pool books that share of each host\'s memory, rounded down', 'crates/js-lease-server/src/lib.rs', '`memory_schedulable_percent` (1 to 100, default 100: the share of `[shape]` `memory_bytes` the pool books, rounded down'],
  ['concepts', '<span class="tag tag-planned">not built</span> A class that books only part of a host (a mixed pool) cannot be served: the server refuses the pool file with "a mixed pool (a class that books part of a host) is not served by this server yet".', 'crates/js-lease-server/src/agent_pool.rs', 'a mixed pool (a class that books part of a host) is not served by this server yet'],
  ['concepts', 'The shipped classes are <code>whole-node</code> and <code>gpu-host</code>, which stays as an alias; any other name is a class that books a whole host too.', 'crates/js-lease-server/src/agent_pool.rs', 'a shipped class (`whole-node`, or `gpu-host`, kept as an alias)'],
  ['concepts', '<strong>Exclusive.</strong> It books every dimension of the host (CPU, memory, disk, all GPUs, all VM slots),', 'docs/design/whole-node.md', 'It books every dimension of the host (CPU, memory, disk, all GPUs, all `vms`)'],
  ['concepts', '<strong>Quiet.</strong> Before the client can use it the host has no run, no VM and no leftover of an earlier job, the post-job checks are clean and the host is not tainted.', 'crates/js-agent/src/exclusive.rs', 'no run held, the post-job checks clean once more, the host not tainted'],
  ['concepts', '<strong>Reserved.</strong> A queued request is given exactly one host, by draining it,', 'docs/design/whole-node.md', 'A queued whole-node request is given exactly one host by draining it'],
  ['concepts', 'then the fewest leases, then the lowest host id.', 'crates/js-whole/src/cost.rs', 'then the fewest leases, then the lowest host id'],
  ['concepts', '<code>gpu-host</code> books a whole host without them and refuses them', 'crates/js-lease-server/src/agent_pool.rs', '`gpu-host` books a whole host (`BookAll`) and refuses them'],
  ['concepts', 'A dedicated pool (class <code>whole-node</code>) lets every host be whole', 'crates/js-whole/src/config.rs', 'every host may be whole, none is kept for ordinary work'],
  ['concepts', 'The defaults plan a worker to 1.1 times its CPU (<code>--placement-cpu-slack</code>) and 1.0 times its memory', 'crates/js-sched/src/lib.rs', 'planned to 1.1x of the worker\'s CPU (`--placement-cpu-slack`) and 1.0x of its memory'],
  ['concepts', 'A worker with no usable reading is charged by predicted cost alone', 'crates/js-sched/src/lib.rs', 'a worker with no usable reading is charged by predicted cost alone'],
  ['concepts', 'and not a supported mode', 'crates/js-sched/src/lib.rs', 'it is not a supported mode'],
  ['concepts', '<td>Memory</td><td><code>round_up_256MiB(max(1.2 * M + 128 MiB, held, oom_floor))</code>,', 'crates/js-sizing/src/lib.rs', '`round_up_256MiB(max(1.2 * M + 128 MiB, held, oom_floor))`'],
  ['concepts', 'and at least 1 GiB</td>', 'crates/js-sizing/src/lib.rs', 'at least 1 GiB'],
  ['concepts', '<td>CPU</td><td><code>round_up_250m(P90(cpu_usage / wall))</code>, clamped to 250 m and the node\'s cores', 'crates/js-sizing/src/lib.rs', '`round_up_250m(P90(cpu_usage / wall))`, clamped to `[250 m, node cpus]`'],
  ['concepts', 'A booking at or above the memory of the largest matching live node owns the host: the estimate never reaches a whole host, and the caller books cold instead.', 'crates/js-sizing/src/lib.rs', 'A booking at or above the memory of the largest matching live node ([`Bounds::host_memory`]) owns the host'],
  ['concepts', 'Which host a reservation drains is decided by the drain cost: the host\'s tail, the latest end of its leases,', 'crates/js-whole/src/cost.rs', 'smallest first: the host\'s tail (see [`tail`]) minus `now`, then the fewest leases, then the lowest host id'],
  ['concepts', 'each ending at its estimate cut to its max-wall bound, or at the bound when it has no estimate;', 'crates/js-whole/src/cost.rs', 'The end of one lease: its estimate cut to its max-wall bound when it has one, else the bound'],
  ['concepts', 'three launch or boot failures in a row trip it back to <code>plan</code>, and', 'crates/js-lease-server/src/lib.rs', 'failures in a row (3, from 1 to 100) the brake trips the pool to `plan`'],
  ['concepts', 'only an operator\'s reset clears the trip.', 'crates/js-lease-server/src/lib.rs', 'Time does not end the trip'],
  ['concepts', 'only an operator\'s reset clears the trip.', 'crates/js-lease-server/src/lib.rs', 'clears the count, the trip and the cooldown'],
  ['concepts', 'Capacity comes in pools', 'crates/js-lease-server/src/lib.rs', '`--config <path>` reads a `[server]` table and `[[pool]]` tables'],
  ['concepts', 'A pool has a name, a class, a shape and <code>max_machines</code>', 'crates/js-lease-server/src/lib.rs', 'the pool agents join, as TOML: `name`, `class`, `max_machines`, `[shape]`'],
  ['concepts', 'The shape is what the pool books for every host', 'crates/js-lease-server/src/agent_pool.rs', 'The `[shape]` table: what the pool promises and books for every host.'],
  ['concepts', '<code>cpu_millis</code> and <code>memory_bytes</code>, and optionally <code>gpus</code>, <code>gpu_class</code> and <code>disk_bytes</code>', 'crates/js-lease-server/src/lib.rs', '`[shape]` (`cpu_millis`, `memory_bytes`, `gpus?`, `gpu_class?`, `disk_bytes?`:'],
  ['concepts', '<strong>A static pool</strong>: the operator lists the hosts in a file.', 'crates/js-lease-server/src/lib.rs', '[`StaticPoolSource`] reads the operator\'s pool file.'],
  ['concepts', '<strong>Agents that join</strong>: an agent says hello with a join token,', 'crates/js-lease-server/src/lib.rs', 'machine\'s agent calls two routes with a join token'],
  ['concepts', 'offers its CPU, memory and GPUs, and heartbeats.', 'crates/js-agent/src/lib.rs', 'tells a lease server what the machine offers, and keeps telling it that the machine is alive'],
  ['concepts', 'The pool file for them is <code>--agent-pool-config</code>', 'crates/js-lease-server/src/lib.rs', '`--agent-pool-config <path>` | the pool agents join, as TOML'],
  ['concepts', '<strong>An elastic pool</strong>: a provider launches machines', 'crates/js-lease-server/src/lib.rs', 'A pool whose provider launches machines (an elastic pool)'],
  ['concepts', 'for requests that wait.', 'crates/js-lease-server/src/lib.rs', 'launches machines for requests that wait'],
  ['concepts', 'A pool\'s <code>launch</code> is <code>plan</code>', 'crates/js-lease-server/src/lib.rs', '`launch` is `plan` unless the file says `live`'],
  ['concepts', 'a live pool also has caps on instances and on spend per hour and per day, and a brake:', 'crates/js-lease-server/src/lib.rs', 'launches machines for requests that wait, within `max_instances`, `max_spend_per_hour` and `max_spend_per_day`'],
  ['concepts', '<code>memory_schedulable_percent</code> is a whole number from 1 to 100, 100 by default', 'crates/js-lease-server/src/lib.rs', '`memory_schedulable_percent`, a whole number from 1 to 100 (default 100)'],
  ['concepts', 'A pool file names a class', 'crates/js-lease-server/src/lib.rs', '`--agent-pool-config <path>` | the pool agents join, as TOML: `name`, `class`,'],
  ['concepts', 'It promises four things that a big share does not', 'docs/design/whole-node.md', 'promises four things a big share does not'],
  ['concepts', 'and nothing else is placed on the host until it is released.', 'docs/design/whole-node.md', 'nothing else is placed on the host'],
  ['concepts', '<strong>Quiet.</strong> Before the client can use it the host has no run, no VM and no leftover of an earlier job, the post-job checks are clean and the host is not tainted.', 'docs/design/whole-node.md', 'the host has no run, no VM and no leftover of an earlier job, the post-job checks are clean and the host is not tainted.'],
  ['concepts', 'so a stream of small requests cannot starve it.', 'docs/design/whole-node.md', 'so a stream of small requests cannot starve it'],
  ['concepts', '<strong>Capped.</strong> Pool limits bound how many hosts are reserved or held whole.', 'docs/design/whole-node.md', 'bound how many hosts are reserved or held whole'],
  ['concepts', 'The pool file\'s whole-node keys are <code>max_whole_node</code>, <code>max_draining</code>, <code>max_whole_node_per_consumer</code>, <code>keep_for_ordinary</code>, <code>allow_last_capable</code>, <code>drain_after</code>, <code>drain_deadline</code>, <code>reserve_backoff</code>, <code>max_reserve_attempts</code>, <code>prepare_timeout</code>, <code>max_replace</code> and <code>regain_grace</code>', 'crates/js-lease-server/src/agent_pool.rs', 'Every key is optional. max_whole_node: Option<usize>, max_draining: Option<usize>, max_whole_node_per_consumer: Option<usize>, keep_for_ordinary: Option<usize>, allow_last_capable: Option<bool>, drain_after: Option<String>, drain_deadline: Option<String>, reserve_backoff: Option<String>, max_reserve_attempts: Option<u32>, prepare_timeout: Option<String>, max_replace: Option<u32>, regain_grace: Option<String>'],
  ['concepts', 'The placement core is <code>js-sched</code>, a pure state machine', 'crates/js-sched/src/lib.rs', 'and assignment of actions to workers, as a pure state machine.'],
  ['concepts', 'it takes time as an input', 'crates/js-sched/src/lib.rs', 'each carrying the farm time'],
  ['concepts', 'and reads no clock', 'crates/js-sched/src/lib.rs', 'This is a pure crate: no async runtime, network, clock, randomness or hashed collections'],
  ['concepts', 'It admits an action to a worker when what the worker\'s readings measure, plus the predicted cost of the leases no reading covers yet', 'crates/js-sched/src/lib.rs', 'is admitted to a worker when what the worker\'s readings measure, plus the predicted cost of the leases no reading covers yet'],
  ['concepts', 'plus the predicted cost of the new lease', 'crates/js-sched/src/lib.rs', 'plus the cost of the new lease'],
  ['concepts', 'stays within the worker\'s CPU and its memory', 'crates/js-sched/src/lib.rs', 'stays within its CPU and its memory'],
  ['concepts', 'each charged at a cost the lease keeps from its grant to its result', 'crates/js-sched/src/lib.rs', 'each charged at a cost the lease keeps from its grant to its result'],
  ['concepts', 'The cost comes from earlier runs of similar actions (<code>js-sizing</code>)', 'crates/js-sizing/src/lib.rs', 'predicts the CPU and memory an action will use from what earlier runs of similar actions used.'],
  ['concepts', 'where <code>M</code> is the largest uncensored sample of the last 256 or more runs,', 'crates/js-sizing/src/lib.rs', 'where `M` is the maximum uncensored sample over the last 256 or more runs'],
  ['concepts', '; 2000 m for a key with no CPU sample</td>', 'crates/js-sizing/src/summary.rs', 'pub const COLD_CPU_MILLIS: u32 = 2000;'],
  ['concepts', 'Mode <code>off</code> of the load rule is a rollback switch to the booked rule', 'crates/js-sched/src/lib.rs', 'is the rollback switch to the booked rule'],
  ['install', 'There is no lease server for macOS', 'docs/release.md', 'there is no macOS `js-lease-server`'],
  ['install', 'not codesigned or notarized.', 'docs/release.md', 'are not codesigned or notarized'],
  ['install', '<span class="tag tag-planned">not proven</span> That the macOS archives come out byte-identical when a partly published release is re-run, nor that the Linux arm64 ones do when built again on a hosted arm64 runner.', 'docs/release.md', 'Not yet proven: that the Mac archives come out byte-identical'],
  ['install', '<span class="tag tag-planned">not proven</span> That the macOS archives come out byte-identical when a partly published release is re-run, nor that the Linux arm64 ones do when built again on a hosted arm64 runner.', 'docs/release.md', 'nor that the Linux arm64 ones do when they are built again on a hosted arm64 runner'],
  ['install', '<span class="tag tag-planned">none</span> A Debian package of the agent for arm64: the package is built for x86_64 only.', 'docs/release.md', 'The js-agent Debian package (`.github/workflows/js-agent-deb.yml`) is built for x86_64 only: there is no arm64 deb'],
  ['install', '<span class="tag tag-planned">none</span> A Debian package of the agent for arm64: the package is built for x86_64 only.', '.github/workflows/js-agent-deb.yml', 'TARGET: x86_64-unknown-linux-musl'],
  ['install', 'the SHA-256 of the fifteen archives', 'docs/release.md', 'SHA-256 of the fifteen archives'],
  ['install', 'the MDM that enrolls Macs as compute hosts, set up on one Linux host', 'crates/js-mdm/src/lib.rs', 'js-mdm is jobscheduler\'s MDM for enrolling Macs as compute hosts'],
  ['install', 'the bundle tool <code>-p \'js-bundle-*-x86_64-unknown-linux-musl.tar.gz\'</code>.', 'docs/release.md', 'for the bundle tool `-p \'js-bundle-*-x86_64-unknown-linux-musl.tar.gz\'`'],
  ['install', 'There is no package repository, no container image and no published installer.', '.github/workflows/js-agent-macos-pkg.yml', 'It publishes nothing'],
  ['install', 'The macOS installer package of the agent, in <code>packaging/js-agent-macos</code>, is built the same way by its own workflow,', 'packaging/js-agent-macos/README.md', 'A signed installer package (`js-agent-<version>-aarch64-apple-darwin.pkg`)'],
  ['install', 'which publishes nothing either.', '.github/workflows/js-agent-macos-pkg.yml', 'It publishes nothing: the packages and their SHA256SUMS are workflow artifacts for seven days'],
  ['install', 'Every push to <code>main</code> that changes code that ships is released as the next version, after the checks of', 'docs/release.md', 'every push to `main` that changes code that ships is released as the next version'],
  ['install', 'That workflow publishes no release, no package repository and no registry', '.github/workflows/js-agent-deb.yml', 'no release, no package repository, no registry'],
  ['install', '<strong>The agent\'s Debian package is not published.</strong> The repository builds it with nfpm, checks what it contains and attests it, as a workflow artifact kept for seven days.', '.github/workflows/js-agent-deb.yml', 'workflow artifact for seven days'],
  ['install', 'The default listen address is <code>127.0.0.1:7300</code>; off the loopback the server needs both', 'crates/js-lease-server/src/lib.rs', 'default `127.0.0.1:7300`; off the loopback it needs both TLS flags'],
  ['install', 'at least 32 bytes), a state file, and a pool file for the agents that will join.', 'crates/js-lease-server/src/lib.rs', 'holding one token of printable ASCII of at least 32 bytes'],
  ['install', 'The server needs a token for clients (a regular file owned by the server\'s user, mode 0600 or 0400, one token of printable ASCII,', 'crates/js-lease-server/src/lib.rs', 'a regular file owned by the server\'s user, mode 0600 or 0400'],
  ['install', 'A start the server refuses prints the flag at fault and exits with status 2', 'crates/js-lease-server/src/lib.rs', 'A start that is refused prints the flag at fault and exits with status 2'],
  ['install', 'one per line, each as strong as the client token, in an owner-only file.', 'crates/js-lease-server/src/lib.rs', 'one per line, each as strong as `--token-file`\'s, in an owner-only file'],
  ['install', 'write the pool\'s join token to <code>/etc/js-agent/token</code>, mode 0600', 'packaging/js-agent/js-agent.service', 'write the pool\'s join token to /etc/js-agent/token, mode 0600'],
  ['install', 'mount a disk of its own at <code>/var/lib/js-agent/scratch</code>', 'packaging/js-agent/js-agent.service', 'mount a disk of its own at /var/lib/js-agent/scratch'],
  ['install', 'the shipped <code>--server</code> is a placeholder the agent refuses', 'packaging/js-agent/js-agent.service', 'the shipped --server is a placeholder the agent refuses'],
  ['install', 'A missing mount fails silently.', 'packaging/js-agent/js-agent.service', 'With the mount of step 3 missing this exits 0 and starts nothing, silently'],
  ['install', 'The agent runs as root, with a rootful Docker or Podman beside it, and refuses to start under <code>ProtectSystem=</code>, <code>PrivateMounts=</code> or <code>PrivatePIDs=</code>.', 'packaging/js-agent/js-agent.service', 'it refuses to start under ProtectSystem=, PrivateMounts= or PrivatePIDs='],
  ['install', 'the agent refuses a state root on the same disk as the scratch root', 'packaging/js-agent/js-agent.service', 'The agent refuses a state root on the same disk as the scratch root'],
  ['install', 'It exits 2 for a wrong flag or file, a job engine it cannot use, or a server that refuses the host for good, and', 'packaging/js-agent/js-agent.service', '2 for a wrong flag or file, a job engine it cannot use, or a server that refuses the host for good'],
  ['install', 'the unit restarts it every 30 seconds, so a refusal is read from the journal.', 'packaging/js-agent/js-agent.service', 'RestartSec=30'],
  ['install', 'The unit runs <code>/usr/bin/js-agent</code> and', 'packaging/js-agent/js-agent.service', 'ExecStart=/usr/bin/js-agent'],
  ['install', '<code>http://</code> is accepted only for <code>localhost</code>, <code>127.0.0.0/8</code> and <code>::1</code>', 'crates/js-agent/src/lib.rs', '`http://` only for `localhost`, `127.0.0.0/8` and `::1`'],
  ['install', 'sets a host up for it and installs it, and the unit is not enabled or started', 'packaging/js-agent/crun_host_setup.sh', 'the unit is NOT enabled or started'],
  ['install', 'it is refused at start', 'crates/js-agent/src/lib.rs', '`--allow-host-jobs` | refused at start'],
  ['install', '<td><code>js-lease-server-X.Y.Z-x86_64-unknown-linux-musl.tar.gz</code>,', 'docs/release.md', '`js-lease-server-X.Y.Z-x86_64-unknown-linux-musl.tar.gz` | the `js-lease-server` binary, `LICENSE` and `updater.d/js-lease-server.toml`'],
  ['install', '<code>js-lease-server-X.Y.Z-aarch64-unknown-linux-musl.tar.gz</code></td><td>the lease server</td>', 'docs/release.md', '`js-lease-server-X.Y.Z-aarch64-unknown-linux-musl.tar.gz` | the `js-lease-server` binary for arm64 Linux hosts, `LICENSE` and `updater.d/js-lease-server.toml`'],
  ['install', '<code>js-X.Y.Z-aarch64-unknown-linux-musl.tar.gz</code>,', 'docs/release.md', '`js-X.Y.Z-aarch64-unknown-linux-musl.tar.gz` | the `js` binary for arm64 Linux hosts and `LICENSE`'],
  ['install', '<code>js-agent-X.Y.Z-aarch64-unknown-linux-musl.tar.gz</code>,', 'docs/release.md', '`js-agent-X.Y.Z-aarch64-unknown-linux-musl.tar.gz` | the `js-agent` binary for arm64 Linux hosts and `LICENSE`'],
  ['install', '<code>js-bundle-X.Y.Z-aarch64-unknown-linux-musl.tar.gz</code>,', 'docs/release.md', '`js-bundle-X.Y.Z-aarch64-unknown-linux-musl.tar.gz` | the `js-bundle` binary for arm64 Linux hosts and `LICENSE`'],
  ['install', '<code>js-mdm-X.Y.Z-aarch64-unknown-linux-musl.tar.gz</code>', 'docs/release.md', '`js-mdm-X.Y.Z-aarch64-unknown-linux-musl.tar.gz` | the `js-mdm` binary for arm64 Linux hosts and `LICENSE`'],
  ['install', '<td><code>js-X.Y.Z-x86_64-unknown-linux-musl.tar.gz</code>,', 'docs/release.md', '`js-X.Y.Z-x86_64-unknown-linux-musl.tar.gz` | the `js` binary and `LICENSE`'],
  ['install', '<code>js-X.Y.Z-aarch64-apple-darwin.tar.gz</code></td><td>the client</td>', 'docs/release.md', '`js-X.Y.Z-aarch64-apple-darwin.tar.gz` | the `js` binary for Apple silicon Macs and `LICENSE`'],
  ['install', '<td><code>js-agent-X.Y.Z-x86_64-unknown-linux-musl.tar.gz</code>,', 'docs/release.md', '`js-agent-X.Y.Z-x86_64-unknown-linux-musl.tar.gz` | the `js-agent` binary and `LICENSE`'],
  ['install', '<code>js-agent-X.Y.Z-aarch64-apple-darwin.tar.gz</code></td><td>the agent</td>', 'docs/release.md', '`js-agent-X.Y.Z-aarch64-apple-darwin.tar.gz` | the `js-agent` binary for Apple silicon Macs and `LICENSE`'],
  ['install', '<td><code>js-bundle-X.Y.Z-x86_64-unknown-linux-musl.tar.gz</code>,', 'docs/release.md', '`js-bundle-X.Y.Z-x86_64-unknown-linux-musl.tar.gz` | the `js-bundle` binary (packs, prints and verifies darwin bundles) and `LICENSE`'],
  ['install', '<code>js-bundle-X.Y.Z-aarch64-apple-darwin.tar.gz</code></td><td>the tool that packs, prints and verifies darwin bundles</td>', 'docs/release.md', '`js-bundle-X.Y.Z-aarch64-apple-darwin.tar.gz` | the `js-bundle` binary for Apple silicon Macs and `LICENSE`'],
  ['install', '<code>js-mdm-X.Y.Z-x86_64-unknown-linux-musl.tar.gz</code>', 'docs/release.md', '`js-mdm-X.Y.Z-x86_64-unknown-linux-musl.tar.gz` | the `js-mdm` binary and `LICENSE`'],
  ['install', '<td><code>js-mac-session-X.Y.Z-aarch64-apple-darwin.tar.gz</code>,', 'docs/release.md', '`js-mac-session-X.Y.Z-aarch64-apple-darwin.tar.gz` | the `js-mac-session` root helper for Apple silicon Macs and `LICENSE`'],
  ['install', '<code>js-mac-provision-X.Y.Z-aarch64-apple-darwin.tar.gz</code></td><td>the root helper and the pf anchor installer a Mac node installs</td>', 'docs/release.md', '`js-mac-provision-X.Y.Z-aarch64-apple-darwin.tar.gz` | the `js-mac-provision` pf anchor installer for Apple silicon Macs and `LICENSE`'],
  ['install', '<code>SHA256SUMS</code></td><td>the SHA-256 of the fifteen archives, the three packages and the index', 'docs/release.md', '`SHA256SUMS` | SHA-256 of the fifteen archives, the three packages and the index'],
  ['install', 'The lease server, the <code>js</code> client and the agent are Rust binaries built by the repository\'s release workflow', 'docs/release.md', 'builds the binaries for `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl` (`js-lease-server`, `js`, `js-agent`'],
  ['install', 'The Linux binaries are static (musl, x86_64 and arm64);', 'docs/release.md', 'The Linux binaries are statically linked against musl, for both architectures'],
  ['install', '(musl, x86_64 and arm64)', 'docs/release.md', 'builds the binaries for `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl`'],
  ['install', 'A host gets the agent from the release archive and the unit in <code>packaging/js-agent</code>, as the steps below say.', 'packaging/js-agent/nfpm.yaml', 'src: packaging/js-agent/js-agent.service dst: /usr/lib/systemd/system/js-agent.service'],
  ['install', '<code>systemctl edit js-agent</code>: clear <code>ExecStart=</code> and set the whole line again with this host\'s <code>--server</code> and <code>--pool</code>', 'packaging/js-agent/js-agent.service', 'systemctl edit js-agent: clear ExecStart= and set the whole line again with this host\'s --server and --pool'],
  ['install', '<code>systemctl enable --now js-agent</code>', 'packaging/js-agent/js-agent.service', 'systemctl enable --now js-agent'],
  ['install', 'With the mount of step 3 absent, step 5 exits 0 and starts nothing', 'packaging/js-agent/js-agent.service', 'With the mount of step 3 missing this exits 0 and starts nothing'],
  ['install', '<code>systemctl status js-agent</code> shows the unmet <code>ConditionPathIsMountPoint</code>', 'packaging/js-agent/js-agent.service', '`systemctl status js-agent` shows the unmet ConditionPathIsMountPoint'],
  ['install', 'The agent reads no environment variable and no configuration file: its flags are the <code>ExecStart</code> line', 'packaging/js-agent/js-agent.service', '(it reads no environment variable and no config file), so the host\'s values are the flags of ExecStart'],
  ['install', '<code>--server</code> is <code>https://host:port</code>', 'crates/js-agent/src/lib.rs', '`--server <url>` | required: `https://host:port`'],
  ['install', 'The unit\'s <code>--runtime docker</code> runs jobs', 'packaging/js-agent/js-agent.service', '--runtime docker --container-bin /usr/bin/docker'],
  ['install', 'runs jobs through the engine at <code>--container-bin</code>', 'crates/js-agent/src/lib.rs', '`docker` (the engine program of `--container-bin`'],
  ['install', '<code>--allow-container-engine</code> is the opt-in the engine needs', 'packaging/js-agent/js-agent.service', '--runtime docker --container-bin /usr/bin/docker --allow-container-engine'],
  ['install', 'an agent given no <code>--runtime</code> runs jobs with <code>crun</code>, the default Linux runtime,', 'crates/js-agent/src/lib.rs', 'which is the default Linux runtime: a Linux agent given no `--runtime` and no `--native-*` flag runs `crun`'],
  ['install', 'and exits 2 on a host that fails the crun check, naming each missing piece.', 'crates/js-agent/src/lib.rs', 'With `crun`, named or the default, the agent checks the host before it registers and exits 2 naming every piece that is missing, and never runs the engine instead'],
  ['install', 'A second unit, <code>js-agent-crun.service</code>, runs jobs with <code>crun</code> on a farm node', 'packaging/js-agent/js-agent-crun.service', 'js-agent on a Linux farm node that runs jobs with crun'],
  ['install', '<code>packaging/js-agent/crun_host_setup.sh</code> sets a host up for it', 'packaging/js-agent/crun_host_setup.sh', 'sets up a Linux host to run js-agent with `--runtime crun`'],
  ['install', '<span class="tag tag-planned">none</span> A published package or', '.github/workflows/js-agent-deb.yml', 'It publishes nothing: no release, no package repository, no registry'],
  ['install', '<span class="tag tag-planned">none</span> A lease server for macOS.', 'docs/release.md', 'there is no macOS `js-lease-server`'],
  ['install', '<span class="tag tag-planned">not available yet</span> The agent flag <code>--allow-host-jobs</code>: it is refused at start in every configuration, because no runtime launches a host job yet (crun and docker run contained jobs only).', 'crates/js-agent/src/lib.rs', '`--allow-host-jobs` | refused at start in every configuration: no runtime launches a host job yet (crun and docker run contained jobs only)'],
  ['install', 'Take the archives <span class="tag tag-ok">works today</span>', 'docs/release.md', '`publish` creates the GitHub release for `vX.Y.Z`'],
  ['install', '<code>ci</code> other than the macOS job have passed.', 'docs/release.md', 'after the jobs of `ci` other than `macos` have passed on it'],
  ['install', 'A <a href="https://github.com/komira-ai/jobscheduler/releases">release</a> for version <code>X.Y.Z</code> carries:', 'docs/release.md', 'For version `X.Y.Z`:'],
  ['install', 'Each archive unpacks to a directory named like the archive without <code>.tar.gz</code>', 'docs/release.md', 'Each archive unpacks to a directory named like the archive without `.tar.gz`.'],
  ['install', 'and holds the binary and <code>LICENSE</code>', 'docs/release.md', '| the `js` binary and `LICENSE` |'],
  ['install', 'The Linux lease-server archives also hold <code>updater.d/js-lease-server.toml</code>', 'docs/release.md', 'The Linux `js-lease-server` archives, of both architectures, also hold `updater.d/js-lease-server.toml`'],
  ['install', '<code>js-agent-X.Y.Z-linux-x86_64.tar.zst</code>, <code>js-agent-X.Y.Z-linux-arm64.tar.zst</code>, <code>js-agent-X.Y.Z-macos-arm64.tar.zst</code></td><td>the agent as a hostkeeper package', 'docs/release.md', '`js-agent-X.Y.Z-linux-x86_64.tar.zst` | the hostkeeper package of `js-agent` for x86_64 Linux hosts'],
  ['install', '<code>js-agent-index.json</code></td><td>the index of the three packages', 'docs/release.md', '`js-agent-index.json` | the release index of the three packages'],
  ['install', 'Verify them <span class="tag tag-ok">works today</span>', 'docs/release.md', 'gh attestation verify js-lease-server-X.Y.Z-x86_64-unknown-linux-musl.tar.gz -R komira-ai/jobscheduler'],
  ['install', 'This needs the GitHub CLI (<code>gh</code>), signed in.', 'docs/release.md', 'Needs the GitHub CLI (`gh`), signed in.'],
  ['install', 'The checksum proves the download is intact', 'docs/release.md', 'The checksum check proves the download is intact.'],
  ['install', 'the attestation proves the file was built from this repository by the <code>release</code> workflow, from <code>main</code>', 'docs/release.md', 'The attestation check proves the file was built from this repository by the `release` workflow, from `main`'],
  ['install', 'at the commit the tag names', 'docs/release.md', 'which is the commit the tag `vX.Y.Z` names'],
  ['install', 'For the client use <code>-p \'js-[0-9]*-x86_64-unknown-linux-musl.tar.gz\'</code>,', 'docs/release.md', 'For the client, use `-p \'js-[0-9]*-x86_64-unknown-linux-musl.tar.gz\'` instead'],
  ['install', 'for the agent <code>-p \'js-agent-*-x86_64-unknown-linux-musl.tar.gz\'</code>, and for', 'docs/release.md', 'and for the agent `-p \'js-agent-*-x86_64-unknown-linux-musl.tar.gz\'`'],
  ['install', 'On an arm64 Linux host, write <code>aarch64-unknown-linux-musl</code> in their place, as in <code>-p \'js-agent-*-aarch64-unknown-linux-musl.tar.gz\'</code>.', 'docs/release.md', 'On an arm64 Linux host, write `aarch64-unknown-linux-musl` where these say `x86_64-unknown-linux-musl`'],
  ['install', 'On an arm64 Linux host, write <code>aarch64-unknown-linux-musl</code> in their place, as in <code>-p \'js-agent-*-aarch64-unknown-linux-musl.tar.gz\'</code>.', 'docs/release.md', 'for the agent `-p \'js-agent-*-aarch64-unknown-linux-musl.tar.gz\'`'],
  ['install', '<code>--boot resume</code>, which refuses a missing one.', 'crates/js-lease-server/src/lib.rs', '`resume` needs it'],
  ['install', 'The pool\'s join tokens are in the file after <code>--agent-token-file</code>,', 'crates/js-lease-server/src/lib.rs', '`--agent-token-file <path>` | the pool\'s shared join tokens'],
  ['install', '<code>--tls-cert</code> and', 'crates/js-lease-server/src/lib.rs', '`--tls-cert`, `--tls-key` | PEM files; both or neither'],
  ['install', '<code>--tls-key</code> and refuses to start without them.', 'crates/js-lease-server/src/lib.rs', '`--tls-cert`, `--tls-key` | PEM files; both or neither'],
  ['install', 'Install the agent on a Linux host <span class="tag tag-ok">works today</span>', 'packaging/js-agent/js-agent.service', 'js-agent on a Linux host, installed by the js-agent deb (packaging/js-agent).'],
  ['install', 'reads <code>/usr/lib/tmpfiles.d/js-agent.conf</code>.', 'packaging/js-agent/js-agent.service', 'ExecStartPre=+systemd-tmpfiles --create /usr/lib/tmpfiles.d/js-agent.conf'],
  ['install', 'Put the binary at the first path, <code>js-agent.service</code> in <code>/usr/lib/systemd/system</code> and <code>tmpfiles.conf</code> from <code>packaging/js-agent</code> at the second', 'packaging/js-agent/nfpm.yaml', 'src: packaging/js-agent/js-agent.service dst: /usr/lib/systemd/system/js-agent.service'],
  ['install', 'Put the binary at the first path, <code>js-agent.service</code> in <code>/usr/lib/systemd/system</code> and <code>tmpfiles.conf</code> from <code>packaging/js-agent</code> at the second', 'packaging/js-agent/nfpm.yaml', 'src: packaging/js-agent/tmpfiles.conf dst: /usr/lib/tmpfiles.d/js-agent.conf'],
  ['install', 'where the Debian package puts them', 'packaging/js-agent/nfpm.yaml', 'src: target/x86_64-unknown-linux-musl/release/js-agent dst: /usr/bin/js-agent'],
  ['install', 'A host gets the agent from the release archive and the unit in <code>packaging/js-agent</code>, as the steps below say.', 'docs/release.md', '`js-agent-X.Y.Z-x86_64-unknown-linux-musl.tar.gz` | the `js-agent` binary and `LICENSE`'],
  ['install', 'Then, as root:', 'packaging/js-agent/js-agent.service', 'The sequence, as root:'],
  ['install', 'make the directories:', 'packaging/js-agent/js-agent.service', 'makes the directories below'],
  ['install', '<code>systemd-tmpfiles --create /usr/lib/tmpfiles.d/js-agent.conf</code>;', 'packaging/js-agent/js-agent.service', '1. systemd-tmpfiles --create /usr/lib/tmpfiles.d/js-agent.conf'],
  ['install', 'The token file is a regular file owned by you, mode 0600 or 0400.', 'crates/js-cli/src/lib.rs', 'required: a regular file owned by you, mode 0600 or 0400'],
  ['install', 'container image of the agent, the lease server or the client.', 'docs/release.md', 'uploads the archives, the packages, the index and `SHA256SUMS` to it'],
  ['install', '<span class="tag tag-planned">none</span> A published package or', '.github/workflows/js-agent-macos-pkg.yml', 'It publishes nothing: the packages and their SHA256SUMS are workflow artifacts for seven days'],
  ['install', 'the macOS ones are for Apple silicon and are', 'docs/release.md', 'The macOS assets (`aarch64-apple-darwin`)'],
  ['kbf', 'kbf is the first user of jobscheduler and the repository it was lifted out of.', 'docs/design.md', 'First user: kbf'],
  ['kbf', 'The first user depends on these crates by git revision: it names every <code>js-*</code> crate once, in its root <code>[workspace.dependencies]</code>, all at the same revision of this repository, and', 'docs/layering.md', 'names every `js-*` crate once, in its root `[workspace.dependencies]`, all at the same revision of this repository'],
  ['kbf', 'a bump is a deliberate pull request there that runs its full CI.', 'docs/layering.md', 'a bump is a deliberate pull request there'],
  ['kbf', 'Where a crate moved, kbf keeps its name as a shim that re-exports the crate here.', 'docs/layering.md', 'keeps the crate\'s name as a shim (`pub use js_x::*`)'],
  ['kbf', 'This repository never depends on kbf:', 'docs/layering.md', 'This repository never depends on the other one'],
  ['kbf', '<code>deny.toml</code> bans kbf\'s crates by name.', 'docs/layering.md', '`deny.toml` also bans kbf\'s crates by name'],
  ['kbf', '<code>js-agent</code>, which the design documents call the generic half of the kbf daemon', 'docs/design.md', '`js-agent` is the generic half of the kbf daemon'],
  ['kbf', 'only the <code>legacy</code> one is built into a running server;', 'docs/design/kbf-today.md', 'only `legacy` is built into a running server'],
  ['kbf', 'The same document says that nothing in kbf runs <code>js-agent</code>, and that', 'docs/design/kbf-today.md', 'Nothing in kbf runs `js-agent`'],
  ['kbf', 'no lease client of jobscheduler was in use.', 'docs/design/kbf-today.md', 'no lease client of js is on the farm'],
  ['kbf', '<code>docs/design/kbf-on-js-agent.md</code> is marked "design only"', 'docs/design/kbf-on-js-agent.md', 'Status: design only'],
  ['kbf', 'nothing in it is built or approved to run', 'docs/design/kbf-on-js-agent.md', 'Nothing here is built or approved to run'],
  ['kbf', '<td>A kbf flag, <code>--executor</code>, chooses between the agent and kbf\'s own daemon, as the way back</td><td><span class="tag tag-planned">design only</span></td>', 'docs/design/kbf-on-js-agent.md', '`--executor` (`js-agent` or `kbf-daemon`, default `kbf-daemon`) as the rollback switch'],
  ['kbf', '<code>/v1/leases/{id}/runs</code>', 'docs/design/kbf-on-js-agent.md', '`POST/GET/DELETE /v1/leases/{id}/runs`'],
  ['kbf', 'The document keeps jobscheduler generic: kbf\'s own types stay in kbf, and jobscheduler gets only generic fields.', 'docs/design/kbf-on-js-agent.md', 'js gets only generic fields'],
  ['kbf', '<td>kbf depends on the <code>js-*</code> crates by git revision</td><td><span class="tag tag-ok">works today</span></td>', 'docs/layering.md', '(kbf, the first one) depends on the crates by git revision'],
  ['kbf', '<td>kbf depends on the <code>js-*</code> crates by git revision</td><td><span class="tag tag-ok">works today</span></td>', 'docs/design/kbf-today.md', 'The jobscheduler libraries are git dependencies at one revision'],
  ['kbf', '<td>kbf\'s running server uses the <code>js</code> scheduler core</td><td><span class="tag tag-planned">not at the revision the document names</span></td>', 'docs/design/kbf-today.md', 'only `legacy` is built into a running server'],
  ['kbf', '<td>kbf runs <code>js-agent</code> on its hosts</td><td><span class="tag tag-planned">not at the revision the document names</span></td>', 'docs/design/kbf-today.md', 'Nothing in kbf runs `js-agent`'],
  ['kbf', 'A lease server is deployed; kbf leases whole hosts from it', 'docs/design/kbf-on-js-agent.md', 'A js-lease-server is deployed on the farm. kbf-server leases whole hosts from it'],
  ['kbf', 'kbf\'s client: not built', 'docs/design/kbf-today.md', 'no lease client of js is on the farm'],
  ['kbf', 'kbf submits each action as a run on its lease, through <code>POST</code>, <code>GET</code> and <code>DELETE</code> on <code>/v1/leases/{id}/runs</code>', 'docs/design/kbf-on-js-agent.md', 'submits each action as a run through its run API (`POST/GET/DELETE /v1/leases/{id}/runs`)'],
  ['kbf', '<code>js-agent</code> stands alone on every host, with no kbf process beside it', 'docs/design/kbf-on-js-agent.md', '`js-agent` is standalone on every host: no kbf process beside it'],
  ['kbf', 'kbf on it: design only', 'docs/design/kbf-on-js-agent.md', 'Status: design only'],
  ['kbf', '<td>Each action runs in its own digest-pinned image;', 'docs/design/kbf-on-js-agent.md', 'the action runs in its own digest-pinned image'],
  ['kbf', 'the agent returns the container\'s exit code</td><td><span class="tag tag-planned">design only</span></td>', 'docs/design/kbf-on-js-agent.md', 'js-agent returns only the container\'s exit code'],
  ['kbf', 'kbf\'s frontend verifies what it reads back</td><td><span class="tag tag-planned">design only</span></td>', 'docs/design/kbf-on-js-agent.md', 'The frontend verifies every byte and adopts in place'],
  ['kbf', 'What kbf takes from jobscheduler today, and the design for running kbf actions on the agent, which is not built.', 'docs/design/kbf-on-js-agent.md', 'Nothing here is built or approved to run'],
  ['kbf', 'This page separates three things: what kbf takes from this repository today, what the documents say kbf does with it, and the design for running kbf\'s actions on the agent, which is not built.', 'docs/design/kbf-on-js-agent.md', 'Nothing here is built or approved to run'],
  ['kbf', 'a test refuses a <code>kbf-*</code> dependency, and', 'crates/xtask/tests/layering.rs', 'Asserts that a dependency on a `kbf-*` crate is refused'],
  ['kbf', '<code>js-sched</code>, the pure scheduler core,', 'crates/js-sched/src/lib.rs', 'The scheduler core: operation states, leases and fencing, result acceptance, and assignment of actions to workers, as a pure state machine.'],
  ['kbf', 'including admission by load (see <a href="concepts.html#placement">placement</a>);', 'crates/js-sched/src/lib.rs', 'admission by measured and predicted load'],
  ['kbf', '<code>js-sizing</code>, the estimator that predicts an action\'s CPU and memory from earlier runs', 'crates/js-sizing/src/lib.rs', 'The size estimator: predicts the CPU and memory an action will use from what earlier runs of similar actions used.'],
  ['kbf', '<code>js-agent</code>, which the design documents call the generic half of the kbf daemon', 'docs/design.md', '`js-agent` is the generic half of the kbf daemon'],
  ['kbf', 'According to <code>docs/design/kbf-today.md</code>, checked against the kbf revision it names,', 'docs/design/kbf-today.md', 'These points were checked against kbf `89af8f2ef`'],
  ['kbf', 'kbf has two scheduler cores selected by <code>--scheduler-core</code>, and', 'docs/design/kbf-today.md', '`--scheduler-core` selects `legacy` or `js`'],
  ['kbf', '<code>js</code> is refused at start unless a test-only feature is built', 'docs/design/kbf-today.md', '`js` is refused at start unless the test-only `farm-core-js` feature is built'],
  ['kbf', '<code>docs/design/kbf-on-js-agent.md</code> is marked "design only"', 'docs/design/kbf-on-js-agent.md', 'Status: design only'],
  ['kbf', 'the agent is standalone', 'crates/js-agent/src/lib.rs', 'runs on a machine, tells a lease server what the machine offers'],
  ['kbf', '<td>The task writes its outputs with one presigned upload, and', 'docs/design/kbf-on-js-agent.md', 'one standard SigV4 presigned PUT for one key the frontend chose'],
  ['kbf', '<strong>About the kbf facts.</strong> The statements about kbf below are the repository\'s own documents, written against a kbf revision those documents name.', 'docs/design.md', 'at commit 45e420656, the commit this document was written against'],
  ['kbf', 'This repository does not contain kbf, and kbf may have moved since.', 'docs/layering.md', 'how another repository (kbf, the first one) depends on the crates by git revision'],
  ['kbf', 'The code here was written for kbf and lifted out of it', 'deny.toml', 'the one its crates were lifted out of (kbf depends on'],
  ['index', 'An agent on each host tells the server what the host offers, keeps telling it that the host is alive, and runs the container or native jobs the server places on it.', 'crates/js-agent/src/lib.rs', 'tells a lease server what the machine offers'],
  ['index', 'A client submits a job to a server, which admits it when a host, or a machine provider of the pool, could ever serve it,', 'crates/js-lease-server/src/lib.rs', 'it is accepted when a capable host exists, else when a launching machine provider of the pool may offer a machine for it'],
  ['index', 'could ever serve it,', 'crates/js-lease-server/src/job_api/submit.rs', 'could ever serve the job'],
  ['index', 'and records it as queued.', 'crates/js-lease-server/src/job_api/submit.rs', 'The job is inserted as queued in one transaction of the store'],
  ['index', 'Jobs on <em>whole hosts</em>, and a core that places work by load.', 'crates/js-lease-server/src/lib.rs', 'Jobs are placed only under `js-sched`'],
  ['index', 'Jobs on <em>whole hosts</em>, and a core that places work by load.', 'crates/js-sched/src/lib.rs', 'admission by measured and predicted load'],
  ['index', 'Jobs on whole hosts, placement by CPU and memory', 'crates/js-lease-server/src/lib.rs', 'Jobs are placed only under `js-sched`'],
  ['index', 'Jobs on whole hosts, placement by CPU and memory', 'crates/js-sched/src/lib.rs', 'placement of a CPU, memory and GPU request onto worker capacity'],
  ['index', 'a server with a job service, an agent on each host, and a pure placement core', 'crates/js-lease-server/src/lib.rs', 'the job service that places work on them'],
  ['index', 'a server with a job service, an agent on each host, and a pure placement core', 'crates/js-agent/src/lib.rs', 'runs on a machine, tells a lease server what the machine offers'],
  ['index', 'a server with a job service, an agent on each host, and a pure placement core', 'crates/js-sched/src/lib.rs', 'This is a pure crate'],
  ['index', 'This is a recording of one run on one machine: the server started with its job service on and a pool file, and the <code>js</code> client submitting a GPU job and listing the jobs.', 'crates/xtask/src/site_demo.rs', 'starts the server on this machine with the job service on, asks the job service for a GPU job and lists the jobs'],
  ['index', 'No agent joins the pool, so no host could ever serve the job: the server refuses it as <code>no-capable-host</code>', 'crates/xtask/src/site_demo.rs', 'No agent joins the pool, so no host could ever serve the job: the server refuses it as `no-capable-host`'],
  ['index', 'and records nothing, and the list is empty.', 'crates/xtask/src/site_demo.rs', 'and records nothing, and the list is empty'],
  ['index', 'A new recording is the same, word for word.', 'crates/xtask/src/site_demo.rs', 'a new recording is the committed one word for word'],
  ['index', '<code>cargo xtask site-demo</code> in the repository builds both programs and runs these commands again; the recording is <code>site/test/demo.transcript</code>', 'crates/xtask/src/site_demo.rs', '`cargo xtask site-demo [--out <path>] [--bin-dir <dir>] [--port <n>] [--job-port <n>]`: records the demonstration on the landing page of jobscheduler.dev, `site/test/demo.transcript`. It builds the server and the client'],
  ['index', 'A job is recorded before it is placed', 'crates/js-lease-server/src/job_api/submit.rs', 'The call never waits for placement'],
  ['index', 'A client runs <code>js job submit</code>', 'crates/js-cli/src/lib.rs', 'js job submit [--id ID] --spec.image REF'],
  ['index', 'with an image pinned by digest', 'crates/js-lease-server/src/job_api/validate.rs', '`NAME[:TAG]@sha256:<64 hex>`: a tag alone is refused'],
  ['index', 'and a wall limit above 0,', 'crates/js-lease-server/src/job_api/validate.rs', 'present and above 0, and at most the ceiling'],
  ['index', 'and the server records the job as queued before it answers.', 'crates/js-lease-server/src/job_api/submit.rs', 'only after it returns is the answer built'],
  ['index', 'The same id with the same job answers from the job the first submit made.', 'crates/js-cli/src/lib.rs', 'the same id with the same job answers from the job the first submit made'],
  ['index', '<code>js job cancel</code> cancels a job.', 'crates/js-cli/src/lib.rs', 'js job cancel --job-id ID'],
  ['index', 'A job that needs the whole host is placed with the whole host booked,', 'crates/js-sched-placer/src/jobs.rs', 'the job placed, the whole host booked, the mark deleted and the prepare deadline set in one transaction'],
  ['index', 'and starts when its host\'s agent reports the host quiet.', 'crates/js-lease-server/src/job_whole.rs', 'reports `exclusive_ready` when the host is quiet'],
  ['index', 'A host that holds such a job\'s mark takes no other job, except as backfill.', 'crates/js-sched-placer/src/jobs.rs', 'A host that holds such a job\'s mark takes no other job, except as backfill'],
  ['index', 'The <code>js-sched</code> crate admits an action to a worker when measured load plus predicted cost stays within the worker\'s CPU and memory.', 'crates/js-sched/src/lib.rs', 'an action is admitted to a worker when what the worker\'s readings measure, plus the predicted cost of the leases no reading covers yet, plus the cost of the new lease, stays within its CPU and its memory'],
  ['index', 'The server places jobs only under <code>--placement js-sched</code>,', 'crates/js-lease-server/src/lib.rs', 'Jobs are placed only under `js-sched`'],
  ['index', 'which keeps one scheduler for the life of the server', 'crates/js-lease-server/src/lib.rs', 'it keeps one `js-sched` scheduler for the life of the server'],
  ['index', 'and tells it of every hello and every accepted heartbeat with its load reading.', 'crates/js-lease-server/src/lib.rs', 'told of every hello, accepted heartbeat (with its load reading)'],
  ['index', '<td>Job API: submit, get, list, cancel', 'crates/js-cli/src/lib.rs', 'js job submit [--id ID] --spec.image REF'],
  ['index', '<td>Job API: submit, get, list, cancel', 'crates/js-cli/src/lib.rs', 'js job get --job-id ID'],
  ['index', '<td>Job API: submit, get, list, cancel', 'crates/js-cli/src/lib.rs', 'js job cancel --job-id ID'],
  ['index', '<td>Job API: submit, get, list, cancel', 'crates/js-cli/src/lib.rs', 'js job list [--states S ...]'],
  ['index', '<td>Job API: submit, get, list, cancel', 'crates/js-lease-server/src/lib.rs', 'the job service (`jobscheduler.v1.JobService`, crate `js-proto`) is served on a port of its own'],
  ['index', 'cancel; the <code>js</code> client;', 'crates/js-cli/src/lib.rs', '`js`: the command-line client of `js-lease-server`'],
  ['index', '<td>The reservation and drain of a whole node for a queued job</td><td><span class="tag tag-ok">built</span> <span class="tag tag-muted">for a job that needs the whole host, under <code>--placement js-sched</code></span></td>', 'crates/js-sched-placer/src/lib.rs', '[`engine::whole_jobs`]: it queues, marks and commits them from the job tables'],
  ['index', '<td>The reservation and drain of a whole node for a queued job</td><td><span class="tag tag-ok">built</span> <span class="tag tag-muted">for a job that needs the whole host, under <code>--placement js-sched</code></span></td>', 'crates/js-sched-placer/src/jobs.rs', 'Under `--placement js-sched`, with the job tables'],
  ['index', '<td><code>js-sched</code> placing the jobs</td><td><span class="tag tag-ok">built</span> <span class="tag tag-muted">', 'crates/js-lease-server/src/lib.rs', 'Jobs are placed only under `js-sched`'],
  ['index', 'crate</td><td><span class="tag tag-ok">built</span> <span class="tag tag-muted">used by the lease server only under <code>--placement js-sched</code>', 'crates/js-lease-server/src/lib.rs', 'it keeps one `js-sched` scheduler for the life of the server, built once and told of every hello, accepted heartbeat (with its load reading)'],
  ['index', '<td>A token for each client of the job service</td><td><span class="tag tag-ok">built</span> <span class="tag tag-muted">a client of the role <code>client</code> reads and cancels only its own jobs</span></td>', 'crates/js-lease-server/src/lib.rs', '`--client-token`: repeatable, none by default: one more client of the job service'],
  ['index', '<td>A token for each client of the job service</td><td><span class="tag tag-ok">built</span> <span class="tag tag-muted">a client of the role <code>client</code> reads and cancels only its own jobs</span></td>', 'docs/http-gateway.md', 'its own jobs only | every job'],
  ['index', '<td>A token for each tenant of the host and pool routes</td><td><span class="tag tag-planned">not built: one shared token</span></td>', 'crates/js-lease-server/src/lib.rs', 'The server is single-tenant: there is one client token, and so one trust domain'],
  ['index', '<td>An idempotent submit</td><td><span class="tag tag-ok">built</span> <span class="tag tag-muted">the id of the job: the same id with the same job answers from the job the first submit made</span></td>', 'crates/js-cli/src/lib.rs', 'the same id with the same job answers from the job the first submit made'],
  ['concepts', 'A job is recorded, then placed', 'crates/js-lease-server/src/job_api/submit.rs', 'The call never waits for placement'],
  ['concepts', 'A client calls the job service, which the server serves on a port of its own, <code>--job-listen</code>.', 'crates/js-lease-server/src/lib.rs', 'the job service (`jobscheduler.v1.JobService`, crate `js-proto`) is served on a port of its own, off by default'],
  ['concepts', '<code>SubmitJob</code> checks the job, admits it when a host, or a machine provider of the pool, could ever serve it, and records it as queued before it answers;', 'crates/js-lease-server/src/lib.rs', '`SubmitJob` (validated, idempotent on the job id, admitted over the registered hosts and the pool\'s catalog, committed before it answers;'],
  ['concepts', 'it never waits for placement.', 'crates/js-lease-server/src/job_api/submit.rs', 'The call never waits for placement'],
  ['concepts', 'A job that nothing could serve is refused <code>no-capable-host</code>, and nothing is recorded.', 'crates/js-lease-server/src/job_api/submit.rs', 'A job nothing could serve is the refusal `no-capable-host` (`FAILED_PRECONDITION`, 422) and nothing is recorded'],
  ['concepts', 'The job service and host routes need the client\'s bearer token.', 'crates/js-lease-server/src/lib.rs', 'Both are behind one check of the token of a client'],
  ['concepts', 'The job service and host routes need the client\'s bearer token.', 'crates/js-lease-server/src/api.rs', '.route("/v1/hosts", get(hosts)) .fallback(no_route) .layer(DefaultBodyLimit::max(BODY_LIMIT)) .layer(from_fn_with_state(shared.clone(), authenticate))'],
  ['concepts', 'A job has a wall limit, and the server refuses a job without one: a limit that is absent or 0 is refused <code>wall-time-not-positive</code>, and one above the server\'s <code>--max-job-wall</code> is refused <code>wall-time-above-ceiling</code>.', 'crates/js-lease-server/src/job_api/validate.rs', '`spec.wallLimitSeconds` | present and above 0, and at most the ceiling | the refusals `wall-time-not-positive` and `wall-time-above-ceiling`'],
  ['concepts', 'A job has a wall limit, and the server refuses a job without one: a limit that is absent or 0 is refused <code>wall-time-not-positive</code>, and one above the server\'s <code>--max-job-wall</code> is refused <code>wall-time-above-ceiling</code>.', 'crates/js-lease-server/src/lib.rs', 'A job asking for more is refused `wall-time-above-ceiling`'],
  ['concepts', 'A cancel of a queued job ends it at once.', 'crates/js-lease-server/src/job_api/cancel.rs', '| queued | cancelled at once, with a result of reason cancelled'],
  ['concepts', 'A cancel of a job that is placed, starting or running records the intent to cancel, and the job is shown in its state with the intent.', 'crates/js-lease-server/src/job_api/cancel.rs', '| placed, starting or running | the cancel intent (`cancel_requested`), and nothing else | the job, in its state, with the intent |'],
  ['concepts', 'A job that has ended is kept, with its result, for <code>--job-retention</code>, seven days unless the server is told otherwise.', 'crates/js-lease-server/src/lib.rs', 'how long an ended job\'s record and result (exit code, reason, log tails, timing, usage) are kept after the job ends, `7d`'],
  ['concepts', 'A submit that is repeated needs its id.', 'crates/js-cli/src/lib.rs', 'run the same command again with --id <id>'],
  ['concepts', 'The same id with the same job answers from the job the first submit made.', 'crates/js-cli/src/lib.rs', 'the same id with the same job answers from the job the first submit made'],
  ['concepts', 'Without <code>--id</code>, <code>js</code> draws an id and prints it on stderr before it sends.', 'crates/js-cli/src/lib.rs', 'and prints `js: job id <id>` on stderr before it sends'],
  ['concepts', 'When the request was sent and no answer was read, the job may exist:', 'crates/js-cli/src/lib.rs', 'When the request was sent and no answer was read'],
  ['concepts', '<code>js</code> says so and exits 9,', 'crates/js-cli/src/lib.rs', 'the job may exist; run the same command again with --id'],
  ['concepts', '<code>js</code> says so and exits 9,', 'crates/js-cli/src/lib.rs', 'exits 9 (JSON kind `unconfirmed`)'],
  ['concepts', 'and the same command with the same <code>--id</code> is safe to run again.', 'crates/js-cli/src/lib.rs', 'the same command with the same `--id` is safe to run again'],
  ['concepts', 'The server is single-tenant: there is one client token, and so one trust domain.', 'crates/js-lease-server/src/lib.rs', 'The server is single-tenant: there is one client token, and so one trust domain.'],
  ['concepts', 'Whoever holds it can list every host and read every pool, and reset a pool\'s brake.', 'crates/js-lease-server/src/lib.rs', 'Whoever holds it can list every host and read every pool, and reset a pool\'s brake'],
  ['concepts', 'The job service has its own clients and roles.', 'crates/js-lease-server/src/lib.rs', 'The job service has its own clients and roles'],
  ['concepts', 'A client of the role <code>client</code> reads and cancels only its own jobs, and an operator every job.', 'docs/http-gateway.md', 'its own jobs only | every job'],
  ['concepts', 'The token of <code>--token-file</code> is the client <code>default</code>, an operator, and <code>--client-token</code> adds another.', 'crates/js-lease-server/src/lib.rs', 'the client `default` is an operator'],
  ['concepts', 'The token of <code>--token-file</code> is the client <code>default</code>, an operator, and <code>--client-token</code> adds another.', 'crates/js-lease-server/src/lib.rs', '`--client-token`: repeatable, none by default: one more client of the job service'],
  ['concepts', 'A job runs on no host of that file: only the hosts that register take jobs.', 'crates/js-lease-server/src/admit.rs', 'the hosts that register (a job runs on no other host)'],
  ['concepts', 'A job that needs the whole host is a request of its own, not a large one.', 'crates/js-lease-server/src/job_api/validate.rs', '`whole_host` with no other need'],
  ['concepts', 'Under <code>--placement js-sched</code> the engine queues each queued job that needs the whole host as a whole-machine request,', 'crates/js-sched-placer/src/whole_jobs.rs', 'Queues each queued whole-host job the engine does not queue yet, as a whole-machine request'],
  ['concepts', 'and marks the host it is to have.', 'crates/js-sched-placer/src/lib.rs', '[`engine::whole_jobs`]: it queues, marks and commits them from the job tables'],
  ['concepts', 'A job that needs the whole host stays <code>placed</code> while its host\'s agent prepares the host,', 'crates/js-lease-server/src/job_whole.rs', 'while the agent prepares the host for it'],
  ['concepts', 'and moves to starting when the agent reports it ready.', 'crates/js-lease-server/src/job_whole.rs', 'naming the allocation, from an untainted host, moves the job to starting'],
  ['concepts', 'A prepare that is not ready by the pool\'s <code>prepare_timeout</code>, ten minutes unless the pool file sets it, fails,', 'crates/js-lease-server/src/job_whole.rs', 'a placed whole-host job whose allocation\'s prepare deadline has passed fails its prepare'],
  ['concepts', 'A prepare that is not ready by the pool\'s <code>prepare_timeout</code>, ten minutes unless the pool file sets it, fails,', 'crates/js-lease-server/src/job_whole.rs', 'the allocation\'s prepare deadline becomes the store\'s now plus `prepare_timeout`'],
  ['concepts', 'and the job goes back to queued;', 'crates/js-lease-server/src/job_whole.rs', 'The job goes back to queued'],
  ['concepts', 'after <code>max_replace</code> failed prepares it fails to start with <code>exclusive-not-ready</code>.', 'crates/js-lease-server/src/job_whole.rs', 'after `max_replace` of them it fails to start with `exclusive-not-ready`'],
  ['concepts', '<strong>Where it stops.</strong> <code>js-lease-server</code> places jobs only under <code>--placement js-sched</code>;', 'crates/js-lease-server/src/lib.rs', 'Jobs are placed only under `js-sched`'],
  ['concepts', 'the default is <code>registry</code>.', 'crates/js-lease-server/src/lib.rs', '`registry` unless given'],
  ['concepts', 'Under <code>js-sched</code> the server keeps one scheduler for its life, told of every hello, accepted heartbeat (with its load reading) and stop.', 'crates/js-lease-server/src/lib.rs', 'it keeps one `js-sched` scheduler for the life of the server, built once and told of every hello, accepted heartbeat (with its load reading) and stop'],
  ['concepts', 'A pass over the queue on each tick walks the queued jobs in order,', 'crates/js-sched-placer/src/jobs.rs', 'the server runs a **pass** over the queue on each tick'],
  ['concepts', 'one placement round of the scheduler per job, over the hosts that could take a job now.', 'crates/js-sched-placer/src/jobs.rs', 'one placement round of the scheduler per job, over the hosts the registry would let take a job now'],
  ['concepts', 'A host\'s free capacity is what its shape leaves after the job allocations and the registry\'s lease bookings on it, so a job and a lease can never book the same room.', 'crates/js-sched-placer/src/jobs.rs', 'free capacity on a host is what its shape leaves after the job allocations and the registry\'s lease bookings on it, which the engine holds side by side, so a job and a lease can never book the same room'],
  ['concepts', 'A host that goes silent for good and is never read absent keeps its jobs as they are, with their allocations booked.', 'crates/js-lease-server/src/job_loss.rs', 'a host that goes silent for good and is never read absent keeps its jobs as they are and their allocations booked'],
  ['concepts', 'A job becomes lost in two ways: the provider reads its host absent, or the host comes back and does not list the job.', 'crates/js-lease-server/src/job_loss.rs', 'How a job becomes lost (design section 6.3): the two ways, and nothing else'],
  ['install', 'The job service is off unless <code>--job-listen</code> is given, and its routes are not served on <code>--listen</code>.', 'crates/js-lease-server/src/lib.rs', '`--job-listen <addr>` | off unless given: serves the job service on this address'],
  ['install', 'The job service is off unless <code>--job-listen</code> is given, and its routes are not served on <code>--listen</code>.', 'crates/js-lease-server/src/lib.rs', 'The job routes are not served on `--listen`'],
  ['install', 'The token file is the token of the client <code>default</code>, an operator.', 'crates/js-lease-server/src/lib.rs', 'the client `default` is an operator'],
  ['install', '<code>--client-token NAME=PATH</code> adds a client of the role <code>client</code>, which reads and cancels only its own jobs.', 'crates/js-lease-server/src/lib.rs', '`--client-token`: repeatable, none by default: one more client of the job service'],
  ['install', '<code>--client-token NAME=PATH</code> adds a client of the role <code>client</code>, which reads and cancels only its own jobs.', 'docs/http-gateway.md', 'its own jobs only | every job'],
  ['install', 'There is one client token and so one trust domain for the host and pool routes: share it only with parties that trust each other with every host of the pool.', 'crates/js-lease-server/src/lib.rs', 'there is one client token, and so one trust domain'],
  ['install', 'There is one client token and so one trust domain for the host and pool routes: share it only with parties that trust each other with every host of the pool.', 'crates/js-lease-server/src/lib.rs', 'Share the token only with parties that trust each other with every host of the pool'],
  ['install', '<code>submit</code> prints one <code>path: value</code> line for each field of the answer that is set, such as <code>job.state: queued</code>.', 'crates/js-cli/src/lib.rs', 'one `path: value` line per field of the answer that is set (`job.state: queued`'],
  ['install', 'The image must be pinned by digest: a tag alone is refused.', 'crates/js-lease-server/src/job_api/validate.rs', '`NAME[:TAG]@sha256:<64 hex>`: a tag alone is refused'],
  ['install', '<code>wait</code> reads the job every two seconds until it has ended, and exits 0 only when the job succeeded.', 'crates/js-cli/src/lib.rs', '`job wait --job-id ID` reads the job (`GetJob`) every two seconds until it has ended'],
  ['install', '<code>wait</code> reads the job every two seconds until it has ended, and exits 0 only when the job succeeded.', 'crates/js-cli/src/lib.rs', 'for `job wait` and `job run`, the job succeeded'],
  ['install', 'Start the server <span class="tag tag-ok">works today</span>', 'crates/js-lease-server/src/lib.rs', 'The job scheduler\'s server: the hosts that register with it'],
  ['install', 'The first start says <code>--boot initialise</code>, which may create the state file; every later start says', 'crates/js-lease-server/src/lib.rs', 'required: `initialise` may create the state file, `resume` needs it'],
  ['install', 'Submit a job <span class="tag tag-ok">works today</span>', 'crates/js-cli/src/lib.rs', 'js job submit [--id ID] --spec.image REF'],
  ['install', 'With the agent heard by the server, a client submits a job to the job service:', 'crates/js-cli/src/lib.rs', 'It makes the calls of the job service (`jobscheduler.v1.JobService`)'],
  ['kbf', 'the job service exists', 'crates/js-lease-server/src/lib.rs', 'the job service (`jobscheduler.v1.JobService`, crate `js-proto`) is served on a port of its own'],
  ['kbf', 'removed: the job service is the client API', 'crates/js-lease-server/src/lib.rs', 'no route takes or ends a lease'],
  ['kbf', 'an unknown route is a 404, the lease and run routes of earlier releases among them', 'crates/js-lease-server/src/lib.rs', 'An unknown route is a 404 (the lease and run routes of earlier releases among them)'],
];

/**
 * Units of the pages that claim nothing a source could hold: framing, navigation and the steps' own wording. Each entry
 * is a unit exactly as claimUnits gives it; a reworded entry fails until it is registered again or held by a row.
 */
const PROSE: Readonly<Record<string, readonly string[]>> = {
  index: [
    '<a class="button button-primary" href="install.html">Install</a> <a class="button" href="concepts.html">Concepts</a> <a class="button" href="kbf.html">kbf on jobscheduler</a>',
    'The <a href="install.html">install page</a> has the steps, including how to verify the download.',
    'A real run',
    'Submit a job to a server on one machine',
    'The <a href="install.html">install page</a> has the steps for a pool of agents.',
    'What it does',
    'A job, an agent, a placement core',
    '<a href="concepts.html">The concepts page</a> says how each of these works and where it stops.',
    'Status',
    'What is built, and what is not',
  ],
  concepts: [
    'Concepts',
    'Jobs, whole-host jobs and placement by CPU and memory, as the code has them.',
    'What a job is, how hosts get into a pool, what a whole-host job promises, and how the placement core decides.',
    'Each section says where the code or the document that backs it is, and marks what is not built;',
    '<a href="index.html#status">the status table</a> on the home page lists it in one place.',
    'Hosts and pools',
    'Hosts reach a pool in these ways:',
    'A whole node: exclusive, quiet, reserved, capped',
    'Placement by CPU and memory',
    'A silent host',
  ],
  install: [
    'The paths below are examples.',
    'Install',
    'Get the server, the client and the agent from a release, and what the agent needs on a host.',
    'What is not shipped',
  ],
  kbf: [
    'kbf on jobscheduler',
    'What kbf takes from this repository',
    'What kbf runs today',
    'The design for kbf on the agent',
    'Check kbf\'s tree before you rely on a statement about it.',
    'The pieces a scheduler server is built from are here:',
    'It proposes the following, and the right-hand column is the state of each part in this repository.',
  ],
};

/** What a source says: comment markers (`//!`, `///`, `//`, `#`) dropped from the start of each line, whitespace collapsed. */
export function flat(text: string): string {
  return text
    .split('\n')
    .map((l) => l.replace(/^\s*(?:\/\/[!/]?|#)\s?/, ''))
    .join(' ')
    .replace(/\s+/g, ' ');
}

/** What a page says: its copy (content/<name>.html), and its title and description, which are in tools/pages.ts. */
const page = (name: string): string => {
  const p = PAGES.find((x) => x.source === name);
  return `${read(SITE, 'content', `${name}.html`)}\n<title>${p?.title ?? ''}</title>\n<description>${p?.description ?? ''}</description>`;
};
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

/**
 * The units of a page that can carry a claim: each heading, each sentence of a paragraph or list item, each row of a
 * table (its cells with their <td> markup, so one phrase can hold a row: the thing and its state), and the page's title and description. Left out: code blocks (<pre>, held by the
 * tests that name them), the copy buttons' commands, the source notes (they are checked to name real files) and the
 * table of contents (links to headings, checked by the link test). Markup stays in, because the rows quote it.
 */
export function claimUnits(html: string): string[] {
  const text = html
    .replace(/<!--[\s\S]*?-->/g, '')
    .replace(/<pre\b[\s\S]*?<\/pre>/g, '')
    .replace(/<p class="(?:source-note|toc)">[\s\S]*?<\/p>/g, '')
    .replace(/<thead>[\s\S]*?<\/thead>/g, '');
  const norm = (s: string): string => s.replace(/\s+/g, ' ').trim();
  const units: string[] = [];
  for (const m of text.matchAll(/<tr>([\s\S]*?)<\/tr>/g)) units.push(norm(m[1]));
  const rest = text.replace(/<table\b[\s\S]*?<\/table>/g, '');
  for (const m of rest.matchAll(/<(title|description)>([\s\S]*?)<\/\1>/g)) if (norm(m[2]) !== '') units.push(norm(m[2]));
  // Innermost blocks only: a block whose inside opens another block is skipped where it starts, and its children are
  // then found as the scan goes on inside it.
  for (const m of rest.matchAll(/<(h[1-6]|p|li)\b[^>]*>((?:(?!<(?:p|li|ul|ol|div|h[1-6]|table)\b)[\s\S])*?)<\/\1>/g)) {
    const inner = m[2];
    const sentences = /^h/.test(m[1]) ? [inner] : inner.split(/(?<=[.;:!?])\s+(?=[A-Z<])/);
    for (const s of sentences) if (norm(s) !== '') units.push(norm(s));
  }
  return units;
}

/**
 * The words of `unit` that no span covers: each run of letters and digits outside markup (a tag's name and attributes,
 * or a character reference) of which at least one character lies outside every span. A held unit must have none, so
 * no word of a claim (a "not", a number, a code name) can change while a row's phrase beside it stays.
 */
export function unanchored(unit: string, spans: readonly (readonly [number, number])[]): string[] {
  const markup = new Array<boolean>(unit.length).fill(false);
  for (const m of unit.matchAll(/<[^>]*>|&#?[A-Za-z0-9]+;/g)) for (let i = m.index; i < m.index + m[0].length; i++) markup[i] = true;
  const words: string[] = [];
  for (const m of unit.matchAll(/[\p{L}\p{N}]+/gu)) {
    const chars = [...Array(m[0].length).keys()].map((k) => m.index + k).filter((i) => !markup[i]);
    if (chars.some((i) => !spans.some(([s, e]) => s <= i && i < e))) words.push(m[0]);
  }
  return words;
}

/** Every place `needle` starts in `hay`. */
function positions(hay: string, needle: string): number[] {
  const at: number[] = [];
  for (let i = hay.indexOf(needle); i >= 0; i = hay.indexOf(needle, i + 1)) at.push(i);
  return at;
}

/**
 * What the claims table leaves unchecked on the pages, as problems naming the page and the text:
 * - a row whose page phrase or source phrase is empty or too short to anchor anything, or names a page that is not one;
 * - a row whose page phrase occurs more than once on its page (an occurrence no row pins can drift unnoticed);
 * - a unit of a page that no row holds and that is not registered in `prose` as framing that claims nothing;
 * - a <code> span or a number in a held unit that lies outside every row phrase in it (a qualifier or a number that can
 *   change while the row's phrase stays);
 * - an entry of `prose` that is no longer a unit of its page, or that a row holds (it is a claim, not framing).
 */
export function uncovered(rows: readonly Row[], html: (name: string) => string, prose: Readonly<Record<string, readonly string[]>>, pages: readonly string[]): string[] {
  const out: string[] = [];
  for (const [name, shown, file, says] of rows) {
    if (!pages.includes(name)) out.push(`a row names a page that is not one: ${name}`);
    if (shown.trim().length < 3) out.push(`content/${name}.html: a row has an empty page phrase ("${shown}", ${file})`);
    if (says.trim().length < 3) out.push(`${file}: a row has an empty source phrase ("${says}", for content/${name}.html)`);
  }
  for (const name of pages) {
    const text = html(name);
    const phrases = [...new Set(rows.filter((r) => r[0] === name && r[1].trim().length >= 3).map((r) => r[1]))];
    for (const shown of phrases) {
      const n = positions(text, shown).length;
      if (n > 1) out.push(`content/${name}.html says "${shown}" ${n} times: lengthen the row's phrase so that it pins one`);
    }
    const units = claimUnits(text);
    const registered = prose[name] ?? [];
    for (const unit of units) {
      const held = phrases.filter((p) => unit.includes(p));
      if (held.length === 0) {
        if (!registered.includes(unit)) out.push(`content/${name}.html: no row holds "${unit}" (add a row, or register it as prose if it claims nothing)`);
        continue;
      }
      const spans = held.flatMap((p) => positions(unit, p).map((i) => [i, i + p.length] as const));
      for (const word of unanchored(unit, spans)) out.push(`content/${name}.html: ${word} in "${unit}" is outside every row phrase`);
    }
    for (const entry of registered) {
      if (!units.includes(entry)) out.push(`content/${name}.html: the prose entry "${entry}" is no longer a unit of the page`);
      else if (phrases.some((p) => entry.includes(p))) out.push(`content/${name}.html: the prose entry "${entry}" is held by a row, so it is a claim`);
    }
  }
  return out;
}

const CLAIM_PAGES = PAGES.filter((x) => x.source !== '404').map((x) => x.source);

test('every claim of every page is held by a row, whole: its numbers and code included, each occurrence pinned', () => {
  assert.deepEqual(uncovered(ROWS, page, PROSE, CLAIM_PAGES), []);
});

test('an omitted claim, a changed qualifier, a repeated phrase and an empty row are each reported (the check can fail)', () => {
  const report = (html: (n: string) => string, rows: readonly Row[] = ROWS): string => uncovered(rows, html, PROSE, CLAIM_PAGES).join('\n');
  // A new sentence no row holds.
  const added = (n: string): string => (n === 'concepts' ? page(n).replace('Capacity comes in pools.', 'Capacity comes in pools. Pools are free.') : page(n));
  assert.match(report(added), /content\/concepts\.html: no row holds "Pools are free\."/);
  // A permission reversed beside a row's phrase: the row's phrase still matches, the word check does not.
  const reversed = (n: string): string => (n === 'concepts' ? page(n).replace('Whoever holds it can list every host', 'Whoever holds it cannot list every host') : page(n));
  assert.match(report(reversed), /content\/concepts\.html: (?:cannot|no row holds) /);
  // A number beside a row's phrase changes: the row would still match, the coverage check does not.
  const numbered = (n: string): string => (n === 'index' ? page(n).replace('for the life of the server and tells it', 'for the life of the server, 3 in all, and tells it') : page(n));
  assert.match(report(numbered), /content\/index\.html: 3 in "[^"]*3 in all[^"]*" is outside every row phrase/);
  // A phrase a row pins, said a second time.
  const shown = ROWS.find((r) => r[0] === 'install')![1];
  const twice = (n: string): string => (n === 'install' ? page(n).replace('</h1>', `</h1>\n<p>${shown}</p>`) : page(n));
  assert.match(report(twice), /content\/install\.html says ".*" 2 times/);
  // Rows that anchor nothing, and a row for a page that is not one.
  assert.match(report(page, [...ROWS, ['kbf', '', 'README.md', 'A generic job and compute scheduler']]), /content\/kbf\.html: a row has an empty page phrase/);
  assert.match(report(page, [...ROWS, ['kbf', 'kbf is the first user of jobscheduler', 'README.md', ' ']]), /README\.md: a row has an empty source phrase/);
  assert.match(report(page, [...ROWS, ['nope', 'x y z', 'README.md', 'A generic job']]), /a row names a page that is not one: nope/);
});

test('a prose entry that is gone from its page, or that a row holds, is reported (the check can fail)', () => {
  const name = Object.keys(PROSE).find((n) => (PROSE[n] ?? []).length > 0)!;
  const entry = PROSE[name]![0]!;
  const gone = (n: string): string => (n === name ? page(n).split(entry).join('') : page(n));
  assert.match(uncovered(ROWS, gone, PROSE, CLAIM_PAGES).join('\n'), /is no longer a unit of the page/);
  const held = uncovered([...ROWS, [name, entry, 'README.md', 'A generic job and compute scheduler']], page, PROSE, CLAIM_PAGES).join('\n');
  assert.match(held, /is held by a row, so it is a claim/);
});

test('unanchored names each word with a character outside every span, and skips markup and character references', () => {
  const unit = 'a <code>x</code> may not &amp; be';
  assert.deepEqual(unanchored(unit, [[0, unit.length]]), []);
  assert.deepEqual(unanchored(unit, []), ['a', 'x', 'may', 'not', 'be']);
  assert.deepEqual(unanchored(unit, [[0, 22]]), ['not', 'be']); // 'may no' covered: a word cut short is not anchored
  assert.deepEqual(unanchored('<span class="tag">ok</span>', [[18, 20]]), []);
});

test('claimUnits splits sentences, keeps a table row whole and leaves out code blocks and source notes', () => {
  const html = '<h2 id="a">Head</h2><p>One. Two <code>x</code>; three.</p><pre>$ ignored</pre><p class="source-note">Sources: <code>a.rs</code>.</p><table><thead><tr><th>H</th></tr></thead><tbody><tr><td>A</td><td><span>b</span></td></tr></tbody></table><ul><li><p>Inner.</p></li></ul>';
  assert.deepEqual(claimUnits(html), ['<td>A</td><td><span>b</span></td>', 'Head', 'One.', 'Two <code>x</code>; three.', 'Inner.']);
  // A paragraph inside a list item is a unit (the list item that holds it is not), and so is a list item's own text.
  assert.deepEqual(claimUnits('<ol><li><h3>Step</h3><p>Do it. Then this.</p></li><li>Plain.</li></ol>'), ['Step', 'Do it.', 'Then this.', 'Plain.']);
});

test('the claims table reports its size: rows, and rows for each page', (ctx) => {
  const per = CLAIM_PAGES.map((n) => `${n} ${ROWS.filter((r) => r[0] === n).length}`).join(', ');
  ctx.diagnostic(`claims rows: ${ROWS.length} (${per}); prose entries: ${Object.values(PROSE).flat().length}`);
  assert.equal(ROWS.length, CLAIM_PAGES.reduce((a, n) => a + ROWS.filter((r) => r[0] === n).length, 0), 'every row names a page with claims');
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
  assert.match(unheld(ROWS, real, rewordedPage).join('\n'), /content\/kbf\.html no longer says: .*marked "design only"/);
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
  assert.match(archiveGaps(added, page('install')).join('\n'), /docs\/release\.md lists 16/);
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

/** What the source notes of `pages` get wrong, as problems naming the page and the file: a page with no source note, or a note naming a file that `exists` does not find. Also returns the files it checked. */
export function sourceNoteGaps(pages: readonly string[], html: (name: string) => string, exists: (file: string) => boolean): { problems: string[]; checked: string[] } {
  const problems: string[] = [];
  const checked: string[] = [];
  for (const name of pages) {
    const notes = [...html(name).matchAll(/<p class="source-note">([\s\S]*?)<\/p>/g)].map((m) => m[1]);
    if (notes.length === 0) problems.push(`${name} has no source note`);
    for (const note of notes) {
      for (const m of note.matchAll(/<code>([^<]+)<\/code>/g)) {
        const token = m[1];
        if (!/\/|\.(?:md|rs|toml|ya?ml|service|conf)$/.test(token)) continue;
        if (!exists(token)) problems.push(`${name}: the source note names ${token}, which is not in the repository`);
        checked.push(token);
      }
    }
  }
  return { problems, checked };
}

const inRepo = (file: string): boolean => existsSync(join(REPO, file));

test('every page but the 404 has a source note, and every file a source note names exists', () => {
  const { problems, checked } = sourceNoteGaps(CLAIM_PAGES, page, inRepo);
  assert.deepEqual(problems, []);
  assert.ok(checked.length >= 20, `${checked.length} files checked`);
});

test('a source note naming a missing file, and a page with no source note, are reported with the page (the check can fail)', () => {
  const missing = (n: string): string => (n === 'concepts' ? page(n).replace('<code>crates/js-cli/src/lib.rs</code> (the commands)', '<code>crates/js-cli/src/nope.rs</code> (the commands)') : page(n));
  assert.deepEqual(sourceNoteGaps(CLAIM_PAGES, missing, inRepo).problems, ['concepts: the source note names crates/js-cli/src/nope.rs, which is not in the repository']);
  const noNote = (n: string): string => (n === 'kbf' ? page(n).replace(/<p class="source-note">[\s\S]*?<\/p>/g, '') : page(n));
  assert.deepEqual(sourceNoteGaps(CLAIM_PAGES, noNote, inRepo).problems, ['kbf has no source note']);
});

/** The repository files the claims of this test read: the rows' sources, the files read by name above, and the files the pages' source notes name. */
function claimInputs(): string[] {
  const own = read(SITE, 'test', 'claims.test.ts');
  const named = [...own.matchAll(/\bsource\('([^']+)'\)/g)].map((m) => m[1]);
  const notes = PAGES.filter((x) => x.source !== '404').flatMap((p) =>
    [...page(p.source).matchAll(/<p class="source-note">([\s\S]*?)<\/p>/g)].flatMap((n) =>
      [...n[1].matchAll(/<code>([^<]+)<\/code>/g)].map((m) => m[1]).filter((t) => /\/|\.(?:md|rs|toml|ya?ml|service|conf|sh)$/.test(t) && existsSync(join(REPO, t)))));
  return [...new Set([...ROWS.map((r) => r[2]), ...named, ...notes])].sort();
}

/** The inputs of `files` that no entry of site/ci-paths matches, as problems; and the way ci.yml's site step fails to read that file. */
export function unwatched(files: readonly string[], entries: readonly string[], yml: string): string[] {
  const out: string[] = [];
  if (entries.length === 0) out.push('site/ci-paths: no entries found');
  if (!siteRuns(yml).some((r) => r.includes('--paths site/ci-paths '))) out.push('ci.yml: the site step does not read site/ci-paths');
  if (!siteSteps(yml).includes("if: ${{ steps.site.outputs.run == 'true' }}")) out.push('ci.yml: the site steps do not wait for the decision');
  for (const f of files) if (!entries.some((p) => pathMatches(p, f))) out.push(`a change to ${f} does not run the site steps of ci.yml, and a claim reads it`);
  return out;
}

test('a change to any file a claim reads runs the site steps of ci.yml, on pull requests and on pushes to main', () => {
  const inputs = claimInputs();
  assert.ok(inputs.length >= 20, `${inputs.length} inputs`);
  assert.deepEqual(unwatched(inputs, ciPaths(), ciWorkflow()), []);
});

test('a claim input outside site/ci-paths, an empty list and a step that does not read the list are reported (the check can fail)', () => {
  const yml = ciWorkflow();
  const entries = ciPaths();
  assert.deepEqual(unwatched(['crates/nope/src/lib.rs'], entries, yml), ['a change to crates/nope/src/lib.rs does not run the site steps of ci.yml, and a claim reads it']);
  assert.deepEqual(unwatched(['docs/release.md'], entries.filter((e) => e !== 'docs/'), yml), ['a change to docs/release.md does not run the site steps of ci.yml, and a claim reads it']);
  assert.deepEqual(unwatched([], [], yml), ['site/ci-paths: no entries found']);
  assert.deepEqual(unwatched([], entries, yml.replaceAll('site/ci-paths', 'site/other')), ['ci.yml: the site step does not read site/ci-paths']);
  assert.deepEqual(unwatched([], entries, yml.replaceAll("steps.site.outputs.run == 'true'", 'true')), ['ci.yml: the site steps do not wait for the decision']);
  assert.equal(pathMatches('crates/js-agent/src/', 'crates/js-agent/src/a/b.rs'), true);
  assert.equal(pathMatches('docs/', 'docs-old/x.md'), false);
  assert.equal(pathMatches('README.md', 'README.mdx'), false);
  assert.equal(pathMatches('README.md', 'README.md'), true);
});
