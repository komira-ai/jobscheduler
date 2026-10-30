//! The worker's side of the usage over fake cgroup files and a clock the test sets: when a
//! window has passed, what a run publishes and what it sends to the session.

use std::cell::{Cell, RefCell};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use js_agent::FileReader;
use js_agent::lifecycle::RunningLease;
use js_agent::usage::{Collector, MAX_UNSENT_AGE_USEC, Outbox, UsageClock};
use js_agent_wire::{LeaseMinute, NodeLoad};

use crate::support::FakeFiles;

const ACTIONS: &str = "/sys/fs/cgroup/jobs.slice";
const SECOND: u64 = 1_000_000;

/// The files the collector reads, which the test goes on writing.
#[derive(Clone, Default)]
struct Shared(Rc<RefCell<FakeFiles>>);

impl FileReader for Shared {
    fn read(&self, path: &str) -> Result<String, String> {
        self.0.borrow().read(path)
    }

    fn link(&self, path: &str) -> Result<String, String> {
        self.0.borrow().link(path)
    }

    fn device(&self, path: &Path) -> Result<u64, String> {
        self.0.borrow().device(path)
    }

    fn is_tmpfs(&self, path: &Path) -> Result<bool, String> {
        self.0.borrow().is_tmpfs(path)
    }
}

/// A clock the test sets.
#[derive(Clone, Default)]
struct SetClock(Rc<Cell<u64>>);

impl UsageClock for SetClock {
    fn now_usec(&self) -> u64 {
        self.0.get()
    }
}

/// A collector with a 30 second window, the files it reads and the clock it is timed by.
struct Rig {
    files: Shared,
    clock: SetClock,
    outbox: Arc<Outbox>,
    collector: Collector,
}

impl Rig {
    fn new() -> Self {
        let (files, clock) = (Shared::default(), SetClock::default());
        let outbox = Arc::new(Outbox::new());
        let collector = Collector::new(
            Box::new(files.clone()),
            ACTIONS,
            Box::new(clock.clone()),
            outbox.clone(),
        );
        Self {
            files,
            clock,
            outbox,
            collector,
        }
    }

    /// A rig whose session has set the 30 second window.
    fn windowed() -> Self {
        let rig = Self::new();
        rig.outbox.set_window(Duration::from_secs(30));
        rig
    }

    fn put(&self, path: &str, text: &str) {
        let mut files = self.files.0.borrow_mut();
        *files = files.clone().with(path, text);
    }

    /// The node's files: `usage` microseconds of CPU, `current` bytes of memory.
    fn node(&self, usage: u64, current: u64) {
        self.dir(ACTIONS, usage, current);
        self.put(&format!("{ACTIONS}/cpu.max"), "max 100000\n");
    }

    /// The files of the lease `id`.
    fn lease(&self, id: &str, usage: u64, current: u64) {
        self.dir(&format!("{ACTIONS}/{id}"), usage, current);
    }

    fn dir(&self, dir: &str, usage: u64, current: u64) {
        self.put(&format!("{dir}/cpu.stat"), &format!("usage_usec {usage}\n"));
        self.put(&format!("{dir}/memory.current"), &format!("{current}\n"));
        self.put(
            &format!("{dir}/memory.stat"),
            "inactive_file 0\nactive_file 5\n",
        );
    }

    /// Runs the collector at `seconds` on the clock.
    fn run_at(&mut self, seconds: u64, running: &[RunningLease]) -> Vec<String> {
        self.clock.0.set(seconds * SECOND);
        self.collector.run(running)
    }

    fn load(&self) -> Option<NodeLoad> {
        self.outbox.take()
    }
}

/// A lease that runs in `ACTIONS/<id>` and started at `seconds`.
fn running(id: &str, seconds: u64) -> RunningLease {
    RunningLease {
        lease_id: id.to_owned(),
        cgroup: Some(PathBuf::from(format!("{ACTIONS}/{id}"))),
        started_usec: Some(seconds * SECOND),
        adopted: false,
    }
}

