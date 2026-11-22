//! The log line of a live host the pass skips for its taint ([`TaintSkips`]): when it is said
//! and when it is said again. Every line is captured by a subscriber of the test's thread.

use js_agent_wire::Taint;
use js_types::HostId;

use super::TaintSkips;
use crate::api::rig::{Lines, process_sink};

/// What the line says, before the fields.
const MESSAGE: &str = "a live host is skipped by placement for the taint the server holds";

fn host(name: &str) -> HostId {
    HostId::new(name).unwrap()
}

/// A residue of `check`, left by the run `run`.
fn residue(check: &str, run: &str) -> Taint {
    Taint {
        reason: "residue".to_owned(),
        run_id: Some(run.to_owned()),
        check: Some(check.to_owned()),
    }
}

/// Notes `pairs` and returns the lines it logged: the level and the fields of each.
fn note(skips: &TaintSkips, lines: &Lines, pairs: &[(&HostId, &Taint)]) -> Vec<String> {
    skips.note(pairs.iter().copied());
    let text = lines.take();
    let found = text.lines().filter_map(|line| line.split_once(MESSAGE));
    found
        .map(|(level, fields)| format!("{}{fields}", level.contains("WARN")))
        .collect()
}

/// The line of `name` for `taint`'s fields, as the warning logs it.
fn said(name: &str, fields: &str) -> String {
    format!("true host={name} {fields}")
}

/// Asserts that a host's taint is logged once however many times it is noted, as a warning
/// with the host, reason, check and run id; that a host that joins the set is logged and the
/// ones already logged are not; that a host that leaves the set and comes back with the same
/// taint is logged again; and that a changed taint of a host still in the set is logged
/// again, a field of the taint at a time. It catches a line per pass, which drowns the
/// journal, and a host that is never said again after its taint changed or was cleared and
/// made again.
#[test]
fn a_taint_is_logged_once_and_again_when_it_changes_or_comes_back() {
    process_sink();
    let lines = Lines::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(lines.clone())
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let skips = TaintSkips::default();
    let (a, b) = (host("host-a"), host("host-b"));
    let first = residue("kernel-module-changed", "r1");
    let fields = "reason=\"residue\" check=\"kernel-module-changed\" run_id=\"r1\"";

    assert_eq!(note(&skips, &lines, &[]), Vec::<String>::new());
    assert_eq!(
        note(&skips, &lines, &[(&a, &first)]),
        [said("host-a", fields)]
    );
    assert_eq!(note(&skips, &lines, &[(&a, &first)]), Vec::<String>::new());
    assert_eq!(
        note(&skips, &lines, &[(&a, &first), (&b, &first)]),
        [said("host-b", fields)]
    );
    assert_eq!(
        note(&skips, &lines, &[(&a, &first), (&b, &first)]),
        Vec::<String>::new()
    );

    assert_eq!(note(&skips, &lines, &[(&b, &first)]), Vec::<String>::new());
    assert_eq!(
        note(&skips, &lines, &[(&a, &first), (&b, &first)]),
        [said("host-a", fields)]
    );

    let changes = [
        residue("kernel-module-changed", "r2"),
        residue("process-as-job-user", "r2"),
        Taint {
            reason: "host-job".to_owned(),
            run_id: None,
            check: None,
        },
    ];
    let expected = [
        "reason=\"residue\" check=\"kernel-module-changed\" run_id=\"r2\"",
        "reason=\"residue\" check=\"process-as-job-user\" run_id=\"r2\"",
        "reason=\"host-job\"",
    ];
    for (taint, fields) in changes.iter().zip(expected) {
        let logged = note(&skips, &lines, &[(&a, taint), (&b, &first)]);
        assert_eq!(logged, [said("host-a", fields)]);
    }
}
