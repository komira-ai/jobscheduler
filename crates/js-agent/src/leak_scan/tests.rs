use std::collections::VecDeque;

use super::*;

/// A login item of the pool uid 601, as the scan prints one.
const LOGIN_ITEM: &str = "login-items \"loginitems.601.plist\" file 601:20 600 sha256:ab";
/// A Background Task Management entry of the pool uid 601.
const POOL_TASK: &str = "background-tasks uid=601 id=\"8.com.left\" type=\"agent\" url=\"\" \
                         exec=\"\" disposition=\"enabled\"";
/// One of root's.
const ROOT_TASK: &str = "background-tasks uid=0 id=\"16.com.apple.btm\" type=\"daemon\" \
                         url=\"\" exec=\"\" disposition=\"enabled\"";
/// A launch daemon, which only root can install.
const DAEMON: &str = "launch-daemons \"com.vendor.plist\" file 0:0 644 sha256:cd";

fn scanned(added: &[&str], removed: &[&str], errors: &[&str]) -> Scanned {
    let owned = |list: &[&str]| list.iter().map(|s| (*s).to_owned()).collect();
    Scanned {
        clean: added.is_empty() && removed.is_empty() && errors.is_empty(),
        added: owned(added),
        removed: owned(removed),
        errors: owned(errors),
    }
}

fn clean() -> Result<Scanned, ScanError> {
    Ok(scanned(&[], &[], &[]))
}

fn no_baseline() -> Result<Scanned, ScanError> {
    Err(ScanError::NoBaseline)
}

/// The check over `answers`, one per scan, in order.
fn checks(answers: Vec<Result<Scanned, ScanError>>) -> NativeChecks {
    let mut answers: VecDeque<_> = answers.into();
    NativeChecks::new(Box::new(move || {
        answers.pop_front().expect("the test scripted every scan")
    }))
}

fn run(checks: &mut NativeChecks) -> Result<(), CheckFinding> {
    checks.run(&Finished {
        run_id: "r-1",
        live: &[],
        slot: Some(0),
        live_slots: &[],
        held_mounts: &[],
    })
}

/// The notes of `checks`, as (code, detail).
fn notes(checks: &mut NativeChecks) -> Vec<(&'static str, String)> {
    checks
        .notes()
        .into_iter()
        .map(|note| (note.code, note.detail))
        .collect()
}

/// Catches: a run that left a login item passed as clean (the check that does nothing),
/// a finding that does not say what was left, or a residue forgotten after it was
/// reported, so that the re-check of the taint finds the host clean while the item is
/// still there.
#[test]
fn a_login_item_a_run_left_taints_the_host_and_is_found_again() {
    let mut checks = checks(vec![
        clean(),
        Ok(scanned(&[LOGIN_ITEM], &[], &[])),
        Ok(scanned(&[LOGIN_ITEM], &[], &[])),
    ]);
    checks.idle();
    let found = CheckFinding::residue(
        code::SCAN_RESIDUE,
        &format!("the leak scan found what a run left: + {LOGIN_ITEM}"),
    );
    assert_eq!(run(&mut checks), Err(found.clone()));
    assert_eq!(notes(&mut checks), []);
    assert_eq!(
        checks.run_restored(&Finished {
            run_id: "recheck",
            live: &[],
            slot: None,
            live_slots: &[],
            held_mounts: &[],
        }),
        Err(found)
    );
}

/// Catches: a Background Task Management entry of a pool user missed, or root's entry
/// (which no pool user can make) taken for a run's and the host tainted for it.
#[test]
fn a_pool_uid_s_background_task_is_residue_and_root_s_is_a_note() {
    let mut checks = checks(vec![
        clean(),
        Ok(scanned(&[ROOT_TASK, POOL_TASK], &[], &[])),
    ]);
    checks.idle();
    assert_eq!(
        run(&mut checks),
        Err(CheckFinding::residue(
            code::SCAN_RESIDUE,
            &format!("the leak scan found what a run left: + {POOL_TASK}"),
        ))
    );
    assert_eq!(
        notes(&mut checks),
        [(
            code::SCAN_HOST_CHANGED,
            format!("the leak scan saw the host change: + {ROOT_TASK}")
        )]
    );
}

/// Catches: the node's drift from its baseline (dirty before any run) judged against
/// every run, so that a host whose baseline is old is tainted by the first job; and a
/// change the idle host already had noted again after each run.
#[test]
fn what_the_idle_host_had_is_not_judged_again() {
    let drift = scanned(&[LOGIN_ITEM, DAEMON], &[ROOT_TASK], &[]);
    let mut checks = checks(vec![Ok(drift.clone()), Ok(drift)]);
    checks.idle();
    assert_eq!(run(&mut checks), Ok(()));
    assert_eq!(notes(&mut checks), []);
}

/// Catches: a change no run can make (a launch daemon, a lease user, the auto-login
/// state, an entry removed) tainting the host, or noted again after every later run. The
/// run comes before the host was first idle, so it is judged against the baseline alone.
#[test]
fn host_changes_are_noted_once_and_never_taint() {
    let changed = scanned(
        &[DAEMON, "lease-users uid 650", "auto-login intent 7.1"],
        &[LOGIN_ITEM, POOL_TASK],
        &[],
    );
    let mut checks = checks(vec![Ok(changed.clone()), Ok(changed)]);
    assert_eq!(run(&mut checks), Ok(()));
    assert_eq!(
        notes(&mut checks),
        [(
            code::SCAN_HOST_CHANGED,
            format!(
                "the leak scan saw the host change: + auto-login intent 7.1; + {DAEMON}; + \
                 lease-users uid 650; - {POOL_TASK}; - {LOGIN_ITEM}"
            )
            .chars()
            .take(crate::lifecycle::CHECK_DETAIL_MAX)
            .collect::<String>()
        )]
    );
    assert_eq!(run(&mut checks), Ok(()));
    assert_eq!(notes(&mut checks), []);
}