fn names(minutes: &[LeaseMinute]) -> Vec<(&str, u16, u32)> {
    let each = minutes.iter();
    each.map(|m| (m.lease_id.as_str(), m.minute, m.cpu_mean_millis))
        .collect()
}

/// Asserts when a window has passed: not before the session has set it, always for the
/// first run once it has, and then exactly when the window has elapsed since the last run
/// (one microsecond short is not due, the full window is). It catches a worker that samples
/// at its own pace instead of the heartbeat's, which would send a beat with no new reading
/// or two readings in a window.
#[test]
fn a_run_is_due_when_the_window_has_passed() {
    let mut rig = Rig::new();
    assert!(!rig.collector.due(), "no window yet");
    rig.outbox.set_window(Duration::from_secs(30));
    assert!(rig.collector.due(), "never run");
    rig.node(0, 100);
    rig.run_at(100, &[]);
    rig.clock.0.set(130 * SECOND - 1);
    assert!(!rig.collector.due());
    rig.clock.0.set(130 * SECOND);
    assert!(rig.collector.due());
}

/// Asserts that `seq` counts the samples from 1 and that a new collector (a new process)
/// starts again at 1. It catches a sequence that continues across a restart (the server
/// would take the new process for a replay) and one that skips.
#[test]
fn seq_counts_samples_and_restarts_with_the_process() {
    let mut rig = Rig::windowed();
    rig.node(0, 100);
    let mut seen = Vec::new();
    for at in [0, 30, 60] {
        assert!(rig.run_at(at, &[]).is_empty());
        seen.push(rig.load().unwrap().seq);
    }
    assert_eq!(seen, [1, 2, 3]);

    let mut again = Rig::windowed();
    again.node(0, 100);
    again.run_at(0, &[]);
    assert_eq!(again.load().unwrap().seq, 1);
}

/// Asserts the node's fields and that `covered` lists only the leases that ran the whole
/// window: the first sample of a lease is its baseline, so a lease that appears in a window
/// is covered from the next one. It catches a lease charged for a window it was not there
/// for and a node reading taken from the leases.
#[test]
fn covered_lists_only_leases_that_ran_the_whole_window() {
    let mut rig = Rig::windowed();
    rig.node(1_000, 5_000);
    rig.lease("a", 10, 700);
    rig.run_at(0, &[running("a", 0)]);
    let first = rig.load().unwrap();
    assert_eq!(
        (
            first.cpu_usage_usec,
            first.working_set_bytes,
            first.mono_usec
        ),
        (1_000, 5_000, 0)
    );
    assert!(first.covered.is_empty(), "a baseline covers nothing");

    rig.node(2_000, 5_000);
    rig.lease("a", 20, 700);
    rig.lease("b", 5, 300);
    rig.run_at(30, &[running("a", 0), running("b", 30)]);
    assert_eq!(rig.load().unwrap().covered, ["a"]);

    rig.lease("a", 30, 700);
    rig.lease("b", 15, 300);
    rig.run_at(60, &[running("a", 0), running("b", 30)]);
    assert_eq!(rig.load().unwrap().covered, ["a", "b"]);
}

/// Asserts that an adopted lease is named in the clocks but never cut into minutes (nor
/// covered), while a lease started by this process beside it is. It catches minutes sent
/// for a run whose first part this process never saw (the server would learn a curve with
/// its early minutes missing).
#[test]
fn an_adopted_lease_gets_a_clock_but_no_minutes() {
    let mut rig = Rig::windowed();
    let adopted = RunningLease {
        adopted: true,
        ..running("old", 0)
    };
    let both = [running("new", 0), adopted];
    let mut minutes = Vec::new();
    for (at, usage) in [(0, 0), (30, 30), (60, 60), (90, 90)] {
        rig.node(usage * SECOND, 100);
        rig.lease("new", usage * SECOND, 100);
        rig.lease("old", usage * SECOND, 100);
        rig.run_at(at, &both);
        let load = rig.load().unwrap();
        let clocks: Vec<(&str, u64)> = load
            .clocks
            .iter()
            .map(|c| (c.lease_id.as_str(), c.started_mono_usec))
            .collect();
        assert_eq!(clocks, [("new", 0), ("old", 0)]);
        assert!(load.covered.iter().all(|id| id == "new"), "{load:?}");
        minutes.extend(load.minutes);
    }
    assert_eq!(names(&minutes), [("new", 0, 1000)]);
}

