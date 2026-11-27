//! The rules of `super::farm_steps`: every buck2 command that reaches the build farm runs through
//! komira's buck2-farm action at the one komira commit with the watchdog, in the job's order, and
//! every step of farm.yml's job has a time limit of its own under the job's. Each rule is shown to
//! refuse its defect by putting it into the committed workflow and reading the exact problems the
//! lint reports.

use super::farm::{FILE, JOB, problems};
use super::farm_steps::{
    BUCK2_FARM, CI_ARGS, KILL_IF, KILL_RUN, KOMIRA_COMMIT, TOOL_ARGS, farm_inputs, programs,
    run_problems,
};
use super::support::{entry, steps, text_of, workflows};
use super::yaml::{self, Node};

/// The problems of the farm.yml text `text`; a text that is not YAML is its own problem.
fn problems_of(text: &str) -> Vec<String> {
    match yaml::load(text) {
        Ok(doc) => problems(&doc),
        Err(e) => vec![e],
    }
}

/// The committed farm.yml with its one occurrence of `from` replaced by `to`.
fn with(from: &str, to: &str) -> String {
    let text = text_of(FILE);
    assert_eq!(text.matches(from).count(), 1, "{from:?} once in farm.yml");
    text.replace(from, to)
}

/// The problems of the committed farm.yml with `from` replaced by `to`.
fn problems_with(from: &str, to: &str) -> Vec<String> {
    problems_of(&with(from, to))
}

/// The 1-based number of the committed job's step named `name`.
fn number(name: &str) -> usize {
    let doc = yaml::load(&text_of(FILE)).expect("farm.yml is YAML");
    let job = entry(&doc, "jobs")
        .and_then(|jobs| entry(jobs, JOB))
        .expect("the job");
    1 + steps(job)
        .iter()
        .position(|s| entry(s, "name").and_then(Node::as_str) == Some(name))
        .unwrap_or_else(|| panic!("no step {name}"))
}

/// The names of the two buck2-farm steps of the committed file.
const TOOL_STEP: &str = "buck2 build the buildrules command line on the build farm, local 0";
const CI_STEP: &str = "buck2 build //:ci on the build farm, local 0";

/// The `uses:` of buck2-farm at the pinned commit, as the committed file writes it.
fn farm_use() -> String {
    format!("uses: {BUCK2_FARM}@{KOMIRA_COMMIT}")
}

#[test]
fn the_committed_workflows_hold_every_farm_rule() {
    // Catches: rules that refuse the files they were written for, and a lint that holds nothing:
    // it does find the one buck2 command of the repository's `run:`s, the stop of the daemon.
    assert_eq!(problems_of(&text_of(FILE)), Vec::<String>::new());
    assert_eq!(run_problems(&workflows()), Vec::<String>::new());
    assert_eq!(programs(KILL_RUN), ["$RUNNER_TEMP/bin/buck2"]);
    let text = text_of(FILE);
    assert_eq!(text.matches(&farm_use()).count(), 2);
    assert!(text.contains(&format!("      KOMIRA_COMMIT: {KOMIRA_COMMIT}\n")));
}

#[test]
fn the_program_of_each_command_is_read_and_an_argument_is_not_one() {
    // Catches: buck2 missed behind a chain, a pipe, a subshell, a variable setting, a wrapper or a
    // quoted path, and buck2 named as an argument (the flag `--buck2` of xtask, its download's
    // `--out`) taken for a run of it.
    for (run, want) in [
        ("./buck2 build //:ci", vec!["./buck2"]),
        ("cd x && buck2 build //:ci", vec!["cd", "buck2"]),
        ("true; buck test //...", vec!["true", "buck"]),
        ("(\"$B/buck2\" build) | tee log", vec!["$B/buck2", "tee"]),
        ("BUCK_CONSOLE=simple buck2 build", vec!["buck2"]),
        ("sudo env ./buck2 kill", vec!["./buck2"]),
        ("echo `buck2 --version`", vec!["echo", "buck2"]),
        (
            "cargo xtask farm-workers --buck2 \"$RUNNER_TEMP/bin/buck2\"",
            vec!["cargo"],
        ),
        (
            "cargo xtask fetch --url \"https://x/buck2\" --out \"$RUNNER_TEMP/bin/buck2\"",
            vec!["cargo"],
        ),
        ("", vec![]),
    ] {
        assert_eq!(programs(run), want, "{run}");
    }
}

