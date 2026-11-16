# Engineering rules

These are the rules for every change to this repository. Whoever writes the code, a person or a coding agent, writes to them, and review checks every pull request against them. `CLAUDE.md` imports this file, so every tool reads the same text.

Each rule has an id and a severity. **BLOCK** is fixed before merge. **SHOULD** is fixed unless the author gives a reason.

The file has three parts: the shared core, which every repository of the project carries word for word; the Rust rules; and the rules of this repository.

The shared core is the text between the two `shared-core` markers below. `docs/agents/shared-core.md` is its canonical copy and `docs/agents/shared-core.sha256` the copy's SHA-256. `cargo test -p xtask --test agents_md` fails when the section differs from the copy, when the copy no longer matches its hash, or when `CLAUDE.md` does not import this file. To change the core, change all three in one pull request.

<!-- shared-core:begin -->
## Shared core

### Tests and coverage

- T1 BLOCK. Every touched package or crate is at least 99% line and 99% branch as measured by the PR check. Aim 100%; never demand above 99. Check: the coverage job table. Fix: add behaviour tests for the uncovered lines and branches.
- T2 SHOULD. Coverage means line and branch percent. Reject claims based on test-file or test-function counts.
- T3 BLOCK. No coverage floor or baseline is lowered or set below 99, and no shared coverage census or shared ratchet file is added. Floors live in each package. Check: diff of floor/baseline files.
- T4 BLOCK. Do not hide code from measurement: no `#[cfg(not(coverage))]`, `#[coverage(off)]`, moving a crate into `exclude`, `test = false` binaries. A `main.rs` only forwards to its library. Check: grep the diff.
- T5 BLOCK. A bug fix ships a test that failed before the fix and is wired in (`#[test]` in the crate; named in `test_srcs` for Mojo). Check: find the test, read it against the pre-fix code, check the PR text says what it asserts.
- T7 BLOCK. Never edit a test green over a capability gap: no weakened or deleted assertions, no tests removed from `test_srcs`, no `tests_known_failing`.
- T8 BLOCK. Tests assert behaviour. The PR body lists tests by name with the behaviour each proves. A "Catches ..." line is allowed; stating the defect caught is SHOULD.
- T9 BLOCK. No mutant bookkeeping in code or PR text: no mutant tables, planted-defect run ids, "seen red" notes, or test names/comments mentioning mutants. Check: grep diff and PR body for `mutant`, `seen red`, `planted defect`.
- T10 BLOCK. Tests never assert timing: no elapsed-time bounds, latency ceilings, or sleeps racing real timeouts. Inject a clock or use explicit progress events. Check: grep for `Instant::now`, `elapsed`, `sleep(`, `perf_counter`, timeout races used as pass/fail.
- T11 BLOCK. Tests are written in the component's own language. No new shell or Python tests (`*_selftest.sh`, `tests/*.sh`, `py_test`), and no growth of existing shell selftests, except for the Python SDK surface. New cases go to a Rust tool with Rust tests.
- T12 BLOCK. Tests are hermetic and deterministic: no network or DNS (use fakes), no hard-coded ports or fixed paths, no shared state between tests, no silent skip (a skip states its reason), no retry-until-green. Use the repo's scratch helpers; tests never touch the host's real service directories.
- T13 SHOULD. Tests cover failure and boundary cases, not only the happy path. A no-leak or redaction test asserts the full message by exact equality.
- T15 BLOCK. A new gate, lint, or check has a negative case that fails on a defect. A warm cached green does not prove a closure change. Describe it as a behaviour test (T9 applies).
- T19 SHOULD. A behaviour change updates goldens and any docs describing it in the same PR; a generator or codegen change moves its goldens in the same PR (BLOCK for codegen).

### Design and change shape

- B8 BLOCK. Formats change additively only: new optional field, record type, oneof case, table, or column-with-default. Reject renumbering, wire renames, removing or re-meaning a shipped field, new required fields, refusing unknown data, or raising a version floor. The PR names the test that runs the previous release on new data and vice versa.
- B20 SHOULD. Do not answer a config mistake with runtime machinery (gates, counters, extra metrics, dry-runs). Fix the config at the source and check it once at deploy. No speculative abstractions or ports without a concrete use case.
- B22 SHOULD. A change touching more than one package, or taking more than a day to reverse, links a design doc. Moving or deleting a package needs the supersession check.
- P7 SHOULD. One subject per PR. Refactors are split from behaviour changes; interfaces land before the code that uses them.
- P10 SHOULD. Docs, headers, and comments describe only what the code does. Flag any doc the diff contradicts or leaves stale (BLOCK when a header contradicts its code).

