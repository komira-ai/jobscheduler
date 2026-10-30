//! The cases that call the runtime directly.

use std::time::Duration;

use js_agent::lifecycle::{Exit, Status, label};
use js_agent_wire::LogTail;
use js_agent_wire::limits::MAX_LOG_STREAM_LEN;

use super::{
    ERR_LINE, FLOOD_END, LONG, OUT_LINE, Program, Subject, begin, clean_up, exited, listed, poll,
    remove, spec_of, stop,
};

/// Proves the whole life of one container: it is created and started, polls as running,
/// is listed with the run's label and the agent's, stops, polls as exited (not by the
/// memory controller), is removed, then polls as gone and is no longer listed, and a second
/// remove and a stop of what is gone succeed. Catches a runtime that cannot tell a stopped
/// container from a running one, drops the agent label that scopes the list, or fails on a
/// repeat of a call the lifecycle makes again after an error.
pub fn a_container_goes_through_create_start_poll_stop_remove(s: &mut dyn Subject) {
    let id = begin(s, "r1", Program::Hold);
    assert_eq!(poll(s, &id), Status::Running);
    let found = listed(s);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].id, id);
    assert_eq!(
        found[0].labels.get(label::RUN).map(String::as_str),
        Some("r1")
    );
    assert_eq!(found[0].labels.get(label::AGENT), Some(&s.host_id()));
    assert!(!found[0].never_started);
    stop(s, &id, LONG);
    s.settled(&id);
    let exit = exited(poll(s, &id));
    assert_ne!(exit.code, 0);
    assert!(!exit.oom_killed);
    remove(s, &id);
    assert_eq!(poll(s, &id), Status::Gone);
    assert!(listed(s).is_empty());
    remove(s, &id);
    stop(s, &id, LONG);
}

/// Proves that the exit status of the program is the container's, for zero and for a
/// code, and that it is reported again by the next poll. Catches an exit code lost, mapped
/// or consumed by the first poll.
pub fn an_exit_code_is_reported(s: &mut dyn Subject) {
    for (run, code) in [("r0", 0), ("r3", 3)] {
        let id = begin(s, run, Program::Exit(code));
        s.settled(&id);
        let want = Status::Exited(Exit {
            code,
            oom_killed: false,
        });
        assert_eq!(poll(s, &id), want);
        assert_eq!(poll(s, &id), want);
        remove(s, &id);
    }
}

/// Proves that a program the kernel kills at the run's own memory booking exits 137 with
/// the memory flag set, the answer the server retries with more memory on. Catches a
/// runtime that reports the kill as an ordinary exit.
pub fn a_kill_at_the_runs_own_memory_limit_is_flagged(s: &mut dyn Subject) {
    let id = begin(s, "r1", Program::Hog { own_limit: true });
    s.settled(&id);
    let want = Status::Exited(Exit {
        code: 137,
        oom_killed: true,
    });
    assert_eq!(poll(s, &id), want);
    clean_up(s);
}

/// Proves the flag is distinct: a program killed by a limit above the lease (the node ran
/// short of memory) exits 137 with the flag clear, because the booking was not the cause
/// and the server retries with the same booking. Catches a runtime that flags every kill.
pub fn a_kill_by_a_node_short_of_memory_is_not_flagged(s: &mut dyn Subject) {
    let id = begin(s, "r1", Program::Hog { own_limit: false });
    s.settled(&id);
    let want = Status::Exited(Exit {
        code: 137,
        oom_killed: false,
    });
    assert_eq!(poll(s, &id), want);
    clean_up(s);
}

/// Proves SIGTERM is sent first: a program that handles it and exits 7 is stopped with a
/// grace no case waits out, and exits 7, not 137. Catches a stop that kills at once.
pub fn sigterm_reaches_a_stopped_job_before_any_kill(s: &mut dyn Subject) {
    let id = begin(s, "r1", Program::OnTerm(7));
    stop(s, &id, LONG);
    s.settled(&id);
    let want = Status::Exited(Exit {
        code: 7,
        oom_killed: false,
    });
    assert_eq!(poll(s, &id), want);
    clean_up(s);
}

/// Proves the kill follows when the grace is spent: a program that ignores SIGTERM is
/// stopped with a grace of zero and exits 137. Catches a stop that returns with the job
/// still running.
pub fn a_job_that_ignores_sigterm_is_killed_when_the_grace_is_spent(s: &mut dyn Subject) {
    let id = begin(s, "r1", Program::IgnoreTerm);
    stop(s, &id, Duration::ZERO);
    s.settled(&id);
    let want = Status::Exited(Exit {
        code: 137,
        oom_killed: false,
    });
    assert_eq!(poll(s, &id), want);
    clean_up(s);
}

/// Proves a container that was created and never started is listed marked
/// `never_started` and listing removes nothing, polls as running (the documented report of
/// a created container), and stops and removes. Catches a runtime that lists it as a job
/// that ran, or removes it while listing.
pub fn a_container_never_started_is_listed_as_such(s: &mut dyn Subject) {
    s.prepared();
    let spec = spec_of(s, "r1", Program::Hold);
    let id = s.unstarted(&spec);
    let found = listed(s);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].id, id);
    assert!(found[0].never_started);
    assert_eq!(listed(s).len(), 1);
    assert_eq!(poll(s, &id), Status::Running);
    stop(s, &id, Duration::ZERO);
    remove(s, &id);
    assert!(listed(s).is_empty());
    assert_eq!(poll(s, &id), Status::Gone);
}

