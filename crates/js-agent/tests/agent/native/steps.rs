//! The start of a native run step by step, over the real account pool (on a fake host) and
//! the fake launcher: the steps in order, a failure at each one undone, a restart, the run's
//! environment and limits, an agent without Full Disk Access, and the self-test.

use std::fs;
use std::os::unix::fs::PermissionsExt as _;

use js_agent::lifecycle::{Exit, RunHandle, Runtime, RuntimeError, Status};
use js_agent::native::{Journal, NativeRuntime, STEPS};
use js_agent_wire::refusal;
use js_mac_session::full_disk_access::FullDiskAccess;

use super::{Failing, Rig, TERM, config, native, pool_settings, settle};
use crate::support::Scratch;

/// The leases the rig's journal holds live.
fn live(rig: &Rig) -> Vec<String> {
    Journal::new(&rig.dir.path("native-leases")).live().unwrap()
}

/// The one account the rig's pool keeps ready, by name.
fn ready_account(rig: &Rig) -> String {
    let records = rig.accounts.records();
    assert_eq!(records.len(), 1, "{records:?}");
    records[0].clone()
}

/// Asserts that a run's start records the eight step names, in their order, once each, and
/// that a second call has none. Never a duration.
///
/// Catches: a step left out, done twice, or done in another order (an account lent before
/// the bundle is pinned, a supervisor started before the profile is built).
#[test]
fn a_start_records_its_eight_steps_in_order_once() {
    let mut rig = Rig::new("steps-order");
    let id = rig.start("steps", "/usr/bin/true", &[]);
    let steps: Vec<&str> = rig
        .rt
        .start_steps(&id)
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    assert_eq!(
        steps,
        [
            "checked",
            "bundle-pinned",
            "account-lent",
            "folder-made",
            "inputs-linked",
            "profile-built",
            "supervisor-started",
            "job-ready",
        ]
    );
    assert_eq!(steps, STEPS);
    assert!(rig.rt.start_steps(&id).is_empty(), "given once");
    rig.settle(&id);
    rig.rt.remove(&id).unwrap();
}

/// What a failed start must leave: no account lent (the pool keeps one ready account, not
/// the failed start's, and holds none), no live lease in the journal, and no job started.
fn nothing_left(rig: &Rig, lent: &str, launches: usize) {
    assert!(rig.pool.held().is_empty(), "{:?}", rig.pool.held());
    let records = rig.accounts.records();
    assert_eq!(records.len(), 1, "{records:?}");
    assert_ne!(records[0], lent, "the failed start's account is retired");
    assert_eq!(live(rig), Vec::<String>::new());
    assert_eq!(rig.launcher.launched().len(), launches);
}

/// Asserts that a start that fails after its account is lent (the run directory, the
/// inputs, the profile, the supervisor's start, the supervisor's setup) retires the account,
/// marks its lease done and starts no job, and that the error names the step's cause.
///
/// Catches: a failed start that leaks a uid (an account left lent), a live lease, or a job.
#[test]
fn a_start_that_fails_after_the_account_is_lent_leaves_nothing() {
    // The run directory: the account's home holds a file named `exec`.
    let mut rig = Rig::new("fail-folder");
    let lent = ready_account(&rig);
    fs::write(rig.home_of(&lent).join("exec"), b"").unwrap();
    let why = rig
        .rt
        .create_and_start(&rig.spec("folder", native("/usr/bin/true", &[])));
    assert!(
        why.unwrap_err()
            .0
            .ends_with("exec: File exists (os error 17)")
    );
    nothing_left(&rig, &lent, 0);

    // The inputs: the run's files directory is not there.
    let mut rig = Rig::new("fail-inputs");
    let lent = ready_account(&rig);
    let mut spec = rig.spec("inputs", native("/usr/bin/true", &[]));
    spec.files = Some(rig.dir.path("no-files"));
    let why = rig.rt.create_and_start(&spec).unwrap_err();
    assert!(why.0.starts_with("the files: "), "{}", why.0);
    nothing_left(&rig, &lent, 0);

    // The profile: a read model the runtime builds no grants for.
    let mut rig = Rig::with("fail-profile", |config| {
        config.read_model = js_mac_pool::seatbelt::ReadModel::Allow;
    });
    let lent = ready_account(&rig);
    let why = rig
        .rt
        .create_and_start(&rig.spec("profile", native("/usr/bin/true", &[])));
    assert!(why.unwrap_err().0.starts_with("the profile: "));
    nothing_left(&rig, &lent, 0);

    // The supervisor cannot be started.
    let mut rig = Rig::new("fail-launch");
    let lent = ready_account(&rig);
    rig.launcher.fail(Failing::Launch);
    let why = rig
        .rt
        .create_and_start(&rig.spec("launch", native("/usr/bin/true", &[])));
    assert_eq!(
        why.unwrap_err().0,
        "the supervisor: the supervisor cannot be started"
    );
    nothing_left(&rig, &lent, 1);

    // The supervisor's setup fails: the job never starts.
    let mut rig = Rig::new("fail-setup");
    let lent = ready_account(&rig);
    rig.launcher.fail(Failing::Setup);
    let why = rig
        .rt
        .create_and_start(&rig.spec("setup", native("/usr/bin/true", &[])));
    let why = why.unwrap_err().0;
    assert!(
        why.starts_with("the supervisor: ended before the job started: SetupFailed"),
        "{why}"
    );
    nothing_left(&rig, &lent, 1);
}

