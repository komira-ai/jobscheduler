//! The sweep over a fake process table ([`fake`]): which processes it ends, which it never
//! touches, its refusals, and its rounds.

mod fake;

use self::fake::{Death, FakeTable};
use super::{JobIds, Proc, ROUNDS_MAX, Swept, Tally, job_ids, sweep};

/// The slot account's uid.
const SLOT: u32 = 700;
/// The job's audit session, as the supervisor's ready record gives it.
const JOB_ASID: i32 = 7001;
/// The agent's own audit session.
const AGENT_ASID: i32 = 100_001;
/// launchd's session, which the per-user daemons of an account are in.
const LAUNCHD_ASID: i32 = 1;
/// The sweeping agent's pid.
const AGENT: i32 = 50;
/// A console user of the Mac.
const CONSOLE: u32 = 501;
/// The tierstore file service's uid.
const TIERSTORE: u32 = 260;

/// A process `pid` named `p<pid>`, of effective and real uid `uid`, in session `asid`.
fn p(pid: i32, uid: u32, asid: i32) -> Proc {
    Proc {
        pid,
        uid,
        ruid: uid,
        asid: Some(asid),
        name: format!("p{pid}"),
    }
}

/// The sweeping agent: root, in its own session.
fn agent() -> Proc {
    Proc {
        name: "js-agent".to_owned(),
        ..p(AGENT, 0, AGENT_ASID)
    }
}

/// launchd and the kernel.
fn system() -> [Proc; 2] {
    [p(0, 0, 0), p(1, 0, LAUNCHD_ASID)]
}

/// The swept process `pid`, named as [`p`] names it.
fn s(pid: i32) -> Swept {
    Swept {
        pid,
        name: format!("p{pid}"),
    }
}

/// The ids of the job: the slot, its session, the agent's pid, as the runtime builds them.
fn ids() -> JobIds {
    job_ids(SLOT, Some(JOB_ASID), AGENT, Some(AGENT_ASID))
}

/// A table of the agent, launchd and the kernel, and `procs`.
fn machine(procs: Vec<Proc>) -> FakeTable {
    let mut all = vec![agent()];
    all.extend(system());
    all.extend(procs);
    FakeTable::new(all)
}

/// A clean tally of `rounds` that found `by_session` and `by_user_only`, by the job's
/// session.
fn clean(by_session: Vec<Swept>, by_user_only: Vec<Swept>, rounds: usize) -> Tally {
    Tally {
        asid: Some(JOB_ASID),
        by_session,
        by_user_only,
        remaining: Vec::new(),
        rounds,
        clean: true,
        error: None,
    }
}

/// A process the job double-forked and that called `setsid` is outside the job's process
/// group, its session and its parent chain (the listing has no group or parent to look
/// at), and is found by its audit session; a daemon of the slot's uid in launchd's session
/// is found by the uid. Catches a sweep by process group or by tree only, which on the
/// proof Mac found 10 of a job's 13 processes.
#[test]
fn a_double_forked_setsid_process_is_found_by_session_and_by_uid() {
    let leave = Proc {
        name: "leave".to_owned(),
        ..p(200, SLOT, JOB_ASID)
    };
    let table = machine(vec![leave, p(300, SLOT, LAUNCHD_ASID)]);
    let tally = sweep(&table, &ids());
    let leave = Swept {
        pid: 200,
        name: "leave".to_owned(),
    };
    assert_eq!(tally, clean(vec![leave], vec![s(300)], 2));
    assert_eq!(table.kills(), [200, 300]);
    assert_eq!(table.pids(), [AGENT, 0, 1]);
}

/// A set-uid program the job ran (effective uid root, real uid the slot's) outside the
/// job's session is found by its real uid; a root process of the same session that is not
/// the slot's by either uid is not. Catches a uid rule that reads the effective uid only.
#[test]
fn a_set_uid_process_of_the_slot_is_found_by_its_real_uid() {
    let set_uid = Proc {
        ruid: SLOT,
        ..p(400, 0, LAUNCHD_ASID)
    };
    let table = machine(vec![set_uid, p(401, 0, LAUNCHD_ASID)]);
    assert_eq!(sweep(&table, &ids()), clean(Vec::new(), vec![s(400)], 2));
    assert_eq!(table.kills(), [400]);
}

