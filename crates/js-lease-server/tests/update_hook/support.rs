//! What the tests of the server's update hook share: a directory tree with a state directory,
//! the declaration and the params of an update from 1.0.0 to 2.0.0, and fakes of every port
//! that keep one call log: the unit manager, the release link, the store, `/healthz`, a clock
//! that moves only when a step pauses, and a gate that can end a step at any kill point.
//!
//! Each test binary module uses some of it, so the unused rest is not an error: the allow
//! below is the stated reason, one rig for every file of the binary.
#![allow(dead_code)]

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::fs;
use std::io;
use std::os::fd::BorrowedFd;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use js_lease_server::update_hook::step::{self, Env};
use js_lease_server::update_hook::{
    Clock, DEFAULT_UNIT, Declaration, Done, Gate, Health, Link, MIN_TIMEOUTS, Params, Ports,
    Record, Schema, Step, StoreUnreachable, StoreView, Units, Writer,
};
use js_testkit::dir::TestDir;
use js_update_hook::{DirStat, PortError, UnitState, peek};

/// The unit the declaration names.
pub const UNIT: &str = DEFAULT_UNIT;
/// The clock's time when a rig is made, in milliseconds.
pub const START_MS: u64 = 1_000_000;
/// The retry hint a deferred `prepare` gives.
pub const RETRY_AFTER_SECS: u64 = 60;

/// A state directory that is as the hook requires, whoever runs the test.
pub fn as_root(_: &Path, _: BorrowedFd<'_>) -> io::Result<DirStat> {
    Ok(DirStat {
        is_symlink: false,
        is_dir: true,
        uid: 0,
        gid: 0,
        mode: 0o700,
    })
}

/// The calls the fakes were asked to make a change with, in order: `stop`, `start`,
/// `relink <target>`. Looks (a unit's state, a link's target, a probe) are not in it.
pub type Log = Rc<RefCell<Vec<String>>>;

/// The unit manager: a unit whose state moves as a stop or a start says, `stop_shows` and
/// `start_shows` looks after the call, and whose calls can fail.
pub struct FakeUnits {
    log: Log,
    /// `ActiveState`, `SubState` now.
    pub state: RefCell<(String, String)>,
    /// The state a stop leads to, and after how many looks.
    pub after_stop: RefCell<((String, String), u32)>,
    /// The state a start leads to, and after how many looks.
    pub after_start: RefCell<((String, String), u32)>,
    pending: RefCell<Option<((String, String), u32)>>,
    /// Every `show` fails.
    pub show_fails: Cell<bool>,
    /// `stop_no_block` fails.
    pub stop_fails: Cell<bool>,
    /// `start` fails.
    pub start_fails: Cell<bool>,
}

/// A unit state.
pub fn st(active: &str, sub: &str) -> (String, String) {
    (active.to_owned(), sub.to_owned())
}

impl FakeUnits {
    fn new(log: Log) -> FakeUnits {
        FakeUnits {
            log,
            state: RefCell::new(st("active", "running")),
            after_stop: RefCell::new((st("inactive", "dead"), 0)),
            after_start: RefCell::new((st("active", "running"), 0)),
            pending: RefCell::new(None),
            show_fails: Cell::new(false),
            stop_fails: Cell::new(false),
            start_fails: Cell::new(false),
        }
    }

    /// Sets the state now.
    pub fn set(&self, active: &str, sub: &str) {
        *self.state.borrow_mut() = st(active, sub);
        *self.pending.borrow_mut() = None;
    }
}

impl Units for FakeUnits {
    fn show(&self, unit: &str) -> Result<UnitState, PortError> {
        assert_eq!(unit, UNIT);
        if self.show_fails.get() {
            return Err(PortError::Command("systemctl: exit status 1".to_owned()));
        }
        let mut pending = self.pending.borrow_mut();
        if let Some((next, looks)) = pending.take() {
            match looks {
                0 => *self.state.borrow_mut() = next,
                n => *pending = Some((next, n - 1)),
            }
        }
        let (active, sub) = self.state.borrow().clone();
        Ok(UnitState { active, sub })
    }

    fn stop_no_block(&self, unit: &str) -> Result<(), PortError> {
        assert_eq!(unit, UNIT);
        self.log.borrow_mut().push("stop".to_owned());
        if self.stop_fails.get() {
            return Err(PortError::Command("systemctl: exit status 5".to_owned()));
        }
        *self.pending.borrow_mut() = Some(self.after_stop.borrow().clone());
        Ok(())
    }

    fn start(&self, unit: &str) -> Result<(), PortError> {
        assert_eq!(unit, UNIT);
        self.log.borrow_mut().push("start".to_owned());
        if self.start_fails.get() {
            return Err(PortError::Command("systemctl: exit status 1".to_owned()));
        }
        *self.pending.borrow_mut() = Some(self.after_start.borrow().clone());
        Ok(())
    }

