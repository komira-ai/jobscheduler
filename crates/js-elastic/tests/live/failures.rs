//! Every way a spend run goes wrong still ends in the cleanup.

use std::collections::{BTreeMap, BTreeSet};

use js_elastic::HostStatus;

use crate::mem::*;

/// Asserts that a capability table that misses an offer's architecture fails the
/// read-only case, which makes a dry run exit 1 and stops a spend run before it launches.
#[test]
fn a_failed_read_only_check_fails_the_dry_run_and_stops_the_spend() {
    let state = state(one_offer());
    state.lock().unwrap().arches.clear();
    let cloud = MemCloud::new(&state);
    let (status, lines) = go(&args_of(&[]), &cloud);
    assert_eq!(status, 1);
    let case = "capabilities_cover_every_offer_the_catalog_returns";
    let failed = lines.iter().find(|l| l.starts_with("FAIL ")).unwrap();
    assert!(
        failed.starts_with(&format!("FAIL {case}: {case}: ")),
        "{failed}"
    );
    assert_eq!(
        lines.last().map(String::as_str),
        Some("dry run: nothing was launched")
    );
    let (status, lines) = go(&spend_args(&printed_hash(&lines)), &cloud);
    assert_eq!(status, 1);
    assert_eq!(
        lines.last().map(String::as_str),
        Some("a read-only check failed: nothing was launched")
    );
    assert!(state.lock().unwrap().requests.is_empty());
}

/// Asserts that a list that fails in the dry run is reported and exits 1.
#[test]
fn a_failed_list_in_the_dry_run_exits_1() {
    let state = state(one_offer());
    let mut cloud = MemCloud::new(&state);
    cloud.fail_lists = BTreeMap::from([(0, BTreeSet::from([0]))]);
    let (status, lines) = go(&args_of(&[]), &cloud);
    assert_eq!(status, 1);
    at(
        &lines,
        "FAIL the list of existing machines: the list was refused",
    );
}

/// Asserts that a list that panics in the dry run is reported as a failure, not let out of
/// the run: exit 1, and a spend run with that plan's hash launches nothing.
#[test]
fn a_panicking_list_in_the_dry_run_exits_1() {
    let state = state(one_offer());
    let mut cloud = MemCloud::new(&state);
    cloud.panic_lists = BTreeMap::from([(0, BTreeSet::from([0]))]);
    let (status, lines) = go(&args_of(&[]), &cloud);
    assert_eq!(status, 1);
    at(
        &lines,
        "FAIL the list of existing machines panicked: the list 0 panicked",
    );
    at(&lines, "dry run: nothing was launched");
    let mut cloud = MemCloud::new(&state);
    cloud.panic_lists = BTreeMap::from([(0, BTreeSet::from([0]))]);
    let (status, lines) = go(&spend_args(&printed_hash(&lines)), &cloud);
    assert_eq!(status, 1);
    at(&lines, "a read-only check failed: nothing was launched");
    assert!(state.lock().unwrap().requests.is_empty());
}

/// Asserts that a panic in a case is printed as FAIL with its text, whether the panic
/// carried a formatted string, a literal, or something that is not text.
#[test]
fn a_panic_is_a_failure_with_its_text() {
    let wanted = [
        (Boom::Formatted, "capabilities broke: 0"),
        (Boom::Literal, "capabilities broke"),
        (Boom::Neither, "a panic that carries no text"),
    ];
    for (boom, text) in wanted {
        let state = state(one_offer());
        state.lock().unwrap().caps_panic = Some(boom);
        let (status, lines) = go(&args_of(&[]), &MemCloud::new(&state));
        assert_eq!(status, 1);
        at(
            &lines,
            &format!("FAIL capabilities_cover_every_offer_the_catalog_returns: {text}"),
        );
    }
}

