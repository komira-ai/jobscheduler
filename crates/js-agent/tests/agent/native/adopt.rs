//! Adoption: a native run outlives the agent object that started it, and the runtime of the
//! next process finds it again through the journal and the helper's `watch`, lists it with
//! its labels and its age, and reports how it ends; a run the helper no longer has is listed
//! ended, never leaked.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use js_agent::lifecycle::{Exit, Found, RunHandle, Runtime, Status, label};
use js_agent::native::{Journal, NativeRuntime};
use js_mac_session::helper::RESET_ALREADY;

use super::fake_helper::FakeHelper;
use super::{Rig, TERM, config, native};

/// A wall clock a test sets, in milliseconds since the Unix epoch.
#[derive(Clone, Default)]
struct Wall(Arc<AtomicU64>);

impl Wall {
    fn at(ms: u64) -> Self {
        let wall = Self::default();
        wall.0.store(ms, Ordering::SeqCst);
        wall
    }

    fn set(&self, ms: u64) {
        self.0.store(ms, Ordering::SeqCst);
    }

    fn clock(&self) -> js_agent::native::Clock {
        let wall = Arc::clone(&self.0);
        Arc::new(move || wall.load(Ordering::SeqCst))
    }
}

fn labels(run_id: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        (label::RUN.to_owned(), run_id.to_owned()),
        (label::LEASE.to_owned(), format!("lease-{run_id}")),
    ])
}

/// A runtime of the next process over the same helper and journal, after the helper has
/// reset already (it did not restart: the agent did).
fn next(fake: &FakeHelper, wall: &Wall, term: u64) -> NativeRuntime {
    fake.refuse("boot-reset", RESET_ALREADY);
    let opened = NativeRuntime::open(fake.client(), config(fake), term).unwrap();
    opened.with_clock(wall.clock())
}

/// Polls `rt` until the run `id` no longer runs.
fn settle(rt: &mut NativeRuntime, id: &RunHandle) -> Status {
    for _ in 0..super::POLLS {
        let status = rt.poll(id).unwrap();
        if status != Status::Running {
            return status;
        }
        std::thread::yield_now();
    }
    panic!("the run did not end");
}

/// Starts `r1` at wall time 1000 on a runtime that is then dropped, as the agent's process
/// ends; returns the helper, the journal's directory and the clock.
fn started_and_dropped(name: &str) -> (FakeHelper, super::Scratch, Wall) {
    let wall = Wall::at(1000);
    let mut rig = Rig::new(name);
    rig.rt = std::mem::replace(
        &mut rig.rt,
        NativeRuntime::open(rig.fake.client(), config(&rig.fake), TERM + 9).unwrap(),
    )
    .with_clock(wall.clock());
    let mut spec = rig.spec("r1", native("/bin/true", &[]));
    spec.labels = labels("r1");
    rig.rt.create_and_start(&spec).unwrap();
    let Rig { fake, rt, dir } = rig;
    drop(rt);
    (fake, dir, wall)
}

/// Asserts the adoption: the next runtime's reclaim leaves the run alone, its listing holds
/// the run with the labels its start had and the time it has run on the wall clock, the run
/// then reports the exit the helper sees, and its removal ends the user and the lease. It
/// catches an agent restart that kills running work, a run that outlives a dead agent with
/// nobody owning it, and an adopted run whose wall time starts again from zero.
#[test]
fn a_restarted_runtime_adopts_a_live_run_and_reports_its_exit() {
    let (fake, dir, wall) = started_and_dropped("adopt");
    wall.set(61_000);
    let mut rt = next(&fake, &wall, TERM + 1);
    assert!(rt.reclaim().is_empty());
    let found = rt.list_labelled().unwrap();
    let want = Found {
        id: RunHandle("7.0".to_owned()),
        labels: labels("r1"),
        running_ms: 60_000,
        never_started: false,
    };
    assert_eq!(found, [want]);
    assert_eq!(rt.list_labelled().unwrap().len(), 1, "listed once");
    let id = RunHandle("7.0".to_owned());
    assert_eq!(rt.poll(&id).unwrap(), Status::Running);
    fake.exit("7.0", 3);
    let exit = Exit {
        code: 3,
        oom_killed: false,
    };
    assert_eq!(settle(&mut rt, &id), Status::Exited(exit));
    rt.remove(&id).unwrap();
    let verbs = fake.verbs();
    assert!(verbs.contains(&"watch 7.0".to_owned()), "{verbs:?}");
    assert_eq!(
        verbs[verbs.len() - 2..],
        ["kill-uid 7.0", "user-delete 7.0"]
    );
    assert!(
        Journal::new(&dir.path("native-leases"))
            .live()
            .unwrap()
            .is_empty()
    );
}

/// Asserts that a run whose process the helper no longer has (it restarted, or its user is
/// gone) is listed ended and removed, which ends its lease; and that a helper from before
/// `watch` gives the same. It catches a journal entry leaked for good, and a listing that
/// fails for one run that cannot be found.
#[test]
fn a_run_the_helper_no_longer_has_is_listed_ended() {
    for old in [false, true] {
        let (fake, dir, wall) = started_and_dropped(&format!("adopt-gone-{old}"));
        match old {
            true => fake.be_old(),
            false => fake.forget_runs(),
        }
        let mut rt = next(&fake, &wall, TERM + 1);
        let found = rt.list_labelled().unwrap();
        assert_eq!(found.len(), 1, "{old}");
        let id = found[0].id.clone();
        assert_eq!(rt.poll(&id).unwrap(), Status::Gone, "{old}");
        rt.remove(&id).unwrap();
        let journal = Journal::new(&dir.path("native-leases"));
        assert!(journal.live().unwrap().is_empty(), "{old}");
    }
}

