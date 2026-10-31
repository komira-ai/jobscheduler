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
