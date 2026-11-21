# The build farm lane: the workspace with Buck2 on the build farm

Cargo is jobscheduler's build of record. CI's gate (`ci.yml`), the coverage ratchet and every
release use it, on GitHub-hosted runners, and that does not change. The build farm lane builds the
same workspace a second way: with Buck2, through [buildrules](https://github.com/komira-ai/buildrules)'
Rust rules, every action on a remote-execution build farm or taken from its cache. ("The build
farm" here is that remote-execution service, not the hosts js-agent runs on.)

The lane is additive and advisory. `.github/workflows/farm.yml` runs it on every push to `main` and
on every pull request from a branch of this repository (not a fork's, not Dependabot's). No check
of it is required, and a build farm outage turns it red and holds nothing.

## What it builds

`buck2 build //:ci` runs three steps for each member of the workspace:

- `:fmt`: the pinned `rustfmt --check` over the files `cargo fmt` reads, with `rustfmt.toml`.
- `:lint`: clippy over the library, the binaries and every test target, with `[workspace.lints]`
  and `clippy.toml`, warnings as errors.
- `:test`: each test target run as a build action. Its `.passed` marker exists only if every test of
  the target passed, and a cached marker means the tests passed on byte-identical inputs.

The members listed below leave out some of these steps. The summary line must read
`Commands: N (cached: N, remote: N, local: 0)`: no action ran on the runner.

`farm.yml` runs the build as one command, `cargo xtask farm-build` (`crates/xtask/src/farm.rs`). It
refuses to start without the build farm's worker property set, fails a build that does not say
`local: 0`, and stops buck2's daemon however the build ended.

The files of the build, all generated or written once by `buildrules`:

| file | what | written by |
|---|---|---|
| `.buckconfig` | the cells, buildrules pinned by commit, the execution platform | `buildrules init`, then by hand (the worker passthrough below) |
| `toolchains/`, `platforms/` | the pinned Rust toolchain and the remote execution platform | `buildrules init` |
| `BUCK`, `crates/*/BUCK` | `cargo_workspace` and one `cargo_package` per member | `buildrules init` and `buildrules cargo adopt`, once; then by hand (`extra_srcs`, below) |
| `buildrules.lock.bzl`, `buildrules.bzl`, `third_party/buildrules/` | Cargo's unit graph of every member; every crates.io crate as a unit pinned by SHA-256 and size; the git dependency (hostkeeper's crates) as GitHub's archive of its pinned commit, by SHA-256 and size (`git_archives.tsv`) | `buildrules sync`; never edited by hand |

`Cargo.toml`, `Cargo.lock` and `rust-toolchain.toml` stay the source. `//:check` fails every
target when the generated table no longer matches them, and prints `buildrules sync` as the fix.
After a change to a manifest or the lock, run `buildrules sync` and commit what it wrote.

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
| `js-caps` | `tests/accel.rs` scans the sources of every member |
| `js-watch` | `tests/read_only.rs` reads other members' sources and walks `crates/` |
| `js-elastic-fake` | `tests/no_cloud_reach.rs` reads every manifest and source file of the repository |
| `js-mac-probe` | its child tests expect an absolute `PATH` |
| `js-mac-session` | its tests read `/etc/passwd` and need directories without other mounts; `tests/as_root.rs` holds `#[ignore]`d root tests |
| `js-testkit` | its fake's tests read `PATH`; its dir, database and reaped targets hold `#[ignore]`d child tests |
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
- a step after farm-connect's that runs `cargo xtask farm-build` of `//:ci`;
- the triggers, the one job and its runner;
- across every workflow, no other job a pull request starts that holds `id-token: write`.

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