/// Asserts that a case that fails prints FAIL with its assertion, that the other cases
/// still run, that the cleanup still releases every machine, and that the exit is 1: a
/// provider that launches on every acquire breaks the idempotency case.
#[test]
fn a_failing_case_is_printed_and_the_rest_and_the_cleanup_still_run() {
    let state = state(one_offer());
    state.lock().unwrap().launch_every_time = true;
    let (status, lines) = spend(&state, &MemCloud::new(&state));
    assert_eq!(status, 1);
    let case = "acquire_is_idempotent_on_the_acquire_id";
    at(
        &lines,
        &format!(
            "FAIL {case}: assertion `left == right` failed: {case}: a retry answers the machine of the first acquire"
        ),
    );
    at(&lines, "PASS accepted_means_launched_not_ready");
    at(&lines, "PASS list_and_observe_see_the_machine");
    at(&lines, "cases: 2 passed, 1 failed");
    at(&lines, "cleanup: 4 machine(s) listed, none left running");
    assert_eq!(statuses(&state), vec![HostStatus::Terminating; 4]);
}

/// Asserts that each way the live case can find the machine wrong fails it by name:
/// absent from the list, listed without a name or with another's, read as absent, a
/// release that refuses, and a release that answers terminating while the machine runs.
#[test]
fn the_live_case_fails_on_a_provider_that_lists_reads_or_releases_wrongly() {
    let case = "list_and_observe_see_the_machine";
    let prefix = format!("FAIL {case}: {case}: ");
    type Break = Box<dyn Fn(&mut State)>;
    let wrong: Vec<(Break, &str)> = vec![
        (
            Box::new(|s| s.omit_from_list = true),
            "the list [] does not hold the machine",
        ),
        (
            Box::new(|s| s.named = Named::Missing),
            "the listed name None does not start with the prefix js-",
        ),
        (
            Box::new(|s| s.named = Named::Wrong),
            "the listed name Some(\"other\") does not start with the prefix js-",
        ),
        (
            Box::new(|s| s.observe_absent = true),
            "a read of the launched machine answers Absent",
        ),
        (
            Box::new(|s| s.release = Release::Refused),
            "a release of the launched machine answers Refused(Foreign)",
        ),
        (
            Box::new(|s| s.release = Release::Ignored),
            "after Terminating(Terminating) a read answers Present(Booting)",
        ),
    ];
    for (break_it, message) in wrong {
        let state = state(one_offer());
        break_it(&mut state.lock().unwrap());
        let (status, lines) = spend(&state, &MemCloud::new(&state));
        assert_eq!(status, 1, "{message}");
        let failed = lines
            .iter()
            .find(|l| l.starts_with(&format!("FAIL {case}")))
            .unwrap();
        assert!(
            failed.starts_with(&format!("{prefix}{message}")),
            "{failed}"
        );
    }
}

/// Asserts that a machine the cleanup cannot end is a LEFT OVER, named with its status and
/// what to do, and the exit is 1: a release that answers terminating and does nothing.
#[test]
fn a_machine_that_is_not_terminating_after_the_cleanup_is_left_over() {
    let state = state(one_offer());
    state.lock().unwrap().release = Release::Ignored;
    let (status, lines) = spend(&state, &MemCloud::new(&state));
    assert_eq!(status, 1);
    let find = "terminate it in the provider console; the label validation-run=run-1 finds it";
    at(
        &lines,
        &format!(
            "LEFT OVER: machine i-00000000000000001 is still Booting and may be billing; {find}"
        ),
    );
    assert!(!lines.iter().any(|l| l.contains("none left running")));
    assert!(statuses(&state).iter().all(|s| *s == HostStatus::Booting));
}