### CI and workflows

- B9 BLOCK. A PR runs only build and unit tests with coverage. Reject release dry-runs, a `pull_request` trigger on a release workflow, new PR-gate jobs, slow tests in the PR set, and darwin/macOS jobs or targets in per-PR checks. Advisories never block; licences/bans/sources do.
- B10 BLOCK. Workflows: hosted runners only (no self-hosted or paid), actions pinned by commit SHA, top-level `permissions: {}` (kbf), no `pull_request_target`, external tools pinned by URL and SHA-256. Workflows are actionlint clean.
- B12 SHOULD. Releases are automatic from main: no manual tag step, no bake or staging-verdict evaluator.

### PR text and commits

- P1 SHOULD. The title says concretely what changes. No vague words: trustworthy, harden, robust, improve.
- P2 SHOULD. The body leads with the user-facing change (before and after, and what does not change); then a layer-by-layer walk in reading order with the why; then compatibility and failure modes; then tests by name with the behaviour each asserts. Plain language. Suggest a split if part can land alone.
- P3 BLOCK. Commit messages cite issues as `Refs komira-ai/<repo>#<n>`, never `Fixes`, `Closes`, `Resolves`. A bare `#12` is SHOULD; name the repo.
- P4 SHOULD. Authors use their own identity. A commit written with a model ends with a `Co-authored-by:` trailer naming that model. Repos enforcing DCO (kbf, ucdk): every commit, merge commits included, has a `Signed-off-by:` trailer matching the author email (BLOCK there).
- P5 SHOULD. History is not rewritten: no `fixup!`/`squash!` commits, no force-push, no amend or rebase of pushed commits. A correction is a new commit saying what it corrects.
- P6 BLOCK. The PR contains only paths for its purpose. No `.claude/agent-memory/**`, scratch, logs, `.out` files, or unrelated churn. Check: file list.
- P8 SHOULD. Stacked PRs: the body states the stack and merge order; the base is main at merge time; stacked trains land with merge commits, not squash.
- P9 BLOCK. CI ran on the PR head SHA, not a stale merge ref. Compare the CI run SHA with the head SHA.
- P11 SHOULD. Every number in PR text (tests, callers, PRs, cache percentages) names the query that produced it and separates measured from inferred.

### Secrets and configuration

- S1 BLOCK. No secret, token, key, endpoint address, or worker property set committed. No remote-execution endpoint or `instance_name` in komira. Check: grep for URLs with ports, bearer tokens, private keys, `AWS_`/`GOOGLE_` key material, `remote_execution` keys.
- S2 BLOCK. Configuration is passed as flags, not environment variables. Allowed env: platform-set mode markers, secret material, provider-standard credentials (`AWS_*`, `GOOGLE_*`). Check: grep for `getenv`, `std::env::var`, `env::var_os`, `os.environ`; judge each against the three exceptions.
- S3 BLOCK. A diff must not make a cluster write, upload, deploy, or cloud spend executable by default, or run against a staging or production account. Owner-only steps are held scripts: dry-run default, never forcing, `bash -n` clean, in the documented location.
- S4 SHOULD. Committed scripts and docs use the workspace scratch directory, not `/tmp`. No `rm -rf` in scripts; use the scratch-removal tool.
<!-- shared-core:end -->

## Rust

