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

/// A job's booking of a quarter core and 16 GiB, with its sizing when `sized`.
fn quarter_core(sized: bool) -> crate::lifecycle::JobBooking {
    crate::lifecycle::JobBooking {
        cpu_millis: 250,
        memory_bytes: 16 << 30,
        sizing: sized.then(|| js_agent_wire::StartSizing {
            class: Some("root.crates.js-sched.js-sched".to_owned()),
            cpu: js_agent_wire::BookedFrom::Class,
            memory: js_agent_wire::BookedFrom::Cold,
        }),
    }
}

/// Asserts the whole line of a job's result, word for word: the booking, the size class and
/// the sources of the CPU and the memory, what the group used (its swap peak among it), and its
/// throttle counts with the throttled share in thousandths. It catches a result line without the booking or the
/// sizing beside the usage (the roll-out compares them per class), a share that is not the
/// throttled periods over all periods, and words out of their fixed order.
#[test]
fn a_result_is_one_line_of_its_booking_its_sizing_its_usage_and_its_throttled_share() {
    let usage = js_agent_wire::JobUsage {
        peak_memory_bytes: 3 << 30,
        cpu_usage_usec: 12_300_000,
        cpu_throttle: Some(js_agent_wire::CpuThrottle {
            periods: 5597,
            throttled_periods: 5585,
        }),
        cpu_pressure_some_usec: Some(9_000_000),
        cpu_peak: Some(js_agent_wire::CpuPeak {
            millicores: 20_000,
            window_usec: 5_000_000,
        }),
        peak_swap_bytes: Some(1 << 29),
    };
    let event = Event::ResultReady {
        run_id: "k1-344439-0".to_owned(),
        booked: quarter_core(true),
        usage: Some(usage),
    };
    assert_eq!(
        describe(&[event]),
        [
            "job: result run=k1-344439-0 cpu_millis=250 memory_bytes=17179869184 \
             class=root.crates.js-sched.js-sched cpu_from=class memory_from=cold \
             cpu_usage_usec=12300000 peak_memory_bytes=3221225472 cpu_periods=5597 \
             cpu_throttled_periods=5585 throttled_permille=997 cpu_pressure_some_usec=9000000 \
             cpu_peak_millicores=20000 peak_swap_bytes=536870912"
        ]
    );
}

/// Asserts that what is not known is `-` and never a zero: a result with no usage (a job that
/// never ran) and no sizing (a run start, an older server), and a usage without throttle
/// counts. It catches an unknown written as a measured 0, which would read as a job its host
/// never throttled.
#[test]
fn what_a_result_does_not_know_is_a_dash() {
    let unknown = Event::ResultReady {
        run_id: "r1".to_owned(),
        booked: quarter_core(false),
        usage: None,
    };
    let unthrottled = Event::ResultReady {
        run_id: "r2".to_owned(),
        booked: quarter_core(false),
        usage: Some(js_agent_wire::JobUsage {
            peak_memory_bytes: 1,
            cpu_usage_usec: 2,
            cpu_throttle: None,
            cpu_pressure_some_usec: None,
            cpu_peak: None,
            peak_swap_bytes: None,
        }),
    };
    assert_eq!(
        describe(&[unknown, unthrottled]),
        [
            "job: result run=r1 cpu_millis=250 memory_bytes=17179869184 class=- cpu_from=- \
             memory_from=- cpu_usage_usec=- peak_memory_bytes=- cpu_periods=- \
             cpu_throttled_periods=- throttled_permille=- cpu_pressure_some_usec=- cpu_peak_millicores=- \
             peak_swap_bytes=-",
            "job: result run=r2 cpu_millis=250 memory_bytes=17179869184 class=- cpu_from=- \
             memory_from=- cpu_usage_usec=2 peak_memory_bytes=1 cpu_periods=- \
             cpu_throttled_periods=- throttled_permille=- cpu_pressure_some_usec=- cpu_peak_millicores=- \
             peak_swap_bytes=-",
        ]
    );
}

/// Asserts that the line of a start carries its booking and its sizing beside its timeline.
/// It catches a start line that says when the job started but not what it booked or where
/// the booking came from.
#[test]
fn a_start_is_logged_with_its_booking_and_its_sizing() {
    let event = Event::Started {
        run_id: "r1".to_owned(),
        timeline: crate::lifecycle::StartTimeline::default(),
        booked: quarter_core(true),
    };
    assert_eq!(
        describe(&[event]),
        [
            "job: Started { run_id: \"r1\", timeline: StartTimeline { received_ms: 0, \
             placed_at_ms: None, received_at_ms: None, steps: [], runtime: [] }, booked: \
             JobBooking { cpu_millis: 250, memory_bytes: 17179869184, sizing: Some(StartSizing \
             { class: Some(\"root.crates.js-sched.js-sched\"), cpu: Class, memory: Cold }) } }"
        ]
    );
}