/// Asserts the outcome of a list that fails at each point of the cleanup, and of a
/// provider that cannot be built for it: a loud message with the label to look for, and
/// exit 1. The cleanup builds the fifth provider of the run: the run's own, then three
/// for the cases, then its own.
#[test]
fn a_cleanup_that_cannot_look_exits_1_loudly() {
    let find = "Machines of this run may be running: terminate it in the provider console; \
                the label validation-run=run-1 finds it";
    let list_refused = "CLEANUP FAILED: the list failed: the list was refused";
    let after_refused = "CLEANUP FAILED: the list after the release failed: the list was refused";
    let build_refused =
        "CLEANUP FAILED: cannot build a provider to look for machines: build 4 was refused";
    for (message, lists, builds) in [
        (list_refused, BTreeSet::from([0]), BTreeSet::new()),
        (after_refused, BTreeSet::from([1]), BTreeSet::new()),
        (build_refused, BTreeSet::new(), BTreeSet::from([4])),
    ] {
        let state = state(one_offer());
        let mut cloud = MemCloud::new(&state);
        cloud.fail_lists = BTreeMap::from([(4, lists)]);
        cloud.fail_builds = builds;
        let (status, lines) = spend(&state, &cloud);
        assert_eq!(status, 1, "{message}");
        let last = lines.len() - 2;
        assert_eq!(lines[last], message);
        assert_eq!(lines[last + 1], find);
    }
}

/// Asserts that a provider that cannot be built, or whose list fails, inside the room
/// making of a case fails that case with the cause and the run goes on to the cleanup.
/// The run's own provider is the first built (0), the first case's the second (1).
#[test]
fn a_case_fails_when_room_cannot_be_made() {
    let case = "acquire_is_idempotent_on_the_acquire_id";
    let refused = "the list was refused";
    let tries = [
        (
            BTreeSet::from([1]),
            BTreeMap::new(),
            "make_room: cannot build a provider: build 1 was refused".to_owned(),
        ),
        (
            BTreeSet::new(),
            BTreeMap::from([(0, BTreeSet::from([1]))]),
            format!("make_room: the list failed: {refused}"),
        ),
        (
            BTreeSet::new(),
            BTreeMap::from([(0, BTreeSet::from([2]))]),
            format!("make_room: the list after the release failed: {refused}"),
        ),
        (
            BTreeSet::new(),
            BTreeMap::from([(1, BTreeSet::from([0]))]),
            format!("launches: the list failed: {refused}"),
        ),
    ];
    for (builds, lists, message) in tries {
        let state = state(one_offer());
        let mut cloud = MemCloud::new(&state);
        cloud.fail_builds = builds;
        cloud.fail_lists = lists;
        let (status, lines) = spend(&state, &cloud);
        assert_eq!(status, 1, "{message}");
        at(&lines, &format!("FAIL {case}: {message}"));
        at(&lines, "PASS accepted_means_launched_not_ready");
    }
}

/// Asserts that the third case fails with the list's error when the list of its provider
/// (the fourth built) fails after the launch.
#[test]
fn the_live_case_fails_when_the_list_fails() {
    let state = state(one_offer());
    let mut cloud = MemCloud::new(&state);
    cloud.fail_lists = BTreeMap::from([(3, BTreeSet::from([0]))]);
    let (status, lines) = spend(&state, &cloud);
    assert_eq!(status, 1);
    let case = "list_and_observe_see_the_machine";
    let error = "Err(ProviderError(\"the list was refused\"))";
    at(
        &lines,
        &format!("FAIL {case}: {case}: the list answers {error}"),
    );
}

/// Asserts that a provider whose list panics during the cleanup, before or after the
/// release, does not unwind out of the run: the cases have run and passed, a loud message
/// names the label to look for in the console, and the exit is 1. The cleanup builds the
/// fifth provider of the run.
#[test]
fn a_panic_in_the_cleanup_is_reported_loudly_and_exits_1() {
    for call in [0, 1] {
        let state = state(one_offer());
        let mut cloud = MemCloud::new(&state);
        cloud.panic_lists = BTreeMap::from([(4, BTreeSet::from([call]))]);
        let (status, lines) = spend(&state, &cloud);
        assert_eq!(status, 1, "list {call}");
        at(&lines, "cases: 3 passed, 0 failed");
        assert_eq!(
            lines.last().map(String::as_str),
            Some(
                format!(
                    "CLEANUP FAILED: cleanup panicked: the list {call} panicked; check the \
                     console for machines labelled validation-run=run-1"
                )
                .as_str()
            ),
            "{lines:#?}"
        );
        assert!(!lines.iter().any(|l| l.contains("none left running")));
    }
}