/// Asserts that a start that fails before an account is lent (a bundle it cannot pin, no
/// free slot) lends none and leaves no live lease.
///
/// Catches: an account lent for a start that could never run, or a lease left live.
#[test]
fn a_start_that_fails_before_an_account_is_lent_lends_none() {
    let mut rig = Rig::new("fail-bundle");
    let ready = ready_account(&rig);
    let mut spec = rig.spec("bundle", native("@bundle/cc", &[]));
    spec.bundle = Some("example.com/b@sha256:00".to_owned());
    let why = rig.rt.create_and_start(&spec).unwrap_err();
    assert_eq!(why.0, "the bundle: this runtime holds no bundle store");
    assert_eq!(
        rig.accounts.records(),
        std::slice::from_ref(&ready),
        "still ready"
    );
    assert_eq!(live(&rig), Vec::<String>::new());

    // One slot, held by a run: the next start finds no free uid.
    let first = rig.start("first", "/bin/cat", &[]);
    let why = rig
        .rt
        .create_and_start(&rig.spec("second", native("/usr/bin/true", &[])))
        .unwrap_err();
    assert!(why.0.starts_with("the account pool: "), "{}", why.0);
    assert_eq!(
        live(&rig),
        std::slice::from_ref(&first.0),
        "only the first run's lease"
    );
    assert_eq!(rig.launcher.launched().len(), 1);
    rig.settle(&first);
    rig.rt.remove(&first).unwrap();
    assert_eq!(live(&rig), Vec::<String>::new());
}

/// Asserts that when the account of a failed start cannot be retired, the start's error says
/// so with the step the pool holds it at, the pool holds it, and its lease stays live for a
/// later start of the agent; and that a removal whose retirement fails is an error that keeps
/// the run, so a second removal retires the account.
///
/// Catches: a held account reported as a clean failure, or a run forgotten while its account
/// is held.
#[test]
fn an_account_that_cannot_be_retired_is_held_and_said() {
    let mut rig = Rig::new("held");
    let lent = ready_account(&rig);
    rig.accounts.keep_record(&lent);
    rig.launcher.fail(Failing::Launch);
    let why = rig
        .rt
        .create_and_start(&rig.spec("held", native("/usr/bin/true", &[])))
        .unwrap_err();
    assert!(
        why.0.starts_with(
            "the supervisor: the supervisor cannot be started; and the run's account could not \
             be retired (account "
        ),
        "{}",
        why.0
    );
    assert!(why.0.contains(&format!("{lent} (uid ")), "{}", why.0);
    assert_eq!(rig.pool.held().len(), 1);
    assert_eq!(live(&rig), [format!("{TERM}.0")]);

    // A run whose removal cannot retire its account.
    let mut rig = Rig::new("held-removal");
    let lent = ready_account(&rig);
    let id = rig.start("held-removal", "/usr/bin/true", &[]);
    rig.settle(&id);
    rig.accounts.keep_record(&lent);
    let why: RuntimeError = rig.rt.remove(&id).unwrap_err();
    assert!(
        why.0.starts_with(&format!("account {lent} (uid ")),
        "{}",
        why.0
    );
    assert_eq!(live(&rig), std::slice::from_ref(&id.0));
    rig.accounts.free_record(&lent);
    rig.rt.remove(&id).unwrap();
    assert!(rig.pool.held().is_empty());
    assert!(!rig.accounts.records().contains(&lent));
    assert_eq!(live(&rig), Vec::<String>::new());
}