/// The tierstore service and a console user's processes (one of them named as the job's
/// program is) stay in the table with their uids, and no kill is sent to them; only the
/// job's process is ended. Catches a rule that widens to every uid that is not root, or
/// that matches by name.
#[test]
fn no_process_of_another_uid_outside_the_session_is_touched() {
    let named_like_the_job = Proc {
        name: "leave".to_owned(),
        ..p(611, CONSOLE, 100_002)
    };
    let others = vec![
        p(600, TIERSTORE, 3),
        p(610, CONSOLE, 100_002),
        named_like_the_job,
    ];
    let job = Proc {
        name: "leave".to_owned(),
        ..p(200, SLOT, JOB_ASID)
    };
    let mut procs = others.clone();
    procs.push(job);
    let table = machine(procs);
    let tally = sweep(&table, &ids());
    assert!(tally.clean);
    assert_eq!(table.kills(), [200]);
    let mut kept = vec![agent()];
    kept.extend(system());
    kept.extend(others);
    assert_eq!(table.procs(), kept);
}

/// The kernel (pid 0), launchd (pid 1) and a pid below them (the `kill` of a negative pid
/// signals a group, of -1 every process the sender may signal) are never ended, even of
/// the slot's uid and in the job's session. Catches a missing pid guard.
#[test]
fn pids_one_and_below_and_the_sweeper_itself_are_never_ended() {
    let table = FakeTable::new(vec![
        agent(),
        p(-1, SLOT, JOB_ASID),
        p(0, SLOT, JOB_ASID),
        p(1, SLOT, JOB_ASID),
    ]);
    assert_eq!(sweep(&table, &ids()), clean(Vec::new(), Vec::new(), 1));
    assert!(table.kills().is_empty());
    // The sweeper itself, in the job's session and of the slot's uid, is never a target
    // (the sweep refuses such ids before it gets here: `super::targets` holds alone).
    let own = JobIds {
        uid: SLOT,
        asid: Some(JOB_ASID),
        own_pid: AGENT,
    };
    assert!(super::targets(&[p(AGENT, SLOT, JOB_ASID)], &own).is_empty());
}

/// A uid of 0 or below the first slot uid ends nothing and is not even listed; the agent's
/// own uid, effective or real, as the listing shows the sweeper, ends nothing; the error
/// says so. The first slot uid itself is swept. Catches a wrong id from a caller turning
/// the sweep on root, the system's accounts or the agent.
#[test]
fn a_uid_of_zero_or_below_501_or_the_agents_own_ends_nothing_and_says_so() {
    for uid in [0, 500] {
        let table = machine(vec![p(200, uid, JOB_ASID)]);
        let tally = sweep(&table, &JobIds { uid, ..ids() });
        let why = format!("uid {uid} is not a slot's (slots are 501 and up): nothing was ended");
        assert_eq!(
            tally,
            Tally {
                error: Some(why),
                ..Tally::default()
            }
        );
        assert!(table.listings().is_empty());
        assert!(table.kills().is_empty());
    }
    let as_slot = Proc {
        uid: SLOT,
        ..agent()
    };
    let real_slot = Proc {
        ruid: SLOT,
        ..agent()
    };
    for sweeper in [as_slot, real_slot] {
        let table = FakeTable::new(vec![sweeper, p(200, SLOT, JOB_ASID)]);
        assert_eq!(
            sweep(&table, &ids()),
            Tally {
                error: Some("uid 700 is the sweeping process's own".to_owned()),
                ..Tally::default()
            }
        );
        assert!(table.kills().is_empty());
    }
    let table = machine(vec![p(200, 501, LAUNCHD_ASID)]);
    let tally = sweep(&table, &JobIds { uid: 501, ..ids() });
    assert_eq!(tally, clean(Vec::new(), vec![s(200)], 2));
}

