//! What counts as news for the server, and that the bell rings for it and for nothing else.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use js_agent_wire::{HeldJob, HeldState, JobResult, Outcome, Taint};

use super::*;
use crate::worker::LeaseAnswers;

fn job(id: &str, state: HeldState) -> HeldJob {
    HeldJob {
        job_id: id.to_owned(),
        state,
        result: None,
    }
}

fn job_result(id: &str) -> JobResult {
    JobResult {
        job_id: id.to_owned(),
        allocation_id: "alloc".to_owned(),
        outcome: Outcome::Succeeded,
        exit_code: Some(0),
        detail: None,
        log_tail: None,
        logs: None,
        input_failure: None,
        usage: None,
        cpu_series: None,
        memory_series: None,
    }
}

fn taint(reason: &str) -> Taint {
    Taint {
        reason: reason.to_owned(),
        run_id: None,
        check: None,
    }
}

fn with_jobs(jobs: Vec<HeldJob>) -> Snapshot {
    Snapshot {
        jobs,
        ..Snapshot::default()
    }
}

/// Proves a job that appears already running, or that changes state from starting to
/// running, is news. Catches a listing the server would learn of only an interval later.
#[test]
fn a_job_that_runs_is_news() {
    let none = Snapshot::default();
    let running = with_jobs(vec![job("j1", HeldState::Running)]);
    assert!(holds_news(&none, &running));
    let starting = with_jobs(vec![job("j1", HeldState::Starting)]);
    assert!(holds_news(&starting, &running));
}

/// Proves a job that is listed for the first time as starting is news, and that a job listed
/// as starting before and still starting is not. Catches a start taken that the server hears
/// of only at the interval (no heartbeat of the host held for a placement meanwhile), and a
/// ring at every publish while the start stands.
#[test]
fn a_job_just_taken_as_starting_is_news_and_one_still_starting_is_not() {
    let none = Snapshot::default();
    let starting = with_jobs(vec![job("j1", HeldState::Starting)]);
    assert!(holds_news(&none, &starting));
    assert!(!holds_news(&starting, &starting));
}

/// Proves a job already listed that goes back to starting counts as a change, and so is news.
/// Catches a change of state that is not told.
#[test]
fn a_job_listed_before_that_changes_to_starting_is_news() {
    let before = with_jobs(vec![job("j1", HeldState::Running)]);
    let now = with_jobs(vec![job("j1", HeldState::Starting)]);
    assert!(holds_news(&before, &now));
}

/// Proves a job that exits is news, and a job that is gone from the listing is not.
/// Catches ringing for an entry the server itself acknowledged away.
#[test]
fn a_job_that_ended_is_news_and_a_job_removed_is_not() {
    let running = with_jobs(vec![job("j1", HeldState::Running)]);
    let exited = with_jobs(vec![job("j1", HeldState::ExitedUnacked)]);
    assert!(holds_news(&running, &exited));
    assert!(!holds_news(&exited, &Snapshot::default()));
}

/// Proves a job newly listed as starting is news, one
/// still starting is not, a job that runs or ends is news, a removed job is not. Catches a
/// job listing left out of the check.
#[test]
fn a_job_is_news_when_it_is_taken_runs_or_ends_and_not_when_unchanged_or_removed() {
    let none = Snapshot::default();
    let starting = with_jobs(vec![job("j1", HeldState::Starting)]);
    let running = with_jobs(vec![job("j1", HeldState::Running)]);
    assert!(holds_news(&none, &starting));
    assert!(!holds_news(&starting, &starting));
    assert!(holds_news(&none, &running));
    assert!(holds_news(&starting, &running));
    assert!(!holds_news(&running, &none));
}

/// Proves a job already listed that changes while starting (here by gaining a result) is
/// news. Catches a check that treats a starting job already listed as quiet.
#[test]
fn a_job_listed_before_that_changes_while_starting_is_news() {
    let before = with_jobs(vec![job("j1", HeldState::Starting)]);
    let mut changed = job("j1", HeldState::Starting);
    changed.result = Some(job_result("j1"));
    assert!(holds_news(&before, &with_jobs(vec![changed])));
}

