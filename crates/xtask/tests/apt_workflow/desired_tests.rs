//! `hostkeeper-desired.yml` ([`super::desired`]): the file of the repository passes, each way of
//! breaking what it is held to is named, and no workflow reads another coordinator's variable or
//! calls the run.

use super::desired::{
    AUDIENCE, CHECK, CLIENT_COMMIT, CLIENT_RELEASE, COORDINATOR_VARS, DESIRED_IF, FILE, HOST_GROUP,
    HOST_GROUP_MAC, PKG_URL, SET_DESIRED, SET_IF, SET_MAC_IF, caller_problems,
    coordinator_problems, desired_problems, with_wanted,
};
use super::support::{in_job, text_of, workflows};

#[test]
fn hostkeeper_desired_yml_checks_the_release_then_sets_it_by_its_targets() {
    // Catches: any change to hostkeeper-desired.yml that breaks what it is held to: a trigger
    // added or dropped, the gate dropped from a job, the check skipped or not handed on,
    // hostkeeper's workflow called with another release, pin, coordinator or tailnet.
    assert_eq!(desired_problems(&text_of(FILE)), Vec::<String>::new());
}

#[test]
fn the_variable_gate_is_the_job_level_if_of_both_jobs() {
    // Catches: a gate on a step (the job would start, and the token exist, with the deploy off),
    // a gate on another variable or on a value other than `true`, a job that runs from a branch
    // other than main, and a check that runs for an event other than the dispatch.
    for condition in [DESIRED_IF, SET_IF, SET_MAC_IF] {
        assert!(condition.contains("github.ref == 'refs/heads/main'"));
        assert!(condition.contains("vars.JS_AGENT_SETS_DESIRED == 'true'"));
    }
    assert!(SET_IF.contains("needs.check.outputs.set == 'true'"));
    assert!(SET_MAC_IF.contains("needs.check.outputs.set == 'true'"));
    assert!(SET_MAC_IF.contains("needs.check.outputs.set-mac == 'true'"));
    let text = text_of(FILE);
    let check_if = |with: &str| in_job(&text, "check", DESIRED_IF, with);
    for (part, with) in [
        (
            "vars.JS_AGENT_SETS_DESIRED == 'true'",
            "vars.JS_AGENT_SETS_DESIRED != ''",
        ),
        ("github.ref == 'refs/heads/main' && ", ""),
        (" && github.event_name == 'workflow_dispatch'", ""),
        (
            "github.event_name == 'workflow_dispatch'",
            "github.event_name == 'workflow_run'",
        ),
    ] {
        assert!(DESIRED_IF.contains(part), "{part}");
        let spoiled = check_if(&DESIRED_IF.replacen(part, with, 1));
        assert!(
            desired_problems(&spoiled)
                .iter()
                .any(|p| p.starts_with("check: `if` is")),
            "check without {part:?}"
        );
    }
    let spoiled = in_job(
        &text,
        "set-desired",
        " && needs.check.outputs.set == 'true'",
        "",
    );
    assert!(
        desired_problems(&spoiled)
            .iter()
            .any(|p| p.starts_with("set-desired: `if` is")),
        "set-desired whatever check said"
    );
    // The Macs are set only for a release that has a package, and only when `check` chose it.
    for (part, with) in [
        (" && needs.check.outputs.set-mac == 'true'", ""),
        (" && needs.check.outputs.set == 'true'", ""),
        ("vars.JS_AGENT_SETS_DESIRED == 'true'", "true"),
    ] {
        let spoiled = in_job(&text, "set-desired-mac", part, with);
        assert!(
            desired_problems(&spoiled)
                .iter()
                .any(|p| p.starts_with("set-desired-mac: `if` is")),
            "set-desired-mac without {part:?}"
        );
    }
    for (job, condition) in [
        ("check", DESIRED_IF),
        ("set-desired", SET_IF),
        ("set-desired-mac", SET_MAC_IF),
    ] {
        let spoiled = in_job(&text, job, &format!("    if: {condition}\n"), "");
        assert!(
            desired_problems(&spoiled)
                .iter()
                .any(|p| p == &format!("{job}: `if` is None, not {condition}")),
            "{job} without an `if`"
        );
    }
}