/// Asserts that after a restart (a new runtime over the same state, its pool opened with no
/// adopted job) the earlier process's run is listed and ended, its account is retired by the
/// open, its removal marks its lease done, and a later job gets an account of its own.
///
/// Catches: a restart that adopts a run nobody can watch, or that leaves the earlier run's
/// account lent.
#[test]
fn a_restart_ends_the_runs_of_the_earlier_process() {
    let mut rig = Rig::new("restart");
    let first = ready_account(&rig);
    let old = rig.start("old", "/usr/bin/true", &[]);
    rig.settle(&old);
    let mut next = rig.reopened();
    assert!(
        !rig.accounts.records().contains(&first),
        "retired by the open"
    );
    let found = next.list_labelled().unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].id, old);
    assert_eq!(next.poll(&old).unwrap(), Status::Gone);
    next.remove(&old).unwrap();
    assert_eq!(live(&rig), Vec::<String>::new());
    let spec = rig.spec("new", native("/usr/bin/true", &[]));
    let new = next.create_and_start(&spec).unwrap();
    assert_eq!(new, RunHandle(format!("{}.0", TERM + 1)));
    let account = rig.launcher.last();
    assert_ne!(account.var("USER"), Some(first.as_str()), "a fresh account");
    settle(&mut next, &new);
    next.remove(&new).unwrap();
}

/// Asserts that the run's environment holds `HOME` and `CFFIXED_USER_HOME` (the job's home in
/// the run directory), `USER` and `LOGNAME` (the account's name) and `xcrun_db` (a file in the
/// run's `TMPDIR`), and that a body or a job start that sets any of them is refused, by the
/// runtime and at admission, before an account is lent.
///
/// Catches: a job running with root's `HOME`, a cache of `xcrun` refused in the per-user
/// folder, or a job that chooses its own home.
#[test]
fn the_run_gets_the_accounts_names_and_may_not_set_them() {
    let mut rig = Rig::new("env");
    let lent = ready_account(&rig);
    let id = rig.start("env", "/usr/bin/true", &[]);
    let launched = rig.launcher.last();
    let exec = rig.home_of(&lent).join("exec");
    let home = exec.join(".home").display().to_string();
    assert_eq!(launched.var("HOME"), Some(home.as_str()));
    assert_eq!(launched.var("CFFIXED_USER_HOME"), Some(home.as_str()));
    assert_eq!(launched.var("USER"), Some(lent.as_str()));
    assert_eq!(launched.var("LOGNAME"), Some(lent.as_str()));
    let db = exec.join(".tmp/xcrun_db").display().to_string();
    assert_eq!(launched.var("xcrun_db"), Some(db.as_str()));
    for name in ["HOME", "CFFIXED_USER_HOME", "USER", "LOGNAME", "xcrun_db"] {
        assert_eq!(
            launched
                .env
                .iter()
                .filter(|(given, _)| given == name)
                .count(),
            1,
            "{name} is set once"
        );
    }
    rig.settle(&id);
    rig.rt.remove(&id).unwrap();

    for name in ["USER", "LOGNAME", "xcrun_db"] {
        let mut spec = rig.spec(&format!("job-{name}"), native("/usr/bin/true", &[]));
        spec.env = vec![(name.to_owned(), "x".to_owned())];
        let why = rig.rt.create_and_start(&spec).unwrap_err();
        assert_eq!(
            why.0,
            format!("env: {name} is set by the runtime for the run's account")
        );
        let refused = rig.rt.check_env(&spec.env).unwrap_err();
        assert_eq!(refused.code, refusal::INVALID_ENV);
        let mut body = native("/usr/bin/true", &[]);
        body.env.insert(name.to_owned(), "x".to_owned());
        assert_eq!(
            rig.rt.check_native(&body).unwrap_err().code,
            refusal::INVALID_ENV
        );
        let why = rig
            .rt
            .create_and_start(&rig.spec(&format!("body-{name}"), body))
            .unwrap_err();
        assert_eq!(
            why.0,
            format!("native.env: {name} is set by the runtime for the run's account")
        );
    }
    assert_eq!(live(&rig), Vec::<String>::new());
    assert_eq!(rig.launcher.launched().len(), 1);
}