#[test]
fn a_buck2_command_outside_the_action_is_refused_in_any_workflow() {
    // Catches: the build of //:ci run by a plain `run:` again (no watchdog: a dead connection to
    // the farm hangs it to its time limit), a buck2 run added to another workflow, and the
    // daemon's stop changed into a build.
    let plain = format!("\"$RUNNER_TEMP/bin/buck2\" {CI_ARGS}");
    let ci_step = format!(
        "        {}\n        timeout-minutes: 120\n        with:\n          args: {CI_ARGS}\n          assert-remote: ${{{{ runner.temp }}}}/bin/assert_remote\n          buck2: ${{{{ runner.temp }}}}/bin/buck2\n",
        farm_use()
    );
    let replaced = with(
        &ci_step,
        &format!("        timeout-minutes: 120\n        run: '{plain}'\n"),
    );
    assert_eq!(
        problems_of(&replaced),
        [format!(
            "job build: exactly two steps must use {BUCK2_FARM}: the build of the buildrules command line, then of //:ci"
        )]
    );
    let mut all = workflows();
    for (file, text) in &mut all {
        if file.as_str() == FILE {
            *text = replaced.clone();
        }
    }
    let outside = |file: &str, job: &str, run: &str| {
        format!(
            "{file}: job {job}: `{run}` runs buck2 outside {BUCK2_FARM}, with no watchdog of its connection to the build farm"
        )
    };
    assert_eq!(run_problems(&all), [outside(FILE, JOB, &plain)]);

    let mut all = workflows();
    let added = "name: other\non: push\njobs:\n  b:\n    runs-on: ubuntu-24.04\n    steps:\n      - run: ./buck2 test //crates/js-types:test\n";
    all.push(("other.yml".to_owned(), added.to_owned()));
    assert_eq!(
        run_problems(&all),
        [outside(
            "other.yml",
            "b",
            "./buck2 test //crates/js-types:test"
        )]
    );

    let mut all = workflows();
    for (file, text) in &mut all {
        if file.as_str() == FILE {
            *text = text.replace(
                &format!("run: '{KILL_RUN}'"),
                "run: '\"$RUNNER_TEMP/bin/buck2\" build //:ci'",
            );
        }
    }
    assert_eq!(
        run_problems(&all),
        [outside(FILE, JOB, "\"$RUNNER_TEMP/bin/buck2\" build //:ci")]
    );
}

#[test]
fn every_komira_action_is_at_the_one_commit_with_the_watchdog() {
    // Catches: a buck2-farm step at a commit without the watchdog (or another than its tool's
    // source and farm-connect's buckconfig), and the variable the source is fetched by moved alone.
    let older = "a26616d62db17ee8aff67ee9c12d411c2fad7609";
    let ci_use = format!("{}\n        timeout-minutes: 120\n", farm_use());
    assert_eq!(
        problems_with(
            &ci_use,
            &format!("uses: {BUCK2_FARM}@{older}\n        timeout-minutes: 120\n")
        ),
        [format!(
            "job build: step {} uses {BUCK2_FARM} at another commit than KOMIRA_COMMIT ({KOMIRA_COMMIT})",
            number(CI_STEP)
        )]
    );
    assert_eq!(
        problems_with(
            &format!("      KOMIRA_COMMIT: {KOMIRA_COMMIT}\n"),
            &format!("      KOMIRA_COMMIT: {older}\n")
        ),
        [format!(
            "job build: env KOMIRA_COMMIT must be {KOMIRA_COMMIT}, the komira commit with the watchdog"
        )]
    );
}

#[test]
fn each_farm_step_takes_exactly_its_inputs_and_there_are_exactly_two() {
    // Catches: the lane's build pointed at another target, the command line built to another
    // path than the table's check reads, the farm step given the checkout's `./buck2` (there is
    // none) instead of the pinned one, and a third buck2 command (one too few is the plain
    // `run:` of the build above).
    let ci_with = |n: usize| {
        format!(
            "job build: step {n}'s `with:` must be exactly {:?}",
            farm_inputs(CI_ARGS)
        )
    };
    assert_eq!(
        problems_with(
            &format!("          args: {CI_ARGS}\n"),
            "          args: build //:lint\n"
        ),
        [ci_with(number(CI_STEP))]
    );
    assert_eq!(
        problems_with(
            &format!("          args: {TOOL_ARGS}\n"),
            "          args: build buildrules//cli:buildrules --out ${{ runner.temp }}/buildrules\n"
        ),
        [format!(
            "job build: step {}'s `with:` must be exactly {:?}",
            number(TOOL_STEP),
            farm_inputs(TOOL_ARGS)
        )]
    );
    assert_eq!(
        problems_with(
            &format!(
                "          args: {CI_ARGS}\n          assert-remote: ${{{{ runner.temp }}}}/bin/assert_remote\n          buck2: ${{{{ runner.temp }}}}/bin/buck2\n"
            ),
            &format!(
                "          args: {CI_ARGS}\n          assert-remote: ${{{{ runner.temp }}}}/bin/assert_remote\n"
            )
        ),
        [ci_with(number(CI_STEP))]
    );
    let two = format!(
        "job build: exactly two steps must use {BUCK2_FARM}: the build of the buildrules command line, then of //:ci"
    );
    let third = "      - name: Stop buck2's daemon\n";
    assert_eq!(
        problems_with(
            third,
            &format!(
                "      - {}\n        timeout-minutes: 30\n        with:\n          args: test //:ci\n      - name: Stop buck2's daemon\n",
                farm_use()
            )
        ),
        std::slice::from_ref(&two)
    );
}