/// A sweeper missing from the listing ends nothing: its own uid, which the sweep must
/// never end, cannot be told. Catches a wrong `own_pid` letting the agent be a target.
#[test]
fn a_listing_without_the_sweeper_ends_nothing() {
    let table = FakeTable::new(vec![p(200, SLOT, JOB_ASID)]);
    let tally = sweep(&table, &ids());
    let why = "the sweeping process 50 is not in the listing, so its own uid is not known";
    assert_eq!(
        tally,
        Tally {
            error: Some(why.to_owned()),
            ..Tally::default()
        }
    );
    assert!(table.kills().is_empty());
}

/// `job_ids` drops a session that is the agent's own, is 0 or negative, or that the
/// agent's own unknown session could be; it keeps any other. And a sweep given the
/// agent's session anyway (ids made by hand) drops it when the listing shows the sweeper
/// in it, or shows no session for the sweeper: the agent's session-mates (root and a
/// console user) are untouched, the slot's processes are still found by uid, and the
/// later listings ask for no session. Catches a supervisor started with no session of its
/// own making the whole agent session the job's.
#[test]
fn the_agents_own_audit_session_is_never_used_as_the_jobs() {
    assert_eq!(
        job_ids(SLOT, Some(AGENT_ASID), AGENT, Some(AGENT_ASID)).asid,
        None
    );
    assert_eq!(job_ids(SLOT, Some(0), AGENT, Some(AGENT_ASID)).asid, None);
    assert_eq!(job_ids(SLOT, Some(-3), AGENT, Some(AGENT_ASID)).asid, None);
    assert_eq!(job_ids(SLOT, Some(JOB_ASID), AGENT, None).asid, None);
    assert_eq!(job_ids(SLOT, None, AGENT, Some(AGENT_ASID)).asid, None);
    assert_eq!(
        job_ids(SLOT, Some(JOB_ASID), AGENT, Some(AGENT_ASID)),
        JobIds {
            uid: SLOT,
            asid: Some(JOB_ASID),
            own_pid: AGENT,
        }
    );
    let by_hand = JobIds {
        asid: Some(AGENT_ASID),
        ..ids()
    };
    let unread = Proc {
        asid: None,
        ..agent()
    };
    for sweeper in [agent(), unread] {
        let mates = [p(60, 0, AGENT_ASID), p(61, CONSOLE, AGENT_ASID)];
        let mut procs = vec![sweeper];
        procs.extend(mates.clone());
        procs.push(p(200, SLOT, AGENT_ASID));
        let table = FakeTable::new(procs);
        let tally = sweep(&table, &by_hand);
        assert_eq!(
            tally,
            Tally {
                asid: None,
                ..clean(Vec::new(), vec![s(200)], 2)
            }
        );
        assert_eq!(table.kills(), [200]);
        assert_eq!(table.listings(), [true, false]);
    }
}

/// A child forked after the listing that found its parent and before the parent's kill is
/// found and ended in the next round. Catches a sweep of one listing.
#[test]
fn a_fork_between_the_listing_and_the_kill_is_found_in_the_next_round() {
    let table = machine(vec![p(200, SLOT, JOB_ASID)]).forking(200, vec![p(201, SLOT, JOB_ASID)]);
    let tally = sweep(&table, &ids());
    assert_eq!(tally, clean(vec![s(200), s(201)], Vec::new(), 3));
    assert_eq!(table.kills(), [200, 201]);
    assert_eq!(table.pids(), [AGENT, 0, 1]);
}

/// The sweep lists again after every round that found something and stops at the first
/// that finds none: a process that ends at its second kill takes three listings; a job
/// with nothing left takes one. A process found twice is named once. Catches a sweep that
/// reports clean without listing again, and one that keeps going after a clean round.
#[test]
fn rounds_stop_at_the_first_round_that_finds_none() {
    let table = machine(vec![p(200, SLOT, JOB_ASID)]).dying(200, Death::Second);
    assert_eq!(sweep(&table, &ids()), clean(vec![s(200)], Vec::new(), 3));
    assert_eq!(table.kills(), [200, 200]);
    assert_eq!(table.listings(), [true, true, true]);

    let table = machine(Vec::new());
    assert_eq!(sweep(&table, &ids()), clean(Vec::new(), Vec::new(), 1));
    assert_eq!(table.listings(), [true]);
    assert!(table.kills().is_empty());
}