/// Proves a newly retired job id is news, an id retired before is not, and an id acknowledged
/// away is not. Catches a retired id that the server hears only at the interval.
#[test]
fn a_new_retired_id_is_news_and_a_known_or_removed_one_is_not() {
    let none = Snapshot::default();
    let retired = Snapshot {
        retired_jobs: vec!["j1".to_owned()],
        ..Snapshot::default()
    };
    assert!(holds_news(&none, &retired));
    assert!(!holds_news(&retired, &retired));
    assert!(!holds_news(&retired, &none));
}

/// Proves a taint newly set or changed is news, the same taint is not, and a taint cleared
/// to nothing is not. Catches a taint that is sent late, and one that rings again at each
/// publish while it stands.
#[test]
fn a_new_taint_is_news_and_the_same_or_a_removed_one_is_not() {
    let none = Snapshot::default();
    let tainted = Snapshot {
        taint: Some(taint("residue")),
        ..Snapshot::default()
    };
    let other = Snapshot {
        taint: Some(taint("cleanup-failed")),
        ..Snapshot::default()
    };
    assert!(holds_news(&none, &tainted));
    assert!(holds_news(&tainted, &other));
    assert!(!holds_news(&tainted, &tainted));
    assert!(!holds_news(&tainted, &none));
}

/// Proves a taint newly reported cleared is news, the same clear is not, and a clear that
/// goes away is not. Catches a clear the server hears of only at the interval.
#[test]
fn a_new_taint_clear_is_news_and_the_same_or_a_removed_one_is_not() {
    let none = Snapshot::default();
    let cleared = Snapshot {
        taint_cleared: Some(taint("residue")),
        ..Snapshot::default()
    };
    let other = Snapshot {
        taint_cleared: Some(taint("cleanup-failed")),
        ..Snapshot::default()
    };
    assert!(holds_news(&none, &cleared));
    assert!(holds_news(&cleared, &other));
    assert!(!holds_news(&cleared, &cleared));
    assert!(!holds_news(&cleared, &none));
}

/// Proves two empty snapshots hold no news. Catches a check that is news for nothing.
#[test]
fn nothing_against_nothing_is_not_news() {
    assert!(!holds_news(&Snapshot::default(), &Snapshot::default()));
}

/// A bell that counts its rings.
fn counting() -> (Wake, Arc<AtomicUsize>) {
    let rings = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&rings);
    let bell = Wake::new(move || {
        counted.fetch_add(1, Ordering::SeqCst);
    });
    (bell, rings)
}

/// Proves a publish rings the bell once when it holds news (a start taken is news) and not
/// when it does not, that what is published is what the shared state then holds, and that
/// `told` becomes the snapshot published so the same news does not ring twice. Catches a bell
/// rung for every publish, and one that never learns what it told.
#[test]
fn a_publish_rings_the_bell_for_news_only_and_remembers_what_it_told() {
    let shared = Shared::default();
    let (bell, rings) = counting();
    assert!(shared.bell.set(bell).is_ok());
    let answers = LeaseAnswers::default();
    let mut told = Snapshot::default();

    let starting = with_jobs(vec![job("j1", HeldState::Starting)]);
    shared.publish_news(&mut told, (starting.clone(), &answers, Vec::new()));
    assert_eq!(rings.load(Ordering::SeqCst), 1, "the start taken");
    assert_eq!(told, starting);

    shared.publish_news(&mut told, (starting.clone(), &answers, Vec::new()));
    assert_eq!(rings.load(Ordering::SeqCst), 1, "the same start rings once");

    let running = with_jobs(vec![job("j1", HeldState::Running)]);
    let lines = vec!["a line".to_owned()];
    shared.publish_news(&mut told, (running.clone(), &answers, lines));
    assert_eq!(rings.load(Ordering::SeqCst), 2);
    assert_eq!(told, running);
    assert_eq!(shared.lock().snapshot, running);

    shared.publish_news(&mut told, (running, &answers, Vec::new()));
    assert_eq!(rings.load(Ordering::SeqCst), 2, "the same news rings once");
}

/// Proves a publish with news and no bell given rings nobody and still publishes. Catches a
/// worker that needs a bell to publish.
#[test]
fn a_publish_with_news_and_no_bell_still_publishes() {
    let shared = Shared::default();
    let answers = LeaseAnswers::default();
    let mut told = Snapshot::default();
    let running = with_jobs(vec![job("j1", HeldState::Running)]);
    shared.publish_news(&mut told, (running.clone(), &answers, Vec::new()));
    assert_eq!(shared.lock().snapshot, running);
}