/// `(job, text to replace once in it, its replacement, what a problem must say)`.
const DEFECTS: &[(&str, &str, &str, &str)] = &[
    (
        "check",
        "    runs-on: ubuntu-24.04\n",
        "    needs: [set-desired]\n    runs-on: ubuntu-24.04\n",
        "check: needs [\"set-desired\"]; it needs no job",
    ),
    (
        "check",
        "      actions: read\n",
        "      actions: write\n",
        "check: permissions are [\"actions: write\", \"contents: read\"], not actions: read and contents: read",
    ),
    // Without `actions: read` the jobs of the release run cannot be read.
    (
        "check",
        "      actions: read\n",
        "",
        "check: permissions are [\"contents: read\"], not actions: read and contents: read",
    ),
    (
        "check",
        "      contents: read\n",
        "      contents: read\n      id-token: write\n",
        "check: permissions are [\"actions: read\", \"contents: read\", \"id-token: write\"]",
    ),
    (
        "check",
        "--dir \"$RUNNER_TEMP/desired\"",
        "--dir dist",
        "check: the step `desired` does not run",
    ),
    // The verb is not told to wait for a run of another workflow: release.yml's `desired` starts
    // this run when the release is published.
    (
        "check",
        " --pkg-url",
        " --after-run \"$RUN_ID\" --pkg-url",
        "check: the step `desired` does not run",
    ),
    (
        "check",
        "        id: desired\n",
        "        id: index\n",
        "check: the step `desired` does not run",
    ),
    (
        "check",
        "VERSION: ${{ inputs.version }}",
        "VERSION: 0.1.0",
        "check: the step `desired` does not read VERSION from ${{ inputs.version }}",
    ),
    (
        "check",
        "          GH_TOKEN: ${{ github.token }}\n",
        "          GH_TOKEN: ${{ github.token }}\n          RUN_ID: ${{ github.run_id }}\n",
        "check: the step `desired` reads RUN_ID, which it has no use for",
    ),
    (
        "check",
        "GH_TOKEN: ${{ github.token }}",
        "GH_TOKEN: ''",
        "check: the step `desired` does not read GH_TOKEN from ${{ github.token }}",
    ),
    (
        "check",
        "set: ${{ steps.desired.outputs.set }}",
        "set: 'true'",
        "check: output set is",
    ),
    (
        "check",
        "package-version: ${{ steps.desired.outputs.package-version }}",
        "package-version: ${{ inputs.version }}",
        "check: output package-version is",
    ),
    (
        "check",
        "targets: ${{ steps.desired.outputs.targets }}",
        "targets: '{}'",
        "check: output targets is",
    ),
    // An output the reusable workflow was never meant to be given (the archive formats' index).
    (
        "check",
        "      targets: ${{ steps.desired.outputs.targets }}\n",
        "      targets: ${{ steps.desired.outputs.targets }}\n      release-index: ${{ steps.desired.outputs.release-index }}\n",
        "check: it hands on 5 outputs, not the 4 of the step `desired`",
    ),
    (
        "check",
        "set-mac: ${{ steps.desired.outputs.set-mac }}",
        "set-mac: 'true'",
        "check: output set-mac is",
    ),
    // The package repository is not looked in: no release would ever have a Mac entry.
    (
        "check",
        " --pkg-url \"$PKG_URL\"",
        "",
        "check: the step `desired` does not run",
    ),
    (
        "check",
        "PKG_URL: https://packages.komira.ai/pkg/jobscheduler",
        "PKG_URL: https://example.test/pkg",
        "check: the step `desired` does not read PKG_URL from https://packages.komira.ai/pkg/jobscheduler",
    ),
    (
        "set-desired",
        "needs: [check]",
        "needs: [check, publish]",
        "set-desired: needs are [\"check\", \"publish\"], not [\"check\"]",
    ),
    (
        "set-desired",
        "    needs: [check]\n",
        "",
        "set-desired: needs are [], not [\"check\"]",
    ),
    (
        "set-desired",
        "      id-token: write\n",
        "      id-token: write\n      actions: read\n",
        "set-desired: permissions are",
    ),
    (
        "set-desired",
        "      contents: read\n",
        "",
        "set-desired: permissions are [\"id-token: write\"], not contents: read and id-token: write",
    ),
    (
        "set-desired",
        "komira-ai/hostkeeper/.github/workflows/set-desired.yml@",
        "komira-ai/other/.github/workflows/set-desired.yml@",
        "set-desired: `uses` is not",
    ),
    // A revision that is not the release the inputs name.
    (
        "set-desired",
        "set-desired.yml@30141591c292d8881ce5bc16c9d0b3ba7fcfdca1",
        "set-desired.yml@32a9cc6f8242d0efbc3476391302062ff444dd47",
        "set-desired: `uses` is not",
    ),
    (
        "set-desired",
        " # v0.1.317",
        " # v0.1.44",
        "set-desired: the pin's comment names Some(\"0.1.44\"), not v0.1.317",
    ),
    (
        "set-desired",
        " # v0.1.317",
        "",
        "set-desired: the pin's comment names None, not v0.1.317",
    ),
    (
        "set-desired",
        "service: js-agent",
        "service: js-lease-server",
        "set-desired: with.service",
    ),
    // The tag alone: not the version the packages carry.
    (
        "set-desired",
        "version: ${{ needs.check.outputs.package-version }}",
        "version: ${{ inputs.version }}",
        "set-desired: with.version",
    ),
    // Targets dropped: no host is told which package to install.
    (
        "set-desired",
        "      targets: ${{ needs.check.outputs.targets }}\n",
        "",
        "set-desired: with.targets is None",
    ),
    // The inputs of the archive formats, which the reusable workflow still takes: not given.
    (
        "set-desired",
        "      wait: false\n",
        "      wait: false\n      release_index: ${{ needs.check.outputs.release-index }}\n",
        "set-desired: with.release_index is not an input it gives",
    ),
    (
        "set-desired",
        "      wait: false\n",
        "      wait: false\n      package_digest: ${{ needs.check.outputs.targets }}\n",
        "set-desired: with.package_digest is not an input it gives",
    ),
    (
        "set-desired",
        "      wait: false\n",
        "      wait: false\n      max_polls: 180\n",
        "set-desired: with.max_polls is not an input it gives",
    ),
    // The Macs: after the Linux nodes, their own group, the same version and targets, the same pin.
    (
        "set-desired-mac",
        "needs: [check, set-desired]",
        "needs: [check]",
        "set-desired-mac: needs are [\"check\"], not [\"check\", \"set-desired\"]",
    ),
    (
        "set-desired-mac",
        "host_group: mac-build",
        "host_group: all-in-one",
        "set-desired-mac: with.host_group is Some(\"all-in-one\"), not \"mac-build\"",
    ),
    (
        "set-desired-mac",
        "      host_group: mac-build\n",
        "",
        "set-desired-mac: with.host_group is None, not \"mac-build\"",
    ),
    (
        "set-desired-mac",
        "targets: ${{ needs.check.outputs.targets }}",
        "targets: '{}'",
        "set-desired-mac: with.targets is",
    ),
    (
        "set-desired-mac",
        "version: ${{ needs.check.outputs.package-version }}",
        "version: ${{ inputs.version }}",
        "set-desired-mac: with.version",
    ),
    (
        "set-desired-mac",
        "set-desired.yml@30141591c292d8881ce5bc16c9d0b3ba7fcfdca1",
        "set-desired.yml@32a9cc6f8242d0efbc3476391302062ff444dd47",
        "set-desired-mac: `uses` is not",
    ),
    (
        "set-desired-mac",
        " # v0.1.317",
        "",
        "set-desired-mac: the pin's comment names None, not v0.1.317",
    ),
    (
        "set-desired-mac",
        "      contents: read\n",
        "",
        "set-desired-mac: permissions are [\"id-token: write\"], not contents: read and id-token: write",
    ),
    (
        "set-desired-mac",
        "      wait: false\n",
        "      wait: false\n      max_polls: 180\n",
        "set-desired-mac: with.max_polls is not an input it gives",
    ),
    (
        "set-desired-mac",
        "wait: false",
        "wait: true",
        "set-desired-mac: with.wait is Some(\"true\"), not \"false\"",
    ),
    // The call names no host group, or another: it would set each group that lists js-agent.
    (
        "set-desired",
        "      host_group: all-in-one\n",
        "",
        "set-desired: with.host_group is None, not \"all-in-one\"",
    ),
    (
        "set-desired",
        "host_group: all-in-one",
        "host_group: mac",
        "set-desired: with.host_group is Some(\"mac\"), not \"all-in-one\"",
    ),
    (
        "set-desired",
        "hostkeeper_version: 0.1.317",
        "hostkeeper_version: 0.1.241",
        "set-desired: with.hostkeeper_version is Some(\"0.1.241\"), not \"0.1.317\"",
    ),
    (
        "set-desired",
        "hostkeeper_commit: 30141591c292d8881ce5bc16c9d0b3ba7fcfdca1",
        "hostkeeper_commit: 0123456789abcdef0123456789abcdef01234567",
        "set-desired: with.hostkeeper_commit",
    ),
    (
        "set-desired",
        "coordinator_url: ${{ vars.HOSTKEEPER_COORDINATOR_URL }}",
        "coordinator_url: ${{ vars.OTHER_URL }}",
        "set-desired: with.coordinator_url",
    ),
    (
        "set-desired",
        "coordinator_ca: ${{ vars.HOSTKEEPER_COORDINATOR_CA }}",
        "coordinator_ca: ''",
        "set-desired: with.coordinator_ca",
    ),
    (
        "set-desired",
        "audience: jobscheduler-hostkeeper-desired",
        "audience: jobscheduler",
        "set-desired: with.audience",
    ),
    // A wait would hold the group for a staged rollout, and fail when it is longer.
    (
        "set-desired",
        "wait: false",
        "wait: true",
        "set-desired: with.wait is Some(\"true\"), not \"false\"",
    ),
    // Without the join a GitHub-hosted runner does not reach the private listener.
    (
        "set-desired",
        "join_tailnet: ${{ vars.HOSTKEEPER_TAILNET_CLIENT_ID != '' }}",
        "join_tailnet: false",
        "set-desired: with.join_tailnet",
    ),
    (
        "set-desired",
        "tailnet_client_id: ${{ vars.HOSTKEEPER_TAILNET_CLIENT_ID }}",
        "tailnet_client_id: ''",
        "set-desired: with.tailnet_client_id",
    ),
    (
        "set-desired",
        "tailnet_audience: ${{ vars.HOSTKEEPER_TAILNET_AUDIENCE }}",
        "tailnet_audience: ''",
        "set-desired: with.tailnet_audience",
    ),
    (
        "set-desired",
        "tailnet_coordinator: ${{ vars.HOSTKEEPER_TAILNET_COORDINATOR }}",
        "tailnet_coordinator: hk-coordinator",
        "set-desired: with.tailnet_coordinator",
    ),
    // The client's default tag, which the federation of this repository may not issue.
    (
        "set-desired",
        "      tailnet_tag: ${{ vars.HOSTKEEPER_TAILNET_TAG }}\n",
        "",
        "set-desired: with.tailnet_tag is None",
    ),
];