#[test]
fn the_steps_come_in_their_order_and_none_is_missing() {
    // Catches: the table checked before its command line is built, the tool built after the farm
    // step that needs it, a check left out, and the daemon's stop moved before a build, left
    // out, or run only when the steps before it passed.
    let workers = "      - name: buildrules has the farm's worker property set\n        timeout-minutes: 10\n        run: cargo xtask farm-workers --buck2 \"$RUNNER_TEMP/bin/buck2\"\n";
    let moved = with(workers, "").replace(
        &format!("      - name: {CI_STEP}\n"),
        &format!("{workers}      - name: {CI_STEP}\n"),
    );
    assert_eq!(
        problems_of(&moved),
        [
            "job build: the build of the buildrules command line does not come after the worker check"
        ]
    );
    assert_eq!(
        problems_with(workers, ""),
        ["job build: no step is the worker check"]
    );
    let build = "      - name: Build assert_remote, the buck2-farm action's tool\n";
    let start = text_of(FILE).find(build).expect("the tool's build");
    let end = start
        + text_of(FILE)[start..]
            .find("      # Only a push to main writes the Cargo cache")
            .expect("the step after it");
    let step = text_of(FILE)[start..end].to_owned();
    let late = with(&step, "").replace(
        &format!("      - name: {CI_STEP}\n"),
        &format!("{step}      - name: {CI_STEP}\n"),
    );
    assert_eq!(
        problems_of(&late),
        ["job build: farm-connect does not come after the build of assert_remote"]
    );
    let kill_if = format!("        if: {KILL_IF}\n        continue-on-error: true\n");
    assert_eq!(
        problems_with(&kill_if, "        continue-on-error: true\n"),
        [format!(
            "job build: the stop of buck2's daemon must run under exactly `if: {KILL_IF}`"
        )]
    );
    assert_eq!(
        problems_with(&format!("run: '{KILL_RUN}'"), "run: 'true'"),
        ["job build: no step is the stop of buck2's daemon"]
    );
}

#[test]
fn every_step_has_a_time_limit_of_its_own_under_the_jobs() {
    // Catches: a step with no limit (it would run to the job's 150 minutes, as a hung build did),
    // a limit equal to the job's or above it, a limit that is not a whole number of minutes from
    // 1, and a job with no limit; one minute under the job's is accepted.
    let ci = number(CI_STEP);
    let ci_limit = "        timeout-minutes: 120\n";
    assert_eq!(
        problems_with(ci_limit, ""),
        [format!(
            "job build: step {ci} ({CI_STEP}) has no timeout-minutes of whole minutes from 1, so it may run to the job's 150"
        )]
    );
    for bad in ["0", "ten", "1.5", "-5"] {
        assert_eq!(
            problems_with(ci_limit, &format!("        timeout-minutes: {bad}\n")),
            [format!(
                "job build: step {ci} ({CI_STEP}) has no timeout-minutes of whole minutes from 1, so it may run to the job's 150"
            )],
            "{bad}"
        );
    }
    for at_or_over in ["150", "151"] {
        assert_eq!(
            problems_with(
                ci_limit,
                &format!("        timeout-minutes: {at_or_over}\n")
            ),
            [format!(
                "job build: step {ci} ({CI_STEP}) may run {at_or_over} minutes, not under the job's 150"
            )],
            "{at_or_over}"
        );
    }
    assert_eq!(
        problems_with(ci_limit, "        timeout-minutes: 149\n"),
        Vec::<String>::new()
    );
    // A step with no name is named by what it uses.
    let checkout =
        "        timeout-minutes: 10\n        with:\n          persist-credentials: false\n";
    assert_eq!(
        problems_with(
            checkout,
            "        with:\n          persist-credentials: false\n"
        ),
        [
            "job build: step 1 (actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1) has no timeout-minutes of whole minutes from 1, so it may run to the job's 150"
        ]
    );
    // And one with neither, by its command.
    let rustup = "      - run: rustup toolchain install\n        timeout-minutes: 10\n";
    assert_eq!(
        problems_with(rustup, "      - run: rustup toolchain install\n"),
        [
            "job build: step 2 (rustup toolchain install) has no timeout-minutes of whole minutes from 1, so it may run to the job's 150"
        ]
    );
    assert_eq!(
        problems_with("    timeout-minutes: 150\n", ""),
        ["job build: no timeout-minutes of whole minutes from 1"]
    );
}