/// Asserts that the supervisor is asked for the run's account (its uid and group), a session
/// of its own, the configured largest file, the configured processes and one more (the
/// supervisor itself is a process of the account), its records in the run's scratch
/// directory, the program and arguments as given, and the profile's text and parameters,
/// with the run directory as `RUN`.
///
/// Catches: a supervisor whose own process makes the job's limit one short, a job started
/// as another uid, or records kept where the job could reach them.
#[test]
fn the_supervisor_is_asked_for_the_account_the_limits_and_the_profile() {
    let mut rig = Rig::with("spec", |config| {
        config.max_processes = 40;
        config.max_file_bytes = 1 << 20;
    });
    let lent = ready_account(&rig);
    let id = rig.start("spec", "/bin/echo", &["a", "b c"]);
    let launched = rig.launcher.last();
    assert_eq!(launched.uid, super::me());
    assert_eq!(launched.gid, super::my_gid());
    assert!(launched.new_session);
    assert_eq!(launched.max_processes, 41);
    assert_eq!(launched.max_file_bytes, 1 << 20);
    assert_eq!(
        launched.record_dir,
        rig.dir.path("scratch").join("spec").join("supervisor")
    );
    assert_eq!(
        fs::metadata(&launched.record_dir)
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        0o700,
        "the supervisor's records are the agent's alone"
    );
    assert_eq!(launched.program, "/bin/echo");
    assert_eq!(launched.args, ["a", "b c"]);
    let exec = rig.home_of(&lent).join("exec");
    assert_eq!(launched.cwd, fs::canonicalize(&exec).unwrap());
    let run = exec.display().to_string();
    assert_eq!(super::param(&launched, "RUN"), Some(run.as_str()));
    assert!(launched.profile.starts_with("(version 1)"));
    assert_eq!(
        rig.settle(&id),
        Status::Exited(Exit {
            code: 0,
            oom_killed: false
        })
    );
    rig.rt.remove(&id).unwrap();
    let (retired, how) = rig
        .rt
        .last_retired()
        .expect("the removal retired the account");
    assert_eq!(retired, &lent);
    assert!(
        how.gone.record && how.gone.home && how.gone.folders,
        "{how:?}"
    );
}

/// Asserts that a runtime opened without Full Disk Access (not granted, or not known) has no
/// pool and makes nothing for a start: the admission refuses it `no-full-disk-access`, and a
/// start that reaches the runtime fails with no lease written.
///
/// Catches: an agent without the grant that exits instead of reporting it, or that lends
/// accounts it could never delete.
#[test]
fn without_full_disk_access_there_is_no_pool_and_every_start_is_refused() {
    for held in [FullDiskAccess::NotGranted, FullDiskAccess::Undetermined] {
        let dir = Scratch::new("native-no-grant");
        let settings = pool_settings(&dir, 1);
        let mut config = config(&dir, &settings);
        config.full_disk_access = held;
        let launcher = super::FakeLauncher::new();
        let mut rt = NativeRuntime::open(launcher.clone(), config, TERM).unwrap();
        assert!(!rt.has_pool());
        let body = native("/usr/bin/true", &[]);
        assert_eq!(
            rt.check_native(&body).unwrap_err().code,
            refusal::NO_FULL_DISK_ACCESS
        );
        let why = rt
            .create_and_start(&super::spec_in(&dir, "refused", body))
            .unwrap_err();
        assert_eq!(
            why.0,
            "the account pool is not open: the agent does not hold Full Disk Access"
        );
        assert!(rt.self_test().is_ok(), "nothing to run without a pool");
        assert!(launcher.launched().is_empty());
        let journal = Journal::new(&dir.path("native-leases"));
        assert_eq!(journal.live().unwrap(), Vec::<String>::new());
    }
}