/// Asserts that a lease that is no longer running has its open minute closed and sent
/// once, while a lease that goes on is not touched. It catches the last, partial minute of
/// a lease being lost, which is the one most runs end in.
#[test]
fn a_lease_that_ended_sends_its_partial_minute() {
    let mut rig = Rig::windowed();
    for (at, k) in [(0, 0), (30, 1), (60, 2), (90, 3)] {
        rig.node(k * 30 * SECOND, 100);
        rig.lease("a", k * 30 * SECOND, 100);
        rig.run_at(at, &[running("a", 0)]);
        let minutes = rig.load().unwrap().minutes;
        rig.outbox.delivered();
        match at {
            60 => assert_eq!(names(&minutes), [("a", 0, 1000)]),
            _ => assert!(minutes.is_empty(), "t={at}: {minutes:?}"),
        }
    }
    rig.node(4 * 30 * SECOND, 100);
    rig.run_at(120, &[]);
    assert_eq!(names(&rig.load().unwrap().minutes), [("a", 1, 1000)]);
    rig.outbox.delivered();
    rig.run_at(150, &[]);
    assert!(rig.load().unwrap().minutes.is_empty(), "sent once");
}

/// Asserts what a node that cannot be read does: no load for that window, one line saying
/// why, no second line for the same reason, a new line for another reason, and a load again
/// as soon as it can be read (a later failure is told again). It catches a zero load
/// invented for an unreadable node, and a log line every window.
#[test]
fn an_unreadable_node_gives_no_load_and_one_line() {
    let mut rig = Rig::windowed();
    let first = rig.run_at(0, &[]);
    assert_eq!(
        first,
        ["usage: no load while the node cpu.stat cannot be read"]
    );
    assert!(
        rig.run_at(30, &[]).is_empty(),
        "the same reason is not told again"
    );
    assert!(rig.load().is_none());

    rig.put(&format!("{ACTIONS}/cpu.stat"), "usage_usec 1\n");
    let other = rig.run_at(60, &[]);
    assert_eq!(
        other,
        ["usage: no load while the node memory.current cannot be read"]
    );

    rig.node(1, 100);
    assert!(rig.run_at(90, &[]).is_empty());
    assert_eq!(rig.load().unwrap().seq, 1, "only samples count");
    let files = rig
        .files
        .0
        .borrow()
        .clone()
        .without(&format!("{ACTIONS}/cpu.stat"));
    *rig.files.0.borrow_mut() = files;
    assert_eq!(rig.run_at(120, &[]).len(), 1, "told again after it worked");
}

/// Asserts that the minutes of a failed beat ride on a later one exactly once: the beat
/// that carried a closed minute fails, the next reading carries the minute again, and after
/// that beat is delivered the one after carries only the minute closed since. It catches
/// minutes lost with a failed send and minutes sent for ever.
#[test]
fn minutes_of_a_failed_beat_are_resent_once() {
    let mut rig = Rig::windowed();
    let at = |rig: &mut Rig, seconds: u64| {
        let k = seconds / 30;
        rig.node(k * 30 * SECOND, 100);
        rig.lease("a", k * 30 * SECOND, 100);
        rig.run_at(seconds, &[running("a", 0)]);
    };
    for seconds in [0, 30] {
        at(&mut rig, seconds);
        rig.load();
        rig.outbox.delivered();
    }
    at(&mut rig, 60);
    assert_eq!(names(&rig.load().unwrap().minutes), [("a", 0, 1000)]);
    rig.outbox.failed();
    at(&mut rig, 90);
    assert_eq!(names(&rig.load().unwrap().minutes), [("a", 0, 1000)]);
    rig.outbox.delivered();
    at(&mut rig, 120);
    assert_eq!(names(&rig.load().unwrap().minutes), [("a", 1, 1000)]);
}