/// Catches: an item the scan's reply cut (`and <n> more`) passed as clean when it may be
/// a run's; and the same line of the removed list, which can hide only removals, taken
/// for residue.
#[test]
fn an_added_more_line_is_residue_and_a_removed_one_is_a_note() {
    let mut checks = checks(vec![
        clean(),
        Ok(scanned(&["and 3 more"], &["and 2 more"], &[])),
    ]);
    checks.idle();
    assert_eq!(
        run(&mut checks),
        Err(CheckFinding::residue(
            code::SCAN_RESIDUE,
            "the leak scan found what a run left: + and 3 more",
        ))
    );
    assert_eq!(
        notes(&mut checks),
        [(
            code::SCAN_HOST_CHANGED,
            "the leak scan saw the host change: - and 2 more".to_owned()
        )]
    );
}

/// Catches: a login item or task line whose uid cannot be read taken for root's (and so
/// passed), or a line that only looks like the cut count (`and some more`, `and  more`)
/// taken for one.
#[test]
fn an_unreadable_uid_is_residue_and_a_lookalike_count_is_not() {
    let odd = [
        "login-items \"loginitems.x.plist\" file 0:0 600 sha256:ab",
        "login-items \"other\" file 0:0 600 sha256:ab",
        "background-tasks uid=7",
        "background-tasks id=\"no uid\"",
        "login-items",
    ];
    for line in odd {
        let mut checks = checks(vec![clean(), Ok(scanned(&[line], &[], &[]))]);
        checks.idle();
        assert_eq!(
            run(&mut checks).map_err(|found| found.code()),
            Err(code::SCAN_RESIDUE),
            "{line}"
        );
    }
    for line in ["and some more", "and  more"] {
        let mut checks = checks(vec![clean(), Ok(scanned(&[line], &[], &[]))]);
        checks.idle();
        assert_eq!(run(&mut checks), Ok(()), "{line}");
        assert_eq!(notes(&mut checks).len(), 1, "{line}");
    }
}

/// Catches: a scan that could not read an item (a failing `sfltool`, an unreadable
/// folder) taken as clean, which would hide what it could not see.
#[test]
fn an_unreadable_item_leaves_the_host_unchecked() {
    let mut checks = checks(vec![
        clean(),
        Ok(scanned(
            &[LOGIN_ITEM],
            &[],
            &["dumpbtm failed: no access", "launch-agents x"],
        )),
    ]);
    checks.idle();
    assert_eq!(
        run(&mut checks),
        Err(CheckFinding::unchecked(
            code::SCAN_UNREADABLE,
            "the leak scan could not read: dumpbtm failed: no access; launch-agents x",
        ))
    );
}

/// Catches: a scan that gave no answer (it could not run, for whatever reason it says; not the
/// missing baseline, which is its own case) passed as clean.
#[test]
fn a_scan_that_cannot_run_leaves_the_host_unchecked() {
    let failures = [
        (
            Err(ScanError::Failed("hung up".to_owned())),
            "the leak scan could not run: hung up",
        ),
        (
            Err(ScanError::Failed("the disk is full".to_owned())),
            "the leak scan could not run: the disk is full",
        ),
    ];
    for (answer, detail) in failures {
        let mut checks = checks(vec![answer]);
        assert_eq!(
            run(&mut checks),
            Err(CheckFinding::unchecked(code::SCAN_FAILED, detail))
        );
        assert_eq!(notes(&mut checks), []);
    }
}

/// Catches: a node with no baseline (every node until one is recorded) tainting the host
/// after each run, a note repeated after every run, or the note never given again once a
/// recorded baseline went away.
#[test]
fn no_baseline_is_noted_once_and_every_run_passes() {
    let told = [(
        code::SCAN_NO_BASELINE,
        "the node has no leak-scan baseline, so the leak scan judges no run until one is \
         recorded"
            .to_owned(),
    )];
    let mut checks = checks(vec![
        no_baseline(),
        no_baseline(),
        no_baseline(),
        clean(),
        no_baseline(),
    ]);
    checks.idle();
    assert_eq!(run(&mut checks), Ok(()));
    assert_eq!(notes(&mut checks), told);
    assert_eq!(run(&mut checks), Ok(()));
    assert_eq!(notes(&mut checks), []);
    assert_eq!(run(&mut checks), Ok(()));
    assert_eq!(run(&mut checks), Ok(()));
    assert_eq!(notes(&mut checks), told);
}

/// Catches: an idle scan that failed or could not read an item replacing the reference,
/// so that what the idle host had is then judged against the next run.
#[test]
fn an_idle_scan_that_fails_keeps_the_reference() {
    let drift = scanned(&[LOGIN_ITEM], &[], &[]);
    let mut checks = checks(vec![
        Ok(drift.clone()),
        Ok(scanned(&[], &[], &["dumpbtm failed"])),
        Err(ScanError::Failed("hung up".to_owned())),
        no_baseline(),
        Ok(drift),
    ]);
    for _ in 0..4 {
        checks.idle();
    }
    assert_eq!(run(&mut checks), Ok(()));
    assert_eq!(notes(&mut checks), []);
}