/// Asserts that the self-test runs `/usr/bin/true` through a lent account and retires it,
/// leaving no lease, and that a self-test whose supervisor fails its setup fails.
///
/// Catches: an agent that says hello while its supervisor or its pool cannot run a job.
#[test]
fn the_self_test_runs_true_through_an_account_and_retires_it() {
    let mut rig = Rig::new("self-test");
    let lent = ready_account(&rig);
    rig.rt.self_test().unwrap();
    let launched = rig.launcher.last();
    assert_eq!(launched.program, "/usr/bin/true");
    assert_eq!(launched.var("USER"), Some(lent.as_str()));
    assert!(!rig.accounts.records().contains(&lent), "retired");
    assert!(rig.pool.held().is_empty());
    assert_eq!(live(&rig), Vec::<String>::new());

    let mut rig = Rig::new("self-test-fails");
    rig.launcher.fail(Failing::Setup);
    let why = rig.rt.self_test().unwrap_err();
    assert!(
        why.0
            .starts_with("the supervisor: ended before the job started"),
        "{}",
        why.0
    );
    assert_eq!(live(&rig), Vec::<String>::new());
}

/// Asserts that a stop sends `SIGTERM` to the job's group (a job that ends on it exits
/// 128 + 15), and that once the stop's grace has passed a poll ends a job that ignores it.
///
/// Catches: a stop that never reaches the job, or a job that outlives its grace.
#[test]
fn a_stop_terms_the_job_and_ends_it_after_the_grace() {
    let mut rig = Rig::new("stop");
    let id = rig.start("stop", "/bin/sleep", &["600"]);
    rig.rt
        .stop(&id, std::time::Duration::from_secs(600))
        .unwrap();
    assert_eq!(
        rig.settle(&id),
        Status::Exited(Exit {
            code: 128 + 15,
            oom_killed: false
        })
    );
    rig.rt.remove(&id).unwrap();

    let now = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1_000));
    let clock = now.clone();
    let rig = Rig::new("stop-ignored");
    let mut rt = rig.rt.with_clock(std::sync::Arc::new(move || {
        clock.load(std::sync::atomic::Ordering::Relaxed)
    }));
    // The job ignores `SIGTERM`, then says so in a file, then sleeps (an ignored signal stays
    // ignored across `exec`); the stop is sent only once the file is there.
    let marker = rig.dir.path("trapped");
    let script = format!(
        "trap '' TERM; : > {}; exec /bin/sleep 600",
        marker.display()
    );
    let spec = super::spec_in(&rig.dir, "ignored", native("/bin/sh", &["-c", &script]));
    let id = rt.create_and_start(&spec).unwrap();
    for _ in 0..super::POLLS {
        if marker.exists() {
            break;
        }
        std::thread::yield_now();
    }
    assert!(marker.exists(), "the job never set its trap");
    rt.stop(&id, std::time::Duration::from_millis(500)).unwrap();
    assert_eq!(rt.poll(&id).unwrap(), Status::Running, "within the grace");
    now.store(1_500, std::sync::atomic::Ordering::Relaxed);
    assert_eq!(
        settle(&mut rt, &id),
        Status::Exited(Exit {
            code: 128 + 9,
            oom_killed: false
        })
    );
    rt.remove(&id).unwrap();
}

/// The write grants of a launch's profile: the value of each parameter a
/// `(allow file-write* (subpath (param "NAME")))` line names.
fn write_grants(launched: &super::Launched) -> Vec<String> {
    launched
        .profile
        .lines()
        .filter(|line| line.starts_with("(allow file-write*"))
        .flat_map(|line| line.split("(param \"").skip(1))
        .filter_map(|rest| rest.split('"').next())
        .filter_map(|name| super::param(launched, name))
        .map(str::to_owned)
        .collect()
}