/// Asserts that minutes kept through an outage longer than the binner's bound are dropped
/// and counted, not sent late. It catches an outbox that keeps minutes for ever.
#[test]
fn minutes_kept_through_a_long_outage_are_dropped() {
    let mut rig = Rig::windowed();
    for (at, k) in [(0, 0), (30, 1), (60, 2)] {
        rig.node(k * 30 * SECOND, 100);
        rig.lease("a", k * 30 * SECOND, 100);
        rig.run_at(at, &[running("a", 0)]);
        rig.load();
        rig.outbox.failed();
    }
    let later = 60 + MAX_UNSENT_AGE_USEC / SECOND + 1;
    rig.node(9 * 30 * SECOND, 100);
    rig.lease("a", 9 * 30 * SECOND, 100);
    let lines = rig.run_at(later, &[running("a", 0)]);
    assert_eq!(rig.outbox.dropped(), 1);
    assert_eq!(
        lines,
        ["usage: dropped 1 minute(s) too old to send or refused by the server (1 since start)"]
    );
    assert!(
        !rig.load().unwrap().minutes.iter().any(|m| m.minute == 0),
        "the minute that rode with the sample of t=60 is too old"
    );
}

/// Asserts the bounds of a load: with 100 running leases only the 96 oldest are named in the
/// clocks, oldest first, and only 96 are covered. It catches a heartbeat that grows with the
/// number of leases past the wire's limit, which the server would refuse.
#[test]
fn clocks_and_covered_are_capped_at_the_wire_limit() {
    let mut rig = Rig::windowed();
    let leases: Vec<RunningLease> = (0..100)
        .rev()
        .map(|i| running(&format!("l{i:03}"), i))
        .collect();
    for at in [100, 130] {
        rig.node(0, 100);
        for i in 0..100 {
            rig.lease(&format!("l{i:03}"), 0, 100);
        }
        rig.run_at(at, &leases);
    }
    rig.load();
    rig.run_at(160, &leases);
    let load = rig.load().unwrap();
    assert_eq!(load.clocks.len(), 96);
    assert_eq!(load.clocks[0].lease_id, "l000");
    assert_eq!(load.clocks[95].lease_id, "l095");
    assert_eq!(load.covered.len(), 96);
}

/// Asserts which leases are sampled and which are named: a lease with no control group is
/// named but not sampled, one whose control group is not text is named but not sampled, and
/// one with no start is neither. It catches a lease sampled from a directory nobody gave, and
/// a clock sent for a start nobody knows.
#[test]
fn a_lease_without_a_cgroup_or_a_start_is_not_sampled() {
    let mut rig = Rig::windowed();
    let bare = RunningLease {
        cgroup: None,
        ..running("bare", 5)
    };
    let odd = RunningLease {
        cgroup: Some(PathBuf::from(OsStr::from_bytes(b"/sys/fs/cgroup/\xff"))),
        ..running("odd", 6)
    };
    let unstamped = RunningLease {
        started_usec: None,
        ..running("unstamped", 0)
    };
    rig.node(0, 100);
    rig.run_at(10, &[bare, odd, unstamped]);
    let load = rig.load().unwrap();
    let named: Vec<&str> = load.clocks.iter().map(|c| c.lease_id.as_str()).collect();
    assert_eq!(named, ["bare", "odd"]);
    assert!(load.covered.is_empty());
}

