//! The steps of `farm.yml`'s job (`super::farm::JOB`) that reach the build farm, and the time
//! limit of every step.
//!
//! Every buck2 command that reaches the farm runs through komira's buck2-farm action
//! ([`BUCK2_FARM`]): buck2 under `assert_remote run`, whose watchdog ends a run whose connection
//! to the farm has heard nothing for 5 minutes, stops buck2's daemon, says so in the log and the
//! job summary, and runs the command again once the farm answers (at most three runs); its second
//! step fails a run that ran an action on the runner. [`step_problems`] holds the job to:
//! - the job's `env` names [`COMMIT_VAR`] as exactly [`KOMIRA_COMMIT`], the commit of komira main
//!   that has the watchdog (komira-ai/komira#2439), and farm-connect and every buck2-farm step are
//!   pinned to it: the action, its tool's source and the machine buckconfig the tool reads are of
//!   one commit;
//! - the tool's source is fetched as GitHub's archive of that commit ([`SOURCE_RUN`]; its SHA-256
//!   and cache are tests/pinned_tool_cache's) and built with the pinned zig ([`TOOL_BUILD_RUN`])
//!   before the first buck2-farm step;
//! - in this order, after farm-connect: the worker check ([`WORKERS_RUN`]), the build of the
//!   `buildrules` command line ([`TOOL_ARGS`]), the table's check with it ([`TABLE_RUN`]) and the
//!   build of `//:ci` ([`CI_ARGS`]), each buck2-farm step with a `with:` of exactly
//!   [`farm_inputs`]; exactly two buck2-farm steps; then the stop of buck2's daemon
//!   ([`KILL_RUN`]) under `if: ${{ always() }}`.
//!
//! [`run_problems`] holds every workflow of the repository to the action: no `run:` runs buck2
//! (a command whose program is `buck2` or `buck`, by any path) but [`KILL_RUN`] in `farm.yml`,
//! which reaches no farm.
//!
//! [`timeout_problems`] holds every step of the job to a `timeout-minutes` of its own, a whole
//! number of minutes from 1, under the job's own `timeout-minutes`: a step the watchdog does not
//! cover (no step but the two builds is under it) still cannot hang to the job's limit.

use std::collections::BTreeMap;

use super::support::{entry, steps};
use super::yaml::Node;

/// komira's action that runs one buck2 command under the watchdog.
pub const BUCK2_FARM: &str = "komira-ai/komira/.github/actions/buck2-farm";
/// The variable of the job's `env` that names komira's commit.
pub const COMMIT_VAR: &str = "KOMIRA_COMMIT";
/// The komira commit the job is pinned to: komira main with the watchdog (komira-ai/komira#2439).
/// A move of the pin is a change of this line too.
pub const KOMIRA_COMMIT: &str = "b2080fe989248adf9b47d4bd14c0ecefa0802f79";
/// The fetch of the tool's source.
pub const SOURCE_RUN: &str = "cargo xtask fetch --url \"https://github.com/komira-ai/komira/archive/$KOMIRA_COMMIT.tar.gz\" --sha256 \"$KOMIRA_SHA256\" --cache \"$RUNNER_TEMP/pinned/komira.tar.gz\" --out \"$RUNNER_TEMP/assert_remote\" --extract tar.gz --member \"komira-$KOMIRA_COMMIT/tools/build/ci/assert_remote\" --tree";
/// The build of the tool.
pub const TOOL_BUILD_RUN: &str = "\"$RUNNER_TEMP/zig/zig\" build-exe -OReleaseSafe -fstrip --cache-dir \"$RUNNER_TEMP/zig-cache\" --global-cache-dir \"$RUNNER_TEMP/zig-cache\" -femit-bin=\"$RUNNER_TEMP/bin/assert_remote\" \"$RUNNER_TEMP/assert_remote/assert_remote.zig\"";
/// The check of the worker property set, before any build.
pub const WORKERS_RUN: &str = "cargo xtask farm-workers --buck2 \"$RUNNER_TEMP/bin/buck2\"";
/// The `args` of the build of the `buildrules` command line.
pub const TOOL_ARGS: &str =
    "build buildrules//cli:buildrules --out ${{ runner.temp }}/bin/buildrules";
