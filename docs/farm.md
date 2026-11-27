# The build farm lane: the workspace with Buck2 on the build farm

Cargo is jobscheduler's build of record. CI's gate (`ci.yml`), the coverage ratchet and every
release use it, on GitHub-hosted runners, and that does not change. The build farm lane builds the
same workspace a second way: with Buck2, through [buildrules](https://github.com/komira-ai/buildrules)'
Rust rules, every action on a remote-execution build farm or taken from its cache. ("The build
farm" here is that remote-execution service, not the hosts js-agent runs on.)

The lane is additive and advisory. `.github/workflows/farm.yml` runs it on every push to `main` and
on every pull request from a branch of this repository (not a fork's, not Dependabot's). No check
of it is required, and a build farm outage turns it red and holds nothing.

The same farm also builds the macOS files of a release on its Macs, from a Linux runner, when the
repository variable `MACOS_ON_FARM` is `true`: that is `macos-farm.yml`, with Cargo and not Buck2
([mac-farm.md](mac-farm.md)).

## What it builds

`buck2 build //:ci` runs three steps for each member of the workspace:

- `:fmt`: the pinned `rustfmt --check` over the files `cargo fmt` reads, with `rustfmt.toml`.
- `:lint`: clippy over the library, the binaries and every test target, with `[workspace.lints]`
  and `clippy.toml`, warnings as errors.
- `:test`: each test target run as a build action. Its `.passed` marker exists only if every test of
  the target passed, and a cached marker means the tests passed on byte-identical inputs.

The members listed below leave out some of these steps. The summary line must read
`Commands: N (cached: N, remote: N, local: 0)`: no action ran on the runner.

`farm.yml` runs it in four steps after it joins the build farm's network:

1. `cargo xtask farm-workers` refuses to go on without the build farm's worker property set
   (`crates/xtask/src/farm.rs`).
2. komira's buck2-farm action builds the `buildrules` command line on the build farm
   (`buck2 build buildrules//cli:buildrules --out <file>`).
3. `cargo xtask farm-table` checks the generated table with it, and refuses a stale one and hands
   over its fix (below).
4. komira's buck2-farm action builds `//:ci`.

Both buck2 commands that reach the build farm run through the buck2-farm action, under its watchdog
(below, "A dead connection to the build farm"). Its second step fails a run that does not say
`local: 0`. A step of its own after the builds, under `if: always()`, stops buck2's daemon however
the steps before it ended.

The files of the build, all generated or written once by `buildrules`:

| file | what | written by |
|---|---|---|
| `.buckconfig` | the cells, buildrules pinned by commit, the execution platform | `buildrules init`, then by hand (the worker passthrough below) |
| `toolchains/`, `platforms/` | the pinned Rust toolchain and the remote execution platform | `buildrules init` |
| `BUCK`, `crates/*/BUCK` | `cargo_workspace` and one `cargo_package` per member | `buildrules init` and `buildrules cargo adopt`, once; then by hand (`extra_srcs`, below) |
| `buildrules.lock.bzl`, `buildrules.bzl`, `third_party/buildrules/` | Cargo's unit graph of every member; every crates.io crate as a unit pinned by SHA-256 and size; the git dependency (hostkeeper's crates) as GitHub's archive of its pinned commit, by SHA-256 and size (`git_archives.tsv`) | `buildrules sync`; never edited by hand |

`Cargo.toml`, `Cargo.lock` and `rust-toolchain.toml` stay the source. `//:check` fails every
target when the generated table no longer matches them.

## A stale table: the fix is a patch of the run

A pull request that changes a manifest or the lock and not the table makes the table stale. You
do not need `buildrules` to fix it: the job makes the fix and hands it to you
(`crates/xtask/src/farm/table.rs`).

1. Before the build, the job builds the `buildrules` command line from the pinned buildrules cell
   on the build farm (`buildrules//cli:buildrules`) and runs `buildrules check` with it.
2. When check finds the table stale or a generated file missing, the job runs that
   `buildrules sync` on the runner, checks the table again, and writes what sync changed as a patch
   (`git diff --binary` of the paths sync writes, new files included). It
   fails without building. Its error and the job's summary name the inputs that changed, say
   that the table is generated from them, list the files the patch changes, and give the one
   command that applies it; the patch is the run's artifact `buildrules-sync`.
3. You read the patch and apply it from a checkout of the branch, then commit and push:

   ```sh
   gh run download <run id> -R komira-ai/jobscheduler -n buildrules-sync -D buck-out/buildrules-sync-<run id> && git apply buck-out/buildrules-sync-<run id>/buildrules-sync.patch
   ```

   The summary prints it with the run's id filled in. `buck-out/` is ignored by git, so the
   download leaves nothing to commit. On a pull request the job's checkout is the merge of the
   branch into main, so the patch is made against that merge. If `git apply` refuses it, main has
   changed the table too: merge main into the branch and push, and the next run carries a new
   patch.

A problem sync does not fix (a member with no `cargo_package` in its BUCK file, a floating
`rust-toolchain.toml`) is still there when the job checks again after sync: the job fails with what
check said, and hands over no patch.

The job does not push the regenerated files to the branch itself. A job that pushes needs a token
that can write to the repository, in the one job of this repository that already holds an OIDC
token and runs a pull request's code; a push made with the job's own token starts no workflow, so
the branch's new head would carry no CI run until someone pushed again; and a generated change of
a few thousand lines would land on the branch unread. A fork's pull request does not run this job
at all. A patch the contributor applies keeps the job read-only and the change reviewed.

A Buck2 action sees only its declared inputs. A library unit sees `src/` and `build.rs`, a test
target `tests/`, and a build script its whole package. A file a unit reads beyond that is declared
in the member's BUCK file (`extra_srcs`), with the reason in a comment there. Examples: a library
that includes the goldens of `tests/`, a test that includes `src/` or another member's fixtures,
an example that takes a module from `tests/`. A file of another member, or of the repository root,
is exported by reference in its owner's BUCK file, so it stays at its path.

## What stays in ci.yml, and why

A farm action has its declared inputs and nothing else: no network, no `PATH` but buildrules'
busybox directory, no git checkout, not root, no database server, and a
`TMPDIR` about 98 bytes deep, too deep for a Unix socket's path below it
(komira-ai/buildrules#170). buildrules' test gate also refuses a test target that has an
`#[ignore]`d test, because an ignored test is a test that never runs. Each member below says why
in its `[package.metadata.buildrules]`, and its tests (and, for the first group, its build and
clippy) run in ci.yml alone.

**Format only on the build farm** (`ci = ["fmt"]`): `js-contain`, and every member that depends on
it directly or through a dev-dependency: `js-agent`, `js-cli`, `js-client`, `js-conformance`,
`js-lease-server`, `js-measure`, `js-sim-cell` and `xtask`.

`js-contain`'s build script compiles `js-init` (`init/init.rs`), the first process of every job's
container, as a static executable, so that it starts in an image with no C library. buildrules
links Rust through `zig cc` for a glibc target. That linker cannot link glibc statically, so it
links dynamically and still succeeds (komira-ai/buildrules#173). `build.rs` reads the binary back
and refuses one that names a program interpreter (`init/static_elf.rs`), so on the build farm
`js-contain` does not build, and nothing that depends on it does. Under cargo on the hosted runners
the binary is static and the check passes.

**Built and linted, tests in ci.yml** (`ci = ["fmt", "lint"]`):

| member | why its tests stay hosted |
|---|---|
| `js-bundle` | `tests/pack.rs` binds a Unix socket below `TMPDIR` (#170) |
| `js-guest` | its serve tests bind a Unix socket below `TMPDIR` (#170); `tests/as_root.rs` holds `#[ignore]`d root tests |
| `js-vmm` | the lease-system tests bind a Unix socket below `TMPDIR` (#170) |
| `js-isolation-probe` | its network tests bind a Unix socket below `TMPDIR` (#170) |
| `js-caps` | `tests/accel.rs` scans the sources of every member |
| `js-watch` | `tests/read_only.rs` reads other members' sources and walks `crates/` |
| `js-elastic-fake` | `tests/no_cloud_reach.rs` reads every manifest and source file of the repository |
| `js-mac-probe` | its child tests expect an absolute `PATH` |
| `js-mac-session` | its tests read `/etc/passwd` and need directories without other mounts; `tests/as_root.rs` holds `#[ignore]`d root tests |
| `js-testkit` | its fake's tests read `PATH`; its dir, database and reaped targets hold `#[ignore]`d child tests |
| `js-supervise` | its tests start stand-in supervisors (`sh`, `cat`, `true`) and expect an absolute `PATH` |
| `js-state` | its tests need a Postgres server (`KBF_TEST_POSTGRES_URL`); `postgres_write_rate.rs` holds an `#[ignore]`d test |
| `js-sched` | five test targets hold `#[ignore]`d tests (golden regeneration, seed replay, long runs) |
| `js-sim` | its determinism and toy_counter targets hold `#[ignore]`d golden regeneration |
| `js-logstore` | `tests/logstore` holds `#[ignore]`d tests that need an S3-compatible store |
| `js-mac-pool` | its work_root and trace_base_set targets hold `#[ignore]`d tests |
| `js-mac-provision` | `tests/pf_as_root.rs` holds an `#[ignore]`d root test |
| `js-mdm-core` | `tests/installed_host` holds an `#[ignore]`d test |
| `js-treestore` | its privileged target holds `#[ignore]`d tests (they need root) |
| `js-provider-lambdalabs` | `tests/http` holds an `#[ignore]`d child test |

Some other checks only ci.yml makes, for every member:

- Coverage: the ratchet of `coverage-baseline` is cargo-llvm-cov's; buildrules has no Rust coverage
  yet.
- Doctests: buildrules does not run them.
- The `postgres` features: buildrules builds each member's tests with that member's own features,
  as `cargo test -p` does, so a test target behind the `postgres` feature (`js-jobs`, `js-usage`,
  `js-lease-server`) compiles to no test and passes, as it does under plain `cargo test`. ci.yml's
  `state-postgres` job runs them. buildrules can start a private Postgres for a member's tests
  (`postgres = { url-env = "NAME" }`), and no member declares one: the one member whose default
  tests need Postgres, `js-state`, also holds an `#[ignore]`d test.
- The privileged, macOS and slow-family jobs.

## Running it yourself

You need buck2 (the release in buildrules' `tools/buck2`) and a `.buckconfig.local` (gitignored)
that names the build farm. It holds `[buck2_re_client]` with the endpoints and instance, and
`[buildrules_re] linux_x86_64_properties` with the worker property set. This repository holds
neither. Then:

```sh
buck2 build //crates/js-sched-placer:test    # one member's tests
buck2 build //:ci                            # what farm.yml builds
buck2 log what-failed                        # the failing actions and their commands
```

Read the summary line: `local` must be 0.

## How the job reaches the build farm

The build farm is on a private network. komira's farm-connect action, pinned by commit, joins it
with the job's OIDC token (workload identity federation: no secret is stored). As that action
documents, it checks that the build farm answers, masks its address in the log, and writes the
build farm's buckconfig under `~/.buckconfig.d`, where buck2 reads it.

Its inputs are the organisation's variables `TS_CLIENT_ID` and `TS_AUDIENCE` (the tailnet
identity), `FARM_ADDRESS`, `FARM_INSTANCE`, `FARM_LINUX_X86_64_PROPERTIES` and `FARM_TLS`.
farm-connect writes the worker property set under `[komira_re]`. `.buckconfig` hands it to
buildrules (`[buildrules_re] linux_x86_64_properties =
$(config komira_re.linux_x86_64_properties)`) and holds no value of its own.

That join is why the job holds `id-token: write` on a pull request. The workflow lint
(`crates/xtask/tests/apt_workflow/farm.rs`) holds it to:

- exactly that scope and `contents: read`, under a top-level `permissions: {}`;
- the exact `if:` that skips a fork's pull request (which gets no OIDC token) and Dependabot's;
- farm-connect's inputs exactly the variables above, farm-connect pinned by a full commit, and
  exactly one step that uses it;
- the triggers, the one job and its runner;
- across every workflow, no other job a pull request starts that holds `id-token: write`.

`crates/xtask/tests/apt_workflow/farm_steps.rs` holds the steps that reach the build farm and the
time limits to:

- the job's `KOMIRA_COMMIT`, which names the one komira commit of farm-connect, buck2-farm and
  the source of buck2-farm's tool (the commit of komira main that has the watchdog,
  komira-ai/komira#2439), and every komira action of the job pinned to it;
- the tool fetched from GitHub's archive of that commit and built before the first buck2-farm step;
- the worker check, the build of the `buildrules` command line, the table's check and the build of
  `//:ci`, in that order after farm-connect, the two builds the only buck2-farm steps, each with
  exactly its inputs; then the stop of buck2's daemon under `if: ${{ always() }}`;
- across every workflow, no `run:` that runs buck2 but that stop, which reaches no farm: a buck2
  command anywhere else would have no watchdog;
- every step of the job with a `timeout-minutes` of its own, a whole number from 1, under the
  job's 150.

## A dead connection to the build farm

When the network path to the build farm drops without closing the connection, buck2 waits for an
answer the farm will never send, and before this a step waited until its job's time limit. buck2
gives an action that waits on the farm no limit of its own, and its log prints the same `Waiting
on ...` line every few seconds whether the action runs or is lost.

So both buck2 commands of the lane run through komira's buck2-farm action
(`komira-ai/komira/.github/actions/buck2-farm`, at `KOMIRA_COMMIT`). It runs buck2 under
`assert_remote run`, built in the job from komira's source at the same commit (GitHub's archive of
it, pinned by SHA-256; zig 0.15.2, the pin `ci.yml` uses). While buck2 runs, the tool reads the
runner's TCP connections to the farm's address from the kernel once a second. A live farm sends
something on each connection at least every 30 s. When one connection has heard nothing for longer
than 5 minutes, the tool:

1. ends buck2 (SIGINT, then SIGKILL if it does not end) and stops its daemon (`buck2 kill`), which
   holds the dead connection;
2. writes one line to the job log, and one to the job summary;
3. waits for the farm's port to answer (at most farm-connect's 20 minutes) and runs the same
   command again.

A run that fails because its connection was cut is run again the same way. A step runs at most
three times in all, a hung run included; when the third run hangs or is cut too, or the farm does
not answer within the wait, the step fails. A slow build whose connection is alive is never ended:
the watchdog reads the connection, never the log, and never limits how long one action runs.

What you see in the job log when it ends a run (from komira's own runs of the action, the port
differs):

    buck2-farm: watchdog: the connection to the farm from local port 41022 has heard nothing from the farm for 5 min 1 s, past the bound of 5 min 0 s: the farm has closed it and buck2 would wait on it for ever; ending buck2 (SIGINT)
    buck2-farm: stopping buck2's daemon, which holds the dead connection (`./buck2 kill`)

and in the job summary, one line that names the step, the silence, the wait for the farm and the
run it is on (`ran again (run 2 of at most 3)`). The failed run's log is kept beside the step's as
`buck2-farm.first.log` (the second's as `buck2-farm.second.log`) in the runner's temporary
directory. A green step after a hang or a cut is a real green: the run that passed had every action
on the build farm or from its cache. In this job the line names buck2 by its pinned path, not
`./buck2`.

What the watchdog does not cover: every step that is not one of the two buck2 commands. The
installs, farm-connect (which has its own bounded wait for the farm), `cargo xtask farm-workers`,
`cargo xtask farm-table` and the daemon's stop run without it. Among them only `farm-table` can
reach the farm: on a stale table, `buildrules sync` builds the pinned toolchain with buck2 itself.
Each of them is bounded by its step's own time limit (below), so none can hang to the job's.
`macos-farm.yml`, which reaches the farm's Macs with `xtask mac-farm` and no buck2, is not under
the watchdog either; its build steps have time limits of their own of 60 minutes.

## Time limits

Every step of the job has a `timeout-minutes` of its own, under the job's 150. The sizes come from
the longest successful run of each step in the 200 most recent green runs of the job (query:
`gh run list -R komira-ai/jobscheduler -w farm.yml -s success -L 200`, then `gh run view --json
jobs` of each; the runs from 2026-11-26 20:02 to 2026-11-27 13:44 UTC; measured). The steps new
here have no runs yet: their sizes are from the same steps of `ci.yml`'s `lint` job (40 green
runs, the same query on `ci.yml`), or inferred.

| step | longest green run | limit |
|---|---|---|
| checkout, `rustup toolchain install`, each cache restore and save | 18 s | 10 |
| Install buck2 (the job's first `cargo xtask`, so it builds xtask) | 105 s | 30 |
| Install zig | 88 s in `ci.yml`, where it builds xtask; here xtask is built already | 10 |
| Install assert_remote's source, Build assert_remote | none yet (a 34 MB archive; a build of a few thousand lines of zig, inferred under a minute) | 10 |
| Connect to the build farm (farm-connect, whose own wait for the farm is at most 20) | 85 s | 30 |
| `buildrules has the farm's worker property set` | none yet (one `buck2 audit config`) | 10 |
| the build of the `buildrules` command line (buck2-farm) | none yet; it is cached on the farm but after a buildrules move | 30 |
| `the generated build table is fresh` (a sync on a stale table builds a toolchain) | none yet | 30 |
| `buck2 build //:ci` (buck2-farm) | 1891 s (31.5 min) as part of the former one build step, which also built the command line | 120 |
| Stop buck2's daemon | none yet | 5 |
| Upload the regenerated build table as a patch | none (runs only on a failure) | 10 |

30 for a buck2-farm step leaves room for one hang ended (5 minutes), the wait for the farm and a run
again. 120 for `//:ci` is three runs of the longest green one (95 minutes) and two hangs ended (10
minutes), with 15 minutes of the waits for the farm; a step that needs more than that has met a farm
that is down, and fails.

A pull request's build and a build of `main` share the build farm's action cache, as every build of
every repository on the build farm does: a result one writes, the other reads. The cache is keyed
by the action's digest, so a result is reused only for byte-identical inputs and command.

## Measured

The first measurements of the lane: `buck2 build //:ci` of this branch (buildrules at a693e5c),
run by hand from a development machine against the build farm. The machine ran no action. Times
are the run's start and end, or the shell's.

| build | `Commands:` line | wall |
|---|---|---|
| the first, cold for this workspace's own units (it failed on undeclared inputs) | `3131 (cached: 28, remote: 3103, local: 0)` | 13 min |
| after the inputs were declared (one failure left) | `472 (cached: 0, remote: 472, local: 0)` | 7 min |
| the last fix, **BUILD SUCCEEDED** | `9 (cached: 0, remote: 9, local: 0)` | 1 min |
| immediately again, daemon kept | no action looked up | 0.09 s |
| immediately again, buck2's daemon restarted (nothing in its memory) | `2078 (cached: 2078, remote: 0, local: 0)` | 1.6 s |