/// Asserts that a boxed reader answers every question as the reader inside does. It catches
/// a collector (which holds its files as a box) reading something other than what the
/// machine gave it.
#[test]
fn a_boxed_reader_reads_like_the_reader_inside() {
    let inner = FakeFiles::default()
        .with("/a", "text")
        .with_link("/l", "target")
        .disk_under(Path::new("/disk"));
    let boxed: Box<dyn FileReader> = Box::new(inner.clone());
    assert_eq!(boxed.read("/a"), Ok("text".to_owned()));
    assert!(boxed.read("/b").is_err());
    assert_eq!(boxed.link("/l"), Ok("target".to_owned()));
    assert_eq!(boxed.device(Path::new("/a")), inner.device(Path::new("/a")));
    assert_eq!(boxed.is_tmpfs(Path::new("/run/x")), Ok(true));
    assert_eq!(boxed.is_tmpfs(Path::new("/disk/x")), Ok(false));
}

/// Asserts that dropped minutes are reported to the operator, since the wire has no field
/// for them: the run after a refused beat says how many minutes were dropped and how many
/// in all, a run with nothing new says nothing, and a second refusal is a second line with
/// the new count. It catches minutes lost without a trace, and the same loss told in every
/// window.
#[test]
fn dropped_minutes_are_told_once_with_their_count() {
    let mut rig = Rig::windowed();
    let at = |rig: &mut Rig, seconds: u64| {
        let k = seconds / 30;
        rig.node(k * 30 * SECOND, 100);
        rig.lease("a", k * 30 * SECOND, 100);
        rig.run_at(seconds, &[running("a", 0)])
    };
    for seconds in [0, 30] {
        assert!(at(&mut rig, seconds).is_empty());
        rig.load();
        rig.outbox.delivered();
    }
    assert!(at(&mut rig, 60).is_empty());
    assert_eq!(names(&rig.load().unwrap().minutes), [("a", 0, 1000)]);
    rig.outbox.refused();
    let told = at(&mut rig, 90);
    assert_eq!(
        told,
        ["usage: dropped 1 minute(s) too old to send or refused by the server (1 since start)"]
    );
    rig.load();
    rig.outbox.delivered();
    assert!(at(&mut rig, 120).is_empty(), "nothing new, nothing said");
    rig.load();
    rig.outbox.refused();
    let again = at(&mut rig, 150);
    assert_eq!(
        again,
        ["usage: dropped 1 minute(s) too old to send or refused by the server (2 since start)"]
    );
}

/// Asserts that the collector goes on when a thread died holding the outbox's lock: the
/// run still publishes its reading and the next window still reclaims. It catches a lock
/// that panics on poison, which would end the worker thread, and with it every job.
#[test]
fn a_poisoned_outbox_does_not_stop_the_collector() {
    let mut rig = Rig::windowed();
    let poisoner = rig.outbox.clone();
    assert!(
        std::thread::spawn(move || poisoner.panic_while_locked())
            .join()
            .is_err()
    );
    rig.node(0, 100);
    assert!(rig.collector.due());
    assert!(rig.run_at(0, &[]).is_empty());
    assert_eq!(rig.load().unwrap().seq, 1);
    rig.node(30 * SECOND, 100);
    assert!(rig.run_at(30, &[]).is_empty());
    assert_eq!(rig.load().unwrap().seq, 2);
}

/// Asserts that a later session's heartbeat interval is the window the collector samples
/// and bins at: after the interval went from 30 to 5 seconds, a lease sampled 20 seconds
/// after its last sample is a gap (more than three windows) and is not covered, where with
/// the first interval it would have been. It catches a binner that keeps the interval of the
/// first session for the life of the process.
#[test]
fn the_window_follows_the_session_that_is_welcomed_last() {
    let mut rig = Rig::windowed();
    let at = |rig: &mut Rig, seconds: u64| {
        rig.node(seconds * SECOND, 100);
        rig.lease("a", seconds * SECOND, 100);
        rig.run_at(seconds, &[running("a", 0)]);
        rig.load().unwrap().covered
    };
    at(&mut rig, 0);
    assert_eq!(at(&mut rig, 30), ["a"], "30 seconds is one window");

    rig.outbox.set_window(Duration::from_secs(5));
    assert!(
        at(&mut rig, 50).is_empty(),
        "20 seconds is four windows of 5: a gap"
    );
    assert_eq!(at(&mut rig, 55), ["a"], "one window of 5 again");
}