#[test]
fn each_defect_in_hostkeeper_desired_yml_is_named_by_the_lint() {
    // Catches: a pin of hostkeeper-desired.yml that holds nothing, because the file passes
    // whether or not the thing it pins is there.
    let text = text_of(FILE);
    for (job, replace, with, says) in DEFECTS {
        let problems = desired_problems(&in_job(&text, job, replace, with));
        assert!(
            problems.iter().any(|p| p.contains(says)),
            "{job}: {replace:?} -> {with:?} must say {says:?}: {problems:?}"
        );
    }
}

#[test]
fn a_trigger_an_input_or_a_job_other_than_the_dispatch_is_named() {
    // Catches: a push or a schedule setting a version, the completion of another workflow (a
    // `workflow_run` trigger of release or of ci: release.yml's `desired` starts the run, in
    // release order), the dispatch dropped (no rollback by hand), a group that splits the runs or
    // a run cancelled, a second input or none, an input that is not a required string, a job
    // beside the three, a top level that grants something, and a document that does not parse
    // being read as clean.
    let text = text_of(FILE);
    for (replace, with, says) in [
        (
            "on:\n  workflow_dispatch:\n",
            "on:\n  push:\n  workflow_dispatch:\n",
            "triggers are [\"push\", \"workflow_dispatch\"], not workflow_dispatch alone",
        ),
        (
            "on:\n  workflow_dispatch:\n",
            "on:\n  workflow_run:\n    workflows: [release]\n    types: [completed]\n    branches: [main]\n  workflow_dispatch:\n",
            "triggers are [\"workflow_dispatch\", \"workflow_run\"], not workflow_dispatch alone",
        ),
        (
            "  workflow_dispatch:\n    inputs:\n      version:\n        description: \"A released version of js-agent, X.Y.Z (the tag without the v)\"\n        type: string\n        required: true\n",
            "  workflow_run:\n    workflows: [release]\n    types: [completed]\n",
            "triggers are [\"workflow_run\"], not workflow_dispatch alone",
        ),
        (
            "      version:\n        description",
            "      tag:\n        description",
            "inputs are [\"tag\"], not version alone",
        ),
        (
            "      version:\n        description",
            "      note:\n        type: string\n      version:\n        description",
            "inputs are [\"note\", \"version\"], not version alone",
        ),
        (
            "        type: string\n        required: true\n",
            "        type: number\n        required: true\n",
            "input version: type is Some(\"number\"), not string",
        ),
        (
            "        type: string\n        required: true\n",
            "        type: string\n        required: false\n",
            "input version: required is Some(\"false\"), not true",
        ),
        // A group that splits the runs: two would set the version at once.
        (
            "  group: hostkeeper-desired\n",
            "  group: hostkeeper-desired-${{ github.run_id }}\n",
            "concurrency: group is Some(\"hostkeeper-desired-${{ github.run_id }}\"), not",
        ),
        (
            "  cancel-in-progress: false\n",
            "  cancel-in-progress: true\n",
            "concurrency: cancel-in-progress is Some(\"true\"), not false",
        ),
        (
            "permissions: {}\n",
            "permissions:\n  contents: read\n",
            "the top level does not grant `permissions: {}`",
        ),
        (
            "jobs:\n  check:\n",
            "jobs:\n  publish:\n    runs-on: ubuntu-24.04\n  check:\n",
            "jobs are [\"check\", \"publish\", \"set-desired\", \"set-desired-mac\"], not check, set-desired and set-desired-mac",
        ),
    ] {
        assert!(text.contains(replace), "{replace:?} is not in {FILE}");
        let problems = desired_problems(&text.replacen(replace, with, 1));
        assert!(
            problems.iter().any(|p| p.contains(says)),
            "{replace:?} -> {with:?} must say {says:?}: {problems:?}"
        );
    }
    assert!(!desired_problems("permissions: {\njobs: [\n").is_empty());
}