    fn is_active(&self, _: &str) -> Result<bool, PortError> {
        unreachable!("the server's hook reads a unit with show")
    }
}

/// The release link, in memory.
pub struct FakeLink {
    log: Log,
    /// Where it points; `None` when there is no link.
    pub target: RefCell<Option<PathBuf>>,
    /// Reading it fails: the path is not a symbolic link.
    pub read_fails: Cell<bool>,
    /// Moving it fails.
    pub relink_fails: Cell<bool>,
}

impl Link for FakeLink {
    fn target(&self, _: &Path) -> Result<Option<PathBuf>, PortError> {
        match self.read_fails.get() {
            true => Err(PortError::Io("Invalid argument (os error 22)".to_owned())),
            false => Ok(self.target.borrow().clone()),
        }
    }

    fn relink(&self, _: &Path, target: &Path) -> Result<(), PortError> {
        self.log
            .borrow_mut()
            .push(format!("relink {}", target.display()));
        if self.relink_fails.get() {
            return Err(PortError::Io("Permission denied (os error 13)".to_owned()));
        }
        *self.target.borrow_mut() = Some(target.to_owned());
        Ok(())
    }
}

/// An answer of the fake store's probe.
pub type Probe = Result<Writer, StoreUnreachable>;

/// A lock held by `holder`.
pub fn held(holder: &str) -> Probe {
    Ok(Writer::Held {
        holder: holder.to_owned(),
    })
}

/// A lock nobody holds.
pub fn free() -> Probe {
    Ok(Writer::Free)
}

/// The store: its probes answer from a queue whose last answer stays.
pub struct FakeStore {
    /// The answers still to give, the last one given forever.
    pub probes: RefCell<VecDeque<Probe>>,
    /// How many probes were made.
    pub probed: Cell<u32>,
    /// The schema's answer.
    pub schema: RefCell<Result<Schema, StoreUnreachable>>,
}

impl FakeStore {
    /// Answers `answers` in order, then the last of them forever.
    pub fn answer(&self, answers: impl IntoIterator<Item = Probe>) {
        *self.probes.borrow_mut() = answers.into_iter().collect();
    }
}

impl StoreView for FakeStore {
    fn writer(&self) -> Result<Writer, StoreUnreachable> {
        self.probed.set(self.probed.get() + 1);
        let mut probes = self.probes.borrow_mut();
        match probes.len() {
            0 => panic!("the store was probed with no answer set"),
            1 => probes[0].clone(),
            _ => probes.pop_front().expect("an answer"),
        }
    }

    fn schema(&self) -> Result<Schema, StoreUnreachable> {
        *self.schema.borrow()
    }
}

/// `/healthz`: answers from a queue whose last answer stays.
pub struct FakeHealth {
    /// The answers still to give.
    pub answers: RefCell<VecDeque<Result<u16, String>>>,
    /// How many looks were made.
    pub looked: Cell<u32>,
}

impl Health for FakeHealth {
    fn status(&self) -> Result<u16, String> {
        self.looked.set(self.looked.get() + 1);
        let mut answers = self.answers.borrow_mut();
        match answers.len() {
            0 => panic!("/healthz was asked with no answer set"),
            1 => answers[0].clone(),
            _ => answers.pop_front().expect("an answer"),
        }
    }
}

/// Time that moves only when a step pauses.
pub struct FakeClock {
    /// Milliseconds since the epoch.
    pub now: Cell<u64>,
    /// How many pauses were made.
    pub pauses: Cell<u32>,
}

impl Clock for FakeClock {
    fn now_ms(&self) -> u64 {
        self.now.get()
    }

    fn pause(&self, span: Duration) {
        self.pauses.set(self.pauses.get() + 1);
        let ms = u64::try_from(span.as_millis()).expect("a short pause");
        self.now.set(self.now.get() + ms);
    }
}

/// The kill points: counts them, and panics at the one it is armed for (a process killed
/// there, from the step's point of view: nothing after it runs).
#[derive(Default)]
pub struct Kill {
    points: Cell<u32>,
    at: Cell<Option<u32>>,
}

impl Kill {
    /// The points passed since the last arm or disarm.
    pub fn points(&self) -> u32 {
        self.points.get()
    }

    /// Counts from zero again and dies at the `n`th point (from 1) of the next run.
    pub fn arm(&self, n: u32) {
        self.points.set(0);
        self.at.set(Some(n));
    }

    /// Counts from zero again and never dies.
    pub fn disarm(&self) {
        self.points.set(0);
        self.at.set(None);
    }
}

impl Gate for Kill {
    fn point(&self) {
        let n = self.points.get() + 1;
        self.points.set(n);
        if self.at.get() == Some(n) {
            panic!("killed at kill point {n}");
        }
    }
}

