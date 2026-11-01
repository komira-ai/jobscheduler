//! `js-measure`: the measurement harness of the jobscheduler paper. It varies one setting
//! across arms, runs a synthetic workload against the lease server in a randomised
//! order, and records one line per run for the analysis to read.
//!
//! # The pieces
//!
//! - [`Plan`]: the parameter, the settings it takes, the arms compared at each setting (one
//!   may be the control), runs per cell, warm-ups and
//!   a seed. [`Plan::schedule`] draws the order from the seed alone.
//! - [`Workload`]: a job mix as data.
//! - [`Farm`]: the calls made to the lease server; [`HttpFarm`] is the live one,
//!   [`FakeFarm`] a simulated one with a [`FakeClock`] for tests and dry runs.
//! - [`drive`]: submits the jobs and watches them to the end ([`Observation`]).
//! - [`assemble`]: the numbers of a run, from the observation and the server's readings.
//! - [`run_plan`]: the plan end to end.
//! - [`reported`]: the numbers a job prints about itself.
//! - [`group`] and [`Summarise`]: the hand-off to the analysis.
//!
//! # Output
//!
//! `js-measure run --out DIR` appends to three JSON-lines files in `DIR`:
//!
//! - `records.jsonl`: one [`Record`] per run: `setting`, `arm`, `run_index`, `seed`,
//!   `commit`, `label` (`"warmup"` on a warm-up) and `metrics`, a map from the names in
//!   [`metric::name`] to a number or `null` (not available).
//! - `jobs.jsonl`: one row per job of each run with the job's own facts (the k of
//!   `runtime_actual_over_predicted` per job, for the histogram).
//! - `minutes.jsonl`: one row per host-minute of the run window with the CPU and RAM
//!   share of the host.
//!
//! A number is `null` when its source cannot be read. The sources, and what the
//! server does not publish today, are listed in [`measure`].
//!
//! # Numbers a job reports
//!
//! A job may print `js-measure-result {"steady_ms":12.3}` to its standard output. After the
//! run is over the harness reads the end of the run's output from the lease server and
//! records the numbers as `reported_<key>` in the run's record (the rules, and what a
//! missing or malformed line records, are in [`reported`]). The job runs as a container on
//! a lease like any other, so what it reports is measured through the lease server.
//!
//! # Whole-host noise
//!
//! `plans/whole-host-noise.plan.json` and `plans/whole-host-noise.workload.json` compare
//! the same benchmark cell (TPC-H q6 at scale sf1, run by `komira_bench_runner_main`) on a
//! shared node (the `shared` arm, the `action` class the workload names) with the same cell
//! on a whole host (the `whole-host` arm, which asks for the `whole-node` lease class), 20
//! runs each. The runner takes every value as a flag; with `--js-measure-result` its last
//! standard-output line is `js-measure-result {"steady_ms":..,"first_run_ms":..,
//! "duckdb_steady_ms":..,"verdict":"PASS|..."}`. The numbers are recorded as
//! `reported_steady_ms`, `reported_first_run_ms` and `reported_duckdb_steady_ms`; `verdict`
//! is a string, so no key is recorded for it. A run whose verdict is not PASS is seen from
//! the job's exit code (its outcome is not `succeeded`), not from that key, and the numbers
//! of such a job are left out of the run's record and counted in `reports_dropped`;
//! a job that succeeded but printed no line leaves the run with no `reported_` key and is
//! counted in `reports_missing`.
//!
//! NOT RUNNABLE until the pinned benchmark image exists: the workload cannot be run as
//! shipped. The image is a placeholder because no pinned benchmark image exists yet: it has
//! to be built, with the runner, a pinned duckdb and a verified dataset cache, and pushed before a live run. The flags whose values depend on
//! that image's layout (the duckdb path, the cache dir, the sql file) and on the host (the
//! host class, cpu model, os, core count, memory bytes, version) are `REPLACE-WITH-...`
//! placeholders; the workload format has no lease environment to take the host facts from.
//! Replace them all before running. A class or image the server refuses is recorded as that
//! job's `failure` (for example `the server answered 422 unknown-class`) in `jobs.jsonl`,
//! and the run has no reported number. The records go to the analysis:
//!
//! ```text
//! js-measure run --plan plans/whole-host-noise.plan.json \
//!     --workload plans/whole-host-noise.workload.json --out DIR --commit SHA --live ...
//! js-analysis DIR/records.jsonl --compare-cv shared whole-host
//! ```
//!
//! # Live mode
//!
//! `--live --server URL --token-file F [--ca-cert F]` runs against a lease server; without
//! `--live` the run is against a [`FakeFarm`] of `--fake-hosts` hosts on a fake clock, which
//! shows a plan end to end and writes no real measurement. `--apply-command PROG` runs
//! `PROG <parameter> <setting> <arm> control|treatment` before each run to put the arm's setting in
//! place; without it the operator owns the setting.

pub mod clock;
pub mod driver;
pub mod exec;
pub mod fake;
pub mod farm;
pub mod http;
pub mod measure;
pub mod metric;
pub mod plan;
pub mod reported;
pub mod summary;
pub mod workload;

mod cli;

pub use cli::{Cli, CliError, Command, execute, main};
pub use clock::{Clock, SystemClock};
pub use driver::{JobTiming, Observation, drive};
pub use exec::{CommandSetter, Experiment, ExperimentError, NoSetter, SetError, Setter, run_plan};
pub use fake::{FakeClock, FakeFarm};
pub use farm::{Acquired, Curve, Farm, FarmError, HostShape, LeaseAsk, RunAsk, UsageRow};
pub use http::HttpFarm;
pub use measure::{RunOutput, assemble};
pub use metric::{Record, finite};
pub use plan::{Arm, Plan, PlanError, Slot};
pub use summary::{Group, Listing, Summarise, group, read_records};
pub use workload::{Job, Workload, WorkloadError};