- R1 BLOCK. `cargo fmt --all --check` and `cargo clippy --workspace --all-targets --locked -- -D warnings` are green. No new `#[allow(...)]`/`#![allow(...)]` without a stated reason.
- R2 BLOCK. No `unsafe` outside the platform bindings (system calls, Objective-C and C interfaces) that cannot be written without it, and every `unsafe` block there carries a `// SAFETY:` comment saying why it is sound (or, at a C call, an `// FFI-BOUNDARY:` comment saying who owns and frees each pointer). Check: grep the diff.
- R3 BLOCK. No new `.unwrap()` or `.expect(` in production paths. Libraries use `thiserror` enums with context; `anyhow` only in binaries; no `Box<dyn Error>` in library signatures.
- R6 BLOCK. Dependencies come from crates.io (or pinned sibling-repo revisions) with a permissive licence; `cargo deny` licences, bans, and sources pass. Advisories (RUSTSEC) never block a PR.
- R7 BLOCK. Manifests parse after the merge: no duplicate keys or merge artifacts in changed `Cargo.toml`.
- R8 BLOCK. Crate drift: if main changed anything under a crate this PR touches (`crates/<crate>/`, including src, tests, Cargo files) since CI ran, require main merged and a fresh CI run. Check: `git diff head...main -- crates/<crate>`.
- R9 SHOULD. Minimal public API: small trait bounds, ownership visible in signatures, newtypes, no trait with a single implementation, no speculative abstraction.
- R12 SHOULD. Files stay under 1000 lines; a file already over 1000 lines does not grow.
- T17 SHOULD. Rust coverage: do not rely on tests the PR profile skips; arms of one function covered only by different test binaries or generic instantiations count as missed.

## This repository

- B14 BLOCK. jobscheduler: configuration is a config file plus service start (no setup/check CLIs, no public listener, no per-provider network modes); memory limited only by `memory_schedulable_percent` (no `MemoryMax`, pod memory limits, `--actions-memory-max-gib`); CPU slack is a setting (default 1.1x) and nothing deliberately exceeds it. Disk caps are exempt.
- R4 BLOCK. Pure crates (the crates of the `types` and `pure` layers in `layers.toml`): no clock reads, no sleep, `BTreeMap`/`BTreeSet` not `HashMap`/`HashSet`, no sockets, no tokio/tonic/hyper/reqwest/rand/getrandom/mio/socket2, and `#![deny(clippy::disallowed_methods, clippy::disallowed_types)]` on one line in each lib/bin root, never relaxed.
- R5 BLOCK. Layering: a crate depends only on its own layer or lower; a new workspace crate is added to `layers.toml` in the same PR; one `js-types` only; jobscheduler never depends on kbf crates; external deps are `name.workspace = true`, declared once at the root. `cargo test -p xtask --test layering` checks R4 and R5; `docs/layering.md` says how a crate is added.
- R10 BLOCK. Every public item is documented (`missing_docs`, which the workspace sets and clippy's `-D warnings` makes an error).
- B17 SHOULD. Host updates use one generic updater with per-service update-hook subcommands declared in a root-owned file; no per-roll scripts or per-service logic in the updater.
- B9-linux BLOCK. This repository's rule for which platforms a pull request runs on, which B9's ban on macOS jobs in per-PR checks leaves open: every job a pull request starts runs on GitHub's hosted Linux x86_64 runners (`ubuntu-latest` or `ubuntu-<YY>.<MM>`), and on nothing else: no `macos-*`, `windows-*`, arm64 (`*-arm`), larger or self-hosted runner, and no job whose runner the workflow text does not settle. x86_64 Linux hosts are the many, so they are the PR checks. Every other platform is the beta stage of the release pipeline: on a push to main, after `artifacts`, `release.yml` calls `beta.yml` (the workspace's tests on Apple silicon), `js-agent-macos-pkg.yml` (the unsigned installer package) and `macos-probe.yml` (the Seatbelt probe) as its jobs `beta-macos`, `beta-macos-pkg` and `beta-macos-probe`, and `promote-macos` adds the macOS archives to the release only when all three passed. The Linux x86_64 files, the .debs, the apt repository, gamma and the control-plane deploy do not wait for the beta stage. A pull request that changes `release.yml` runs `plan` as a dry run on Linux x86_64 and builds nothing: `artifacts` and the beta jobs run for a push to main alone. `cargo test -p xtask --test pr_linux_only` holds every workflow to this, with negative cases (a pull request job on macOS, arm64, Windows or a custom runner, an arm leg in a matrix, a beta job without its push condition); a pull request that gives a pull request job another runner changes this rule in the same pull request.

Coverage here is the ratchet in `coverage-baseline` (see the README's Coverage section): the coverage job fails a crate that misses more lines or branches than its line records, or misses fewer (so the file never keeps slack), and every crate records 0, or `-` where it has nothing of that kind to measure. It is stricter than T1, and T1's 99% does not loosen it.