/// A process that never dies ends the sweep after `ROUNDS_MAX` rounds, not clean, with the
/// process named in `remaining` and in the error. Catches an unbounded loop, and a sweep
/// that runs out of rounds and calls itself clean.
#[test]
fn a_process_that_never_dies_ends_the_sweep_at_rounds_max_not_clean_and_names_it() {
    let stuck = Proc {
        name: "stuck".to_owned(),
        ..p(200, SLOT, JOB_ASID)
    };
    let table = machine(vec![stuck]).dying(200, Death::Never);
    let tally = sweep(&table, &ids());
    let named = Swept {
        pid: 200,
        name: "stuck".to_owned(),
    };
    assert_eq!(
        tally,
        Tally {
            asid: Some(JOB_ASID),
            by_session: vec![named.clone()],
            by_user_only: Vec::new(),
            remaining: vec![named],
            rounds: ROUNDS_MAX,
            clean: false,
            error: Some("after 50 rounds these processes remain: 200 (stuck)".to_owned()),
        }
    );
    assert_eq!(table.kills(), vec![200; ROUNDS_MAX]);
    assert_eq!(table.listings().len(), ROUNDS_MAX);
}

/// The sweep pauses once between two rounds and never before the first or after the last:
/// counted, not timed. Catches a sweep that lists again at once, before the killed
/// processes have ended, and one that waits with no round to follow.
#[test]
fn pause_is_called_between_rounds_and_counted() {
    let table = machine(Vec::new());
    let _ = sweep(&table, &ids());
    assert_eq!(table.pauses(), 0);

    let table = machine(vec![p(200, SLOT, JOB_ASID)]).forking(200, vec![p(201, SLOT, JOB_ASID)]);
    assert_eq!(sweep(&table, &ids()).rounds, 3);
    assert_eq!(table.pauses(), 2);

    let table = machine(vec![p(200, SLOT, JOB_ASID)]).dying(200, Death::Never);
    let _ = sweep(&table, &ids());
    assert_eq!(table.pauses(), ROUNDS_MAX - 1);
}

/// A listing that fails ends the sweep, not clean, with the reason: at the first listing
/// nothing was ended; at the second, the first round's kills stand and the round is
/// counted. Catches a sweep that takes a failed listing for an empty one.
#[test]
fn a_listing_that_fails_ends_the_sweep_not_clean_with_the_reason() {
    let failed = |rounds| Tally {
        asid: Some(JOB_ASID),
        rounds,
        error: Some("listing the processes: the table is gone".to_owned()),
        ..Tally::default()
    };
    let table = machine(vec![p(200, SLOT, JOB_ASID)]).failing_at(0);
    assert_eq!(
        sweep(&table, &ids()),
        Tally {
            asid: None,
            ..failed(0)
        }
    );
    assert!(table.kills().is_empty());

    let table = machine(vec![p(200, SLOT, JOB_ASID)]).failing_at(1);
    assert_eq!(
        sweep(&table, &ids()),
        Tally {
            by_session: vec![s(200)],
            ..failed(1)
        }
    );
    assert_eq!(table.kills(), [200]);
}

/// The tally a log line or a report shows: the session used, the processes by rule, the
/// rounds and whether it is clean. Catches a renamed or dropped field.
#[test]
fn a_tally_serialises_with_its_session_its_rules_and_its_outcome() {
    let tally = clean(vec![s(200)], vec![s(300)], 2);
    assert_eq!(
        serde_json::to_string(&tally).expect("json"),
        r#"{"asid":7001,"by_session":[{"pid":200,"name":"p200"}],"by_user_only":[{"pid":300,"name":"p300"}],"remaining":[],"rounds":2,"clean":true,"error":null}"#
    );
}
