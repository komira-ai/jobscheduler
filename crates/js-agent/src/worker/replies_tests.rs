use js_agent_wire::Taint;

use super::describe;
use crate::lifecycle::{Event, Finding};

fn taint(reason: &str, check: Option<&str>, run: Option<&str>) -> Taint {
    Taint {
        reason: reason.to_owned(),
        run_id: run.map(str::to_owned),
        check: check.map(str::to_owned),
    }
}

fn tainted(taint: Taint, detail: Option<&str>) -> Event {
    Event::Tainted {
        taint,
        detail: detail.map(str::to_owned),
    }
}

/// Asserts the line of a new taint for each combination of what the taint and the finding
/// have: the reason alone, with the check, with the run, with both, and with a detail after
/// them. It catches a line that is the debug text of the event, one that leaves a part out
/// or keeps a part that is not there.
#[test]
fn a_new_taint_is_one_line_with_the_reason_the_check_the_run_and_the_detail() {
    let events = [
        tainted(taint("check-owed", None, None), None),
        tainted(taint("cleanup-failed", Some("scratch-failed"), None), None),
        tainted(taint("host-job", None, Some("r1")), None),
        tainted(
            taint("residue", Some("process-as-job-user"), Some("r1")),
            None,
        ),
        tainted(
            taint("residue", Some("process-as-job-user"), Some("r1")),
            Some("pid 41 (sleep, state S) is running after job r1"),
        ),
        tainted(taint("unchecked", None, None), Some("no /proc")),
    ];
    assert_eq!(
        describe(&events),
        [
            "tainted: check-owed",
            "tainted: cleanup-failed (check scratch-failed)",
            "tainted: host-job (run r1)",
            "tainted: residue (check process-as-job-user, run r1)",
            "tainted: residue (check process-as-job-user, run r1): pid 41 (sleep, state S) \
             is running after job r1",
            "tainted: unchecked: no /proc",
        ]
    );
}

/// Asserts that the detail of a taint is one line whatever the host called a process or a
/// mount: each control character, a line break included, is a space. It catches a log line
/// that a name with a newline splits in two.
#[test]
fn the_detail_of_a_taint_is_one_line() {
    let event = tainted(taint("residue", None, None), Some("comm a\nb\r\x1b[0m\tz"));
    assert_eq!(describe(&[event]), ["tainted: residue: comm a b  [0m z"]);
}

/// Asserts that every other event is still its debug text after `job: `. It catches a change
/// of the lines the log and the tests already read.
#[test]
fn another_event_is_its_debug_text() {
    let finding = Finding {
        run_id: Some("r1".to_owned()),
        code: "kernel-module-changed",
        message: "loaded zram".to_owned(),
    };
    let lines = describe(&[Event::Finding(finding)]);
    assert_eq!(
        lines,
        [
            "job: Finding(Finding { run_id: Some(\"r1\"), code: \"kernel-module-changed\", \
          message: \"loaded zram\" })"
        ]
    );
}