/// An update from release 1.0.0 to 2.0.0 of a server running as [`UNIT`], over fakes: the
/// unit is up on 1.0.0, the link points there, the store answers schema 2 and the lock is
/// held by `old`, `/healthz` answers 200.
pub struct Rig {
    /// Keeps the tree for the rig's life.
    pub dir: TestDir,
    /// The calls of the unit manager and the link.
    pub log: Log,
    pub units: FakeUnits,
    pub link: FakeLink,
    pub store: FakeStore,
    pub health: FakeHealth,
    pub clock: FakeClock,
    pub kill: Kill,
    pub declaration: Declaration,
    pub params: Params,
}

impl Rig {
    /// A rig in a directory of its own, named for the test.
    pub fn new(name: &str) -> Rig {
        let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), name);
        let root = dir.path().to_owned();
        for sub in ["releases/1.0.0", "releases/2.0.0", "link", "state"] {
            fs::create_dir_all(root.join(sub)).expect("the tree");
        }
        let log: Log = Rc::default();
        let declaration = Declaration {
            state_dir: root.join("state"),
            unit: UNIT.to_owned(),
            write_paths: vec![root.join("releases"), root.join("link"), root.join("state")],
            timeouts: MIN_TIMEOUTS,
        };
        let params = Params {
            staged_release: root.join("releases/2.0.0"),
            previous_release: Some(root.join("releases/1.0.0")),
            target_version: "2.0.0".to_owned(),
            previous_version: Some("1.0.0".to_owned()),
            defer_retry_secs: RETRY_AFTER_SECS,
            release_link: Some(root.join("link/current")),
            unit: UNIT.to_owned(),
        };
        Rig {
            units: FakeUnits::new(Rc::clone(&log)),
            link: FakeLink {
                log: Rc::clone(&log),
                target: RefCell::new(Some(root.join("releases/1.0.0"))),
                read_fails: Cell::new(false),
                relink_fails: Cell::new(false),
            },
            store: FakeStore {
                probes: RefCell::new(VecDeque::from([held("old")])),
                probed: Cell::new(0),
                schema: RefCell::new(Ok(Schema::Runs(2))),
            },
            health: FakeHealth {
                answers: RefCell::new(VecDeque::from([Ok(200)])),
                looked: Cell::new(0),
            },
            clock: FakeClock {
                now: Cell::new(START_MS),
                pauses: Cell::new(0),
            },
            kill: Kill::default(),
            log,
            dir,
            declaration,
            params,
        }
    }

    /// A first install of 2.0.0: no previous release, no link, the unit down, nobody holding
    /// the lock.
    pub fn first_install(name: &str) -> Rig {
        let mut rig = Rig::new(name);
        rig.params.previous_release = None;
        rig.params.previous_version = None;
        *rig.link.target.borrow_mut() = None;
        rig.units.set("inactive", "dead");
        rig.store.answer([free()]);
        rig
    }

    /// A path in the rig's tree.
    pub fn path(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    /// The staged release.
    pub fn staged(&self) -> PathBuf {
        self.params.staged_release.clone()
    }

    /// The previous release.
    pub fn previous(&self) -> PathBuf {
        self.path("releases/1.0.0")
    }

    /// The environment of a step over the rig's fakes.
    pub fn env(&self) -> Env<'_> {
        Env {
            ports: Ports {
                units: &self.units,
                link: &self.link,
                store: &self.store,
                health: &self.health,
                clock: &self.clock,
                gate: &self.kill,
            },
            declaration: &self.declaration,
            stat: &as_root,
        }
    }

    /// Runs `step` to its end.
    pub fn run(&self, step: Step) -> Done {
        step::run(step, &self.env(), &self.params)
    }

    /// Runs `step`, killed at its `n`th kill point: `None` when it was killed there, its ending
    /// when it ended before.
    pub fn run_killed_at(&self, step: Step, n: u32) -> Option<Done> {
        self.kill.arm(n);
        let ended = catch_unwind(AssertUnwindSafe(|| self.run(step))).ok();
        self.kill.disarm();
        ended
    }

    /// The record as it is on disk.
    pub fn record(&self) -> Option<Record> {
        peek::<Record>(&self.declaration.state_dir, &as_root).expect("a readable record")
    }

    /// The record `prepare` writes for this rig, with `holder`.
    pub fn prepared(&self, holder: Option<&str>) -> Record {
        let mut record = Record::new(&self.params, self.params.previous_release.clone());
        record.holder = holder.map(str::to_owned);
        record.store_schema = Some(2);
        record
    }

    /// Puts `record` on disk as the record.
    pub fn put(&self, record: &Record) {
        let body = serde_json::to_vec_pretty(record).expect("a record");
        fs::write(self.declaration.state_dir.join("record.json"), body).expect("the record");
    }

    /// The calls made, and forgets them.
    pub fn calls(&self) -> Vec<String> {
        std::mem::take(&mut *self.log.borrow_mut())
    }

    /// `relink <previous>` and `relink <staged>`, as the log says them.
    pub fn relink_to(&self, target: &Path) -> String {
        format!("relink {}", target.display())
    }
}