/// The check of the generated build table with that command line.
pub const TABLE_RUN: &str = "cargo xtask farm-table --buck2 \"$RUNNER_TEMP/bin/buck2\" --buildrules \"$RUNNER_TEMP/bin/buildrules\" --patch \"$RUNNER_TEMP/buildrules-sync/buildrules-sync.patch\" --summary \"$GITHUB_STEP_SUMMARY\" --repo \"$GITHUB_REPOSITORY\" --run-id \"$GITHUB_RUN_ID\"";
/// The `args` of the build of the lane.
pub const CI_ARGS: &str = "build //:ci";
/// The stop of buck2's daemon, the one `run:` of the repository that runs buck2.
pub const KILL_RUN: &str = "\"$RUNNER_TEMP/bin/buck2\" kill";
/// The exact `if:` of [`KILL_RUN`]'s step.
pub const KILL_IF: &str = "${{ always() }}";

/// The `with:` of a buck2-farm step whose command is `args`, exactly.
pub fn farm_inputs(args: &str) -> BTreeMap<&str, &str> {
    BTreeMap::from([
        ("args", args),
        ("assert-remote", "${{ runner.temp }}/bin/assert_remote"),
        ("buck2", "${{ runner.temp }}/bin/buck2"),
    ])
}

/// The scalars of the mapping at `key` of `node`, by key; `None` when it is missing or any of
/// its keys or values is not a scalar.
fn scalars_at<'a>(node: &'a Node, key: &str) -> Option<BTreeMap<&'a str, &'a str>> {
    let map = entry(node, key)?;
    super::support::entries(map)
        .iter()
        .map(|(k, v)| Some((k.as_str()?, v.as_str()?)))
        .collect()
}

/// The ref after `@` of the `uses:` of `step` when it uses `action`, a lookalike name excluded.
fn ref_of<'a>(step: &'a Node, action: &str) -> Option<&'a str> {
    entry(step, "uses")
        .and_then(Node::as_str)
        .and_then(|u| u.strip_prefix(action))
        .and_then(|rest| rest.strip_prefix('@'))
}

/// The index of the one step of `all` whose `run:` is `run`.
fn run_at(all: &[Node], run: &str) -> Option<usize> {
    all.iter()
        .position(|s| entry(s, "run").and_then(Node::as_str) == Some(run))
}

/// What is wrong with the farm steps of the job `job` (see the module's words).
pub fn step_problems(job: &Node) -> Vec<String> {
    let job_name = super::farm::JOB;
    let mut problems = Vec::new();
    let env = scalars_at(job, "env").unwrap_or_default();
    if env.get(COMMIT_VAR) != Some(&KOMIRA_COMMIT) {
        problems.push(format!(
            "job {job_name}: env {COMMIT_VAR} must be {KOMIRA_COMMIT}, the komira commit with the watchdog"
        ));
    }
    let all = steps(job);
    for (i, step) in all.iter().enumerate() {
        for action in [super::farm::FARM_CONNECT, BUCK2_FARM] {
            if ref_of(step, action).is_some_and(|at| at != KOMIRA_COMMIT) {
                problems.push(format!(
                    "job {job_name}: step {} uses {action} at another commit than {COMMIT_VAR} ({KOMIRA_COMMIT})",
                    i + 1
                ));
            }
        }
    }
    let farm: Vec<usize> = (0..all.len())
        .filter(|&i| ref_of(&all[i], BUCK2_FARM).is_some())
        .collect();
    let [tool, ci] = farm[..] else {
        problems.push(format!(
            "job {job_name}: exactly two steps must use {BUCK2_FARM}: the build of the buildrules command line, then of //:ci"
        ));
        return problems;
    };
    for (at, args) in [(tool, TOOL_ARGS), (ci, CI_ARGS)] {
        if scalars_at(&all[at], "with") != Some(farm_inputs(args)) {
            problems.push(format!(
                "job {job_name}: step {}'s `with:` must be exactly {:?}",
                at + 1,
                farm_inputs(args)
            ));
        }
    }
    let connect = all
        .iter()
        .position(|s| ref_of(s, super::farm::FARM_CONNECT).is_some());
    // Each step in its order. A missing farm-connect is super::farm's problem, not one here.
    let order = [
        (
            "the fetch of assert_remote's source",
            run_at(all, SOURCE_RUN),
            true,
        ),
        (
            "the build of assert_remote",
            run_at(all, TOOL_BUILD_RUN),
            true,
        ),
        ("farm-connect", connect, false),
        ("the worker check", run_at(all, WORKERS_RUN), true),
        ("the build of the buildrules command line", Some(tool), true),
        ("the table's check", run_at(all, TABLE_RUN), true),
        ("the build of //:ci", Some(ci), true),
        ("the stop of buck2's daemon", run_at(all, KILL_RUN), true),
    ];
    for (what, at, required) in &order {
        if *required && at.is_none() {
            problems.push(format!("job {job_name}: no step is {what}"));
        }
    }
    let present: Vec<(&str, usize)> = order
        .iter()
        .filter_map(|(what, at, _)| at.map(|at| (*what, at)))
        .collect();
    for pair in present.windows(2) {
        if let [(before, b), (after, a)] = pair
            && b >= a
        {
            problems.push(format!(
                "job {job_name}: {after} does not come after {before}"
            ));
        }
    }
    if let Some(kill) = run_at(all, KILL_RUN)
        && entry(&all[kill], "if").and_then(Node::as_str) != Some(KILL_IF)
    {
        problems.push(format!(
            "job {job_name}: the stop of buck2's daemon must run under exactly `if: {KILL_IF}`"
        ));
    }
    problems
}