/// Asserts that the lent account's own per-user temporary and cache folders are granted
/// writes by their own paths, beside the run directory, and that nothing else below the
/// per-user folders' root is: not the root, not the account's whole entry, not another
/// account's folder; and that a lookup that fails fails the start with nothing left.
///
/// Catches a profile that grants `/var/folders` as a whole or another account's folder, and a
/// start that runs without the folders `xcrun` writes its cache in.
#[test]
fn the_accounts_own_folders_are_granted_and_nothing_else_below_them() {
    let rig = Rig::new("own-folders");
    let root = rig.dir.path("folders");
    for part in ["ab/mine/T", "ab/mine/C", "ab/mine/0", "cd/theirs/T"] {
        fs::create_dir_all(root.join(part)).unwrap();
    }
    let root = fs::canonicalize(root).unwrap();
    let mine = (root.join("ab/mine/T"), root.join("ab/mine/C"));
    let given = mine.clone();
    let find: js_agent::native::OwnFolders =
        std::sync::Arc::new(move |_, _| Ok(vec![given.0.clone(), given.1.clone()]));
    let mut rig = Rig {
        rt: rig.rt.with_own_folders(find),
        ..rig
    };
    let lent = ready_account(&rig);
    let id = rig.start("own", "/usr/bin/true", &[]);
    let launched = rig.launcher.last();
    let grants = write_grants(&launched);
    let exec = rig.home_of(&lent).join("exec").display().to_string();
    let (temp, cache) = (mine.0.display().to_string(), mine.1.display().to_string());
    // The grants follow the run directory in path order: the cache folder (`C`), then the
    // temporary one (`T`).
    assert_eq!(grants, [exec, cache, temp], "{}", launched.profile);
    for refused in [
        root.clone(),
        root.join("ab/mine"),
        root.join("ab/mine/0"),
        root.join("cd/theirs/T"),
    ] {
        let covered = grants.iter().any(|grant| refused.starts_with(grant));
        assert!(!covered, "{} is writable", refused.display());
    }
    rig.settle(&id);
    rig.rt.remove(&id).unwrap();

    let rig = Rig::new("own-folders-fail");
    let lent = ready_account(&rig);
    let fails: js_agent::native::OwnFolders =
        std::sync::Arc::new(|_, _| Err(std::io::Error::other("getconf failed")));
    let mut rig = Rig {
        rt: rig.rt.with_own_folders(fails),
        ..rig
    };
    let why = rig
        .rt
        .create_and_start(&rig.spec("fail", native("/usr/bin/true", &[])))
        .unwrap_err();
    assert_eq!(why.0, "the account's own folders: getconf failed");
    nothing_left(&rig, &lent, 0);
}

/// Asserts that the self-test's scratch directory is `self-test` below the pool's state
/// directory (the supervisor's records are made in it), that what an earlier self-test left
/// there is removed before each run, that the directory is private to the agent (mode 0700),
/// and that a second self-test over the same directory passes.
///
/// Catches: a self-test whose scratch is elsewhere, one that trips over or reuses the files
/// of an earlier self-test, and a scratch directory other users can enter.
#[test]
fn the_self_test_scratch_is_fresh_private_and_below_the_state_directory() {
    let mut rig = Rig::new("self-test-scratch");
    let scratch = rig.dir.path("accounts").join("self-test");
    for round in 0..2 {
        fs::create_dir_all(scratch.join("old/nested")).unwrap();
        fs::write(scratch.join("leftover"), b"left").unwrap();
        ready_account(&rig);
        rig.rt.self_test().unwrap();
        assert_eq!(
            rig.launcher.last().record_dir,
            scratch.join("supervisor"),
            "round {round}"
        );
        assert!(!scratch.join("leftover").exists(), "round {round}");
        assert!(!scratch.join("old").exists(), "round {round}");
        assert_eq!(
            fs::metadata(&scratch).unwrap().permissions().mode() & 0o7777,
            0o700,
            "round {round}"
        );
        assert_eq!(live(&rig), Vec::<String>::new());
    }
    assert_eq!(rig.launcher.launched().len(), 2);
}

/// Asserts that a self-test whose scratch directory cannot be made fresh (its parent is a
/// file, so the removal fails with an error other than "not found") fails naming that
/// directory and the cause, and starts no supervisor.
///
/// Catches: a self-test that goes on over a scratch directory it could not make, and an error
/// that does not say which directory it was.
#[test]
fn a_self_test_scratch_that_cannot_be_made_fails_naming_it() {
    let mut rig = Rig::in_dir("self-test-blocked", 1, |config, dir| {
        let blocker = dir.path("blocker");
        fs::write(&blocker, b"a file").unwrap();
        config.accounts.state = blocker.join("state");
    });
    let scratch = rig.dir.path("blocker").join("state").join("self-test");
    let why = rig.rt.self_test().unwrap_err();
    assert_eq!(
        why.0,
        format!("{}: Not a directory (os error 20)", scratch.display())
    );
    assert!(rig.launcher.launched().is_empty());
    assert_eq!(live(&rig), Vec::<String>::new());
}