#[test]
fn the_check_runs_the_verb_and_the_call_pins_the_client_hostkeeper_releases_with() {
    // Catches: set-desired starting without the check (it needs it), the check running another
    // command than the verb, and a pin whose commit and release disagree with the inputs that name
    // them.
    let text = text_of(FILE);
    assert!(text.contains(&format!("        run: {CHECK}\n")));
    assert!(text.contains("    needs: [check]\n"));
    assert!(text.contains(&format!(
        "    uses: {SET_DESIRED}@{CLIENT_COMMIT} # v{CLIENT_RELEASE}\n"
    )));
    assert!(text.contains(&format!("      audience: {AUDIENCE}\n")));
    let wanted = with_wanted(HOST_GROUP);
    assert!(text.contains(&format!("      host_group: {HOST_GROUP_MAC}\n")));
    assert!(text.contains(&format!("          PKG_URL: {PKG_URL}\n")));
    assert!(with_wanted(HOST_GROUP_MAC).contains(&("host_group", HOST_GROUP_MAC)));
    assert!(wanted.contains(&("hostkeeper_commit", CLIENT_COMMIT)));
    assert!(wanted.contains(&("hostkeeper_version", CLIENT_RELEASE)));
    assert!(wanted.contains(&("service", "js-agent")));
    assert!(wanted.contains(&("host_group", HOST_GROUP)));
    assert!(
        !wanted
            .iter()
            .any(|(k, _)| matches!(*k, "package_digest" | "release_index"))
    );
}