/// The programs of the commands of `run` (the text of a `run:`): the first word of each command
/// between `&&`, `||`, `;`, `|`, a line break, a parenthesis or a backquote, without quotes and
/// without the variable settings (`NAME=value`) and the wrappers (`sudo`, `env`, `exec`, `time`,
/// `nice`) before it.
pub fn programs(run: &str) -> Vec<String> {
    let mut found = Vec::new();
    for command in run.split(['\n', ';', '|', '&', '(', ')', '`']) {
        let program = command
            .split_whitespace()
            .map(|w| w.trim_matches(['"', '\'']))
            .find(|w| !w.is_empty() && !is_setting(w) && !is_wrapper(w));
        found.extend(program.map(str::to_owned));
    }
    found
}

/// Whether the word `word` sets a variable for the command (`NAME=value`), not a flag's value.
fn is_setting(word: &str) -> bool {
    word.contains('=') && !word.starts_with('-') && !word.contains('/')
}

/// Whether the word `word` is a wrapper that runs the command after it.
fn is_wrapper(word: &str) -> bool {
    matches!(word, "sudo" | "env" | "exec" | "time" | "nice")
}

/// Whether the program `program` is buck2 (or buck), by its last path component.
fn is_buck2(program: &str) -> bool {
    matches!(program.rsplit('/').next(), Some("buck2" | "buck"))
}

/// What is wrong with the `run:`s of `workflows` (file name, text): any that runs buck2, but
/// [`KILL_RUN`] in `farm.yml`, is a buck2 command outside komira's buck2-farm action.
pub fn run_problems(workflows: &[(String, String)]) -> Vec<String> {
    let mut problems = Vec::new();
    for (file, text) in workflows {
        let Ok(doc) = super::yaml::load(text) else {
            continue;
        };
        let jobs = entry(&doc, "jobs").map_or(&[][..], super::support::entries);
        for (name, job) in jobs {
            for step in steps(job) {
                let Some(run) = entry(step, "run").and_then(Node::as_str) else {
                    continue;
                };
                if file == super::farm::FILE && run == KILL_RUN {
                    continue;
                }
                if programs(run).iter().any(|p| is_buck2(p)) {
                    problems.push(format!(
                        "{file}: job {}: `{run}` runs buck2 outside {BUCK2_FARM}, with no watchdog of its connection to the build farm",
                        name.as_str().unwrap_or_default()
                    ));
                }
            }
        }
    }
    problems
}

/// The whole minutes of the `timeout-minutes` of `node`, when it is a number from 1.
fn minutes(node: &Node) -> Option<u32> {
    entry(node, "timeout-minutes")
        .and_then(Node::as_str)
        .and_then(|m| m.parse().ok())
        .filter(|&m| m >= 1)
}

/// What is wrong with the time limits of the job `job`: it has one, and every step has one of
/// its own, under the job's.
pub fn timeout_problems(job: &Node) -> Vec<String> {
    let job_name = super::farm::JOB;
    let Some(limit) = minutes(job) else {
        return vec![format!(
            "job {job_name}: no timeout-minutes of whole minutes from 1"
        )];
    };
    let mut problems = Vec::new();
    for (i, step) in steps(job).iter().enumerate() {
        let what = entry(step, "name")
            .or_else(|| entry(step, "uses"))
            .or_else(|| entry(step, "run"))
            .and_then(Node::as_str)
            .unwrap_or_default();
        match minutes(step) {
            None => problems.push(format!(
                "job {job_name}: step {} ({what}) has no timeout-minutes of whole minutes from 1, so it may run to the job's {limit}",
                i + 1
            )),
            Some(m) if m >= limit => problems.push(format!(
                "job {job_name}: step {} ({what}) may run {m} minutes, not under the job's {limit}",
                i + 1
            )),
            Some(_) => {}
        }
    }
    problems
}