/// Asserts that a helper that cannot be reached fails the listing (the agent cannot say what
/// it holds, and must not say it holds nothing), and that a journal that cannot be read
/// fails it too. It catches an agent that registers with an empty list while runs go on.
#[test]
fn a_listing_that_cannot_ask_the_helper_or_read_the_journal_fails() {
    let (fake, dir, wall) = started_and_dropped("adopt-lost");
    let mut rt = next(&fake, &wall, TERM + 1);
    drop(fake);
    let error = rt.list_labelled().unwrap_err();
    assert!(error.0.starts_with("watch: "), "{}", error.0);
    let journal = dir.path("native-leases");
    std::fs::remove_file(&journal).unwrap();
    std::fs::create_dir(&journal).unwrap();
    let error = rt.list_labelled().unwrap_err();
    assert!(error.0.starts_with("the lease journal: "), "{}", error.0);
}

/// Asserts what the journal keeps of a run and how it reads back: the record of a live lease
/// is listed, the record of a lease marked done is not, a retirement keeps the record of a
/// lease it keeps, and a torn record line is skipped. It catches a retirement that drops the
/// records of the runs it keeps (they would then be killed at the next start), and a torn
/// line that stops the whole journal from being read.
#[test]
fn the_journal_keeps_run_records_of_live_leases() {
    let (_fake, dir, _wall) = started_and_dropped("adopt-journal");
    let path = dir.path("native-leases");
    let journal = Journal::new(&path);
    let runs = journal.runs().unwrap();
    let record = runs["7.0"].clone();
    assert_eq!((record.pid, record.started_ms), (4242, 1000));
    assert_eq!(record.labels, labels("r1"));
    journal.add("7.5").unwrap();
    journal.started("7.5", &record).unwrap();
    journal.done("7.5").unwrap();
    journal.add("7.6").unwrap();
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .and_then(|mut file| std::io::Write::write_all(&mut file, b"= 7.6 {\"pid\":"))
        .unwrap();
    assert_eq!(journal.runs().unwrap().keys().collect::<Vec<_>>(), ["7.0"]);
    journal.retire(&["7.6".to_owned()]).unwrap();
    assert_eq!(journal.live().unwrap(), ["7.0"]);
    assert_eq!(journal.runs().unwrap()["7.0"], record);
}

/// The lifecycle configuration of `dir` again, over the directories an earlier lifecycle of
/// the same test made (`crate::lifecycle::config_in` makes them).
fn reopened(dir: &super::Scratch) -> js_agent::lifecycle::Config {
    use js_agent::lifecycle::{Capacity, Config, FilesRoot, MountRoots, ScratchRoot, StateRoot};
    Config {
        capacity: Capacity {
            cpu_millis: 4000,
            memory_bytes: 8 << 30,
            disk_bytes: 10 << 30,
            gpus: std::collections::BTreeSet::from([0, 1]),
        },
        allow_host_jobs: false,
        mounts: MountRoots::new(&[("data".to_owned(), dir.path("data"))]).unwrap(),
        scratch: ScratchRoot::open(&dir.path("scratch")).unwrap(),
        files: Some(FilesRoot::open(&dir.path("files")).unwrap()),
        state: StateRoot::open(&dir.path("state")).unwrap(),
        stop_grace: crate::lifecycle::GRACE,
        bundle: None,
        quota: None,
    }
}

/// Asserts the restart end to end: a native job's lifecycle and runtime are dropped while it
/// runs, and the lifecycle of the next process adopts it (an `adopted` event, the job held
/// from the first heartbeat), then reports its exit as the job's result through the normal
/// exit path and removes its user. It catches an agent update that cuts running work.
#[test]
fn a_restarted_lifecycle_adopts_a_native_job_and_reports_its_result() {
    use js_agent::lifecycle::{Event, Lifecycle};
    use js_agent::native::NativeChecks;
    use js_agent_wire::Outcome;

    let mut h = super::lifecycle::Native::new("adopt-life");
    assert_eq!(
        h.start(super::lifecycle::job("r1", "/bin/sleep")),
        ["started r1"]
    );
    let super::lifecycle::Native {
        life,
        fake,
        clock,
        dir,
    } = h;
    drop(life);
    fake.refuse("boot-reset", RESET_ALREADY);
    let runtime = NativeRuntime::open(fake.client(), config(&fake), TERM + 1).unwrap();
    let (mut life, events) = Lifecycle::adopt(
        reopened(&dir),
        Box::new(runtime),
        Box::new(NativeChecks),
        Box::new(clock.clone()),
    )
    .unwrap();
    let adopted = Event::Adopted {
        run_id: "r1".to_owned(),
    };
    assert!(events.contains(&adopted), "{events:?}");
    assert_eq!(life.held().len(), 1);
    fake.exit("7.0", 0);
    for _ in 0..super::POLLS {
        life.step();
        if !life.results().is_empty() {
            break;
        }
        std::thread::yield_now();
    }
    let result = &life.results()[0];
    assert_eq!(
        (result.run_id.as_str(), result.outcome),
        ("r1", Outcome::Succeeded)
    );
    for _ in 0..super::POLLS {
        life.step();
        if fake
            .verbs()
            .last()
            .is_some_and(|verb| verb == "user-delete 7.0")
        {
            return;
        }
        std::thread::yield_now();
    }
    panic!(
        "the adopted run's user was never removed: {:?}",
        fake.verbs()
    );
}