/// Asserts that a self-test whose run is still going when its polls are used up fails with
/// `/usr/bin/true did not end`, and still removes the run: its lease is done, its account is
/// retired and the pool holds none. The test gives the self-test no polls, so the run has not
/// been seen to end however fast it is.
///
/// Catches: a self-test that passes, or hangs, over a job that never ends, and one that leaves
/// the run's account lent or its lease live after giving up.
#[test]
fn a_self_test_whose_job_does_not_end_fails_and_removes_the_run() {
    let rig = Rig::new("self-test-hangs");
    let lent = ready_account(&rig);
    let mut rig = Rig {
        rt: rig.rt.with_self_test_polls(0),
        ..rig
    };
    let why = rig.rt.self_test().unwrap_err();
    assert_eq!(why.0, "/usr/bin/true did not end");
    assert_eq!(rig.launcher.launched().len(), 1);
    assert!(!rig.accounts.records().contains(&lent), "retired");
    assert!(rig.pool.held().is_empty());
    assert_eq!(live(&rig), Vec::<String>::new());
}

/// Asserts that the production `open` installs the finder of the lent account's own per-user
/// folders (so a real run may write them), even with no pool, and that the test constructor
/// `over` installs none.
///
/// Catches: a production runtime opened without the folders finder, whose runs could not
/// write the cache folders `xcrun` needs.
#[test]
fn the_production_open_installs_the_own_folders_finder() {
    let dir = Scratch::new("native-own-finder");
    let settings = pool_settings(&dir, 1);
    let mut cfg = config(&dir, &settings);
    cfg.full_disk_access = FullDiskAccess::NotGranted;
    let opened = NativeRuntime::open(super::FakeLauncher::new(), cfg.clone(), TERM).unwrap();
    assert!(opened.has_own_folders());
    let over = NativeRuntime::over(None, super::FakeLauncher::new(), cfg, TERM).unwrap();
    assert!(!over.has_own_folders());
}

/// Asserts that an existing state directory is left as it is by the open (not an error of its
/// own, not changed to mode 0700) and the pool then refuses it: here the mode 0777 lets others
/// write in it, so the open fails with `the account pool: ...` and the directory keeps its
/// mode and its contents.
///
/// Catches: an open that fails on a directory that is already there, one that rewrites its
/// mode, and a pool refusal that does not say it is the pool's.
#[test]
fn a_pool_that_refuses_the_existing_state_directory_ends_the_open_naming_the_pool() {
    let dir = Scratch::new("native-pool-refused");
    let settings = pool_settings(&dir, 1);
    let mut cfg = config(&dir, &settings);
    let state = dir.path("loose-state");
    fs::create_dir(&state).unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o777)).unwrap();
    fs::write(state.join("marker"), b"mine").unwrap();
    cfg.accounts.state = state.clone();
    let why = NativeRuntime::open(super::FakeLauncher::new(), cfg, TERM)
        .err()
        .expect("no runtime over a pool that refuses")
        .0;
    assert!(why.starts_with("the account pool: "), "{why}");
    assert!(why.len() > "the account pool: ".len(), "{why}");
    assert_eq!(
        fs::metadata(&state).unwrap().permissions().mode() & 0o7777,
        0o777
    );
    assert_eq!(fs::read(state.join("marker")).unwrap(), b"mine");
}

/// Asserts that a state directory that cannot be made (its parent is missing, or is a file)
/// ends the open with its path and the operating system's reason, and makes nothing.
///
/// Catches: an open that swallows every error of making the directory (and goes on to the
/// pool), or that names no path.
#[test]
fn a_state_directory_that_cannot_be_made_ends_the_open_naming_it() {
    let dir = Scratch::new("native-state-unmade");
    let settings = pool_settings(&dir, 1);
    let missing = dir.path("no-such-parent").join("state");
    fs::write(dir.path("a-file"), b"a file").unwrap();
    let below_file = dir.path("a-file").join("state");
    for (state, reason) in [
        (missing, "No such file or directory (os error 2)"),
        (below_file, "Not a directory (os error 20)"),
    ] {
        let mut cfg = config(&dir, &settings);
        cfg.accounts.state = state.clone();
        let why = NativeRuntime::open(super::FakeLauncher::new(), cfg, TERM)
            .err()
            .expect("no runtime without a state directory")
            .0;
        assert_eq!(why, format!("{}: {reason}", state.display()));
        assert!(!state.exists());
    }
}