/// Proves a start that fails leaves no container behind, and the run id can be used
/// again. Catches a failed start that is still listed (and so adopted or booked).
pub fn a_start_that_fails_leaves_no_container(s: &mut dyn Subject) {
    s.prepared();
    let mut spec = spec_of(s, "r1", Program::Hold);
    s.spoil(&mut spec);
    let failed = s.runtime().borrow_mut().create_and_start(&spec);
    assert!(failed.is_err());
    assert!(listed(s).is_empty());
    begin(s, "r1", Program::Hold);
    clean_up(s);
}

/// Proves a run id that already has a container is refused, and the first container is
/// untouched. Catches a second container made under one name, which the removal of either
/// would take away from the other.
pub fn a_run_id_in_use_is_refused(s: &mut dyn Subject) {
    let id = begin(s, "r1", Program::Hold);
    let again = spec_of(s, "r1", Program::Hold);
    let refused = s.runtime().borrow_mut().create_and_start(&again);
    assert!(refused.is_err());
    assert_eq!(poll(s, &id), Status::Running);
    assert_eq!(listed(s).len(), 1);
    clean_up(s);
}

/// Proves a running container names its control group as an absolute path, and a removed
/// one names none. Catches usage sampling that has no cgroup to read.
pub fn a_running_container_names_its_cgroup(s: &mut dyn Subject) {
    let id = begin(s, "r1", Program::Hold);
    let cgroup = s.runtime().borrow_mut().cgroup(&id);
    assert!(cgroup.expect("a cgroup").is_absolute());
    clean_up(s);
    assert_eq!(s.runtime().borrow_mut().cgroup(&id), None);
}

/// Proves a runtime without GPUs says so and refuses a start that names one, creating
/// nothing. Catches a runtime that starts a job without the device it was booked.
pub fn a_runtime_without_gpus_says_so_and_refuses_one(s: &mut dyn Subject) {
    assert!(!s.runtime().borrow().supports_gpus());
    s.prepared();
    let mut spec = spec_of(s, "r1", Program::Hold);
    spec.gpu_indices = vec![0];
    let refused = s.runtime().borrow_mut().create_and_start(&spec);
    assert!(refused.is_err());
    assert!(listed(s).is_empty());
}

/// The tail the runtime keeps of the container `id`, read as the lifecycle does.
fn tail_of(s: &dyn Subject, id: &js_agent::lifecycle::RunHandle) -> Option<LogTail> {
    s.runtime()
        .borrow_mut()
        .log_tail(id)
        .expect("read the tail")
}

/// Proves the tail of a run that ends on its own reaches the result: standard output and
/// standard error each hold what the program wrote to it and nothing of the other, the
/// tail is not marked truncated, and a second read answers the same (the lifecycle reads
/// once, but a retried step must not lose it). Catches a runtime that keeps no tail, merges
/// the two streams, or empties the tail when the end is read.
pub fn the_tail_of_a_run_that_ends_normally_reaches_the_result(s: &mut dyn Subject) {
    let id = begin(s, "r1", Program::Say { hold: false });
    s.settled(&id);
    assert_eq!(exited(poll(s, &id)).code, 0);
    let want = LogTail {
        stdout: OUT_LINE.to_owned(),
        stderr: ERR_LINE.to_owned(),
        truncated: false,
    };
    assert_eq!(tail_of(s, &id), Some(want.clone()));
    assert_eq!(tail_of(s, &id), Some(want));
    clean_up(s);
}

/// Proves the tail of a run that was killed (it ignores SIGTERM and the grace is zero)
/// holds what it wrote before the kill, with the exit 137. Catches a runtime whose kill
/// path loses the output, which is what the user needs most from a job that was killed.
pub fn the_tail_of_a_run_that_is_killed_reaches_the_result(s: &mut dyn Subject) {
    let id = begin(s, "r1", Program::Say { hold: true });
    stop(s, &id, Duration::ZERO);
    s.settled(&id);
    assert_eq!(exited(poll(s, &id)).code, 137);
    let tail = tail_of(s, &id).expect("a tail");
    assert_eq!(tail.stdout, OUT_LINE);
    assert_eq!(tail.stderr, ERR_LINE);
    clean_up(s);
}

/// Proves a container created and never started has no output: its tail is empty (or the
/// runtime keeps none), never another run's and never an error. Catches a runtime that
/// fails the read for a container that did not run, which the lifecycle reports as a
/// failed log.
pub fn a_run_never_started_has_an_empty_tail(s: &mut dyn Subject) {
    s.prepared();
    let spec = spec_of(s, "r1", Program::Say { hold: false });
    let id = s.unstarted(&spec);
    let tail = tail_of(s, &id);
    assert_eq!(tail.unwrap_or_default(), LogTail::default());
    stop(s, &id, Duration::ZERO);
    remove(s, &id);
}

/// Proves output far past what a tail holds neither blocks the job nor grows the tail: the
/// program ends with exit 0, each stream is within the wire's limit, the tail is marked
/// truncated and ends with the program's last line. Catches a job stalled on a full pipe
/// (the wait for its end would not return), a tail past the limit, and one that keeps the
/// start of the output instead of its end.
pub fn a_tail_is_bounded_and_says_it_dropped_bytes(s: &mut dyn Subject) {
    let id = begin(s, "r1", Program::Flood);
    s.settled(&id);
    assert_eq!(exited(poll(s, &id)).code, 0);
    let tail = tail_of(s, &id).expect("a tail");
    assert!(tail.stdout.len() <= MAX_LOG_STREAM_LEN);
    assert!(tail.stdout.ends_with(FLOOD_END));
    assert!(tail.truncated);
    assert_eq!(tail.stderr, "");
    clean_up(s);
}