#[test]
fn no_workflow_reads_another_coordinator_variable_or_calls_the_run() {
    // Catches: a job that sets the version through another coordinator than hostkeeper's (the
    // other coordinator's variable coming back into hostkeeper-desired.yml or into a workflow that
    // is not release.yml's gamma step), a workflow calling hostkeeper-desired.yml with `uses:`,
    // and any `gh workflow run` of it outside release.yml's job `desired`.
    let files = workflows();
    for (file, text) in &files {
        assert_eq!(
            coordinator_problems(file, text),
            Vec::<String>::new(),
            "{file}"
        );
    }
    assert_eq!(caller_problems(&files), Vec::<String>::new());
    // Hostkeeper's two variables, and a variable that is not a coordinator's, are read freely.
    let reads = format!(
        "a: ${{{{ vars.{} }}}} ${{{{ vars.{} }}}}\nb: ${{{{ vars.RELEASE_FREEZE }}}}\n",
        COORDINATOR_VARS[0], COORDINATOR_VARS[1]
    );
    assert_eq!(coordinator_problems("t.yml", &reads), Vec::<String>::new());
    // The gamma step's variable is read in release.yml alone.
    let gamma = "x: 1\n    if: ${{ vars.JS_COORDINATOR_URL != '' }}\n";
    assert_eq!(
        coordinator_problems("release.yml", gamma),
        Vec::<String>::new()
    );
    assert_eq!(
        coordinator_problems("hostkeeper-desired.yml", gamma),
        vec!["hostkeeper-desired.yml: line 2: reads vars.JS_COORDINATOR_URL, not one of hostkeeper's [\"HOSTKEEPER_COORDINATOR_URL\", \"HOSTKEEPER_COORDINATOR_CA\"]".to_owned()]
    );
    assert_eq!(
        coordinator_problems(
            "t.yml",
            "x: 1\n    if: ${{ vars.OTHER_COORDINATOR_URL != '' }}\n"
        ),
        vec![
            "t.yml: line 2: reads vars.OTHER_COORDINATOR_URL, not one of hostkeeper's [\"HOSTKEEPER_COORDINATOR_URL\", \"HOSTKEEPER_COORDINATOR_CA\"]".to_owned(),
        ]
    );
    let release = files
        .iter()
        .find(|(name, _)| name == "release.yml")
        .expect("release.yml")
        .1
        .clone();
    let calling = vec![
        (FILE.to_owned(), text_of(FILE)),
        (
            "release.yml".to_owned(),
            format!("{release}  set-desired:\n    uses: ./.github/workflows/{FILE}\n"),
        ),
    ];
    assert_eq!(
        caller_problems(&calling),
        vec!["release.yml: calls hostkeeper-desired.yml, which no other workflow calls".to_owned()]
    );
    // `gh workflow run hostkeeper-desired.yml` is release.yml's `desired` alone: in another job of
    // release.yml, in another workflow, or in a job after it, it is refused; in a comment it is not.
    let start = "gh workflow run hostkeeper-desired.yml --repo \"$REPO\" --ref main";
    assert!(release.contains(start));
    let elsewhere = release.replacen(
        "      - run: echo \"the publish and deploy jobs of this run have ended\"",
        &format!("      - run: {start}"),
        1,
    );
    assert_ne!(elsewhere, release);
    let moved = vec![("release.yml".to_owned(), elsewhere)];
    let problems = caller_problems(&moved);
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(problems[0].contains("outside the job `desired` of release.yml"));
    let other = vec![(
        "ci.yml".to_owned(),
        format!("jobs:\n  desired:\n    steps:\n      - run: {start}\n"),
    )];
    assert_eq!(caller_problems(&other).len(), 1);
    let comment = vec![(
        "ci.yml".to_owned(),
        format!("jobs:\n  x:\n    steps:\n      # {start}\n"),
    )];
    assert_eq!(caller_problems(&comment), Vec::<String>::new());
    // A workflow that is not YAML is not a caller (the YAML reader's own test names it).
    assert_eq!(
        caller_problems(&[("t.yml".to_owned(), "a: b: c\n".to_owned())]),
        Vec::<String>::new()
    );
}
