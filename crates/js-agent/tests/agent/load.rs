//! The load a heartbeat carries: the actions control group read through fake files, the
//! runs the worker publishes, and the body the server is sent. The clock moves when a test
//! moves it; no test waits and none asserts a duration.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::time::Duration;

use js_agent::lifecycle::RunningLease;
use js_agent::usage::SampleError;
use js_agent::{Drain, FileReader, LoadReader, Note, Session, SharedClock, Snapshot, Step, Worker};
use serde_json::{Value, json};

use crate::jobs::fake::FakeMachine;
use crate::jobs::rescue;
use crate::support::{FakeFiles, Scratch, Script, beat_ok, hello, lowest, reply, welcome_with};

const ACTIONS: &str = "/sys/fs/cgroup/actions";
const LEASE_DIR: &str = "/sys/fs/cgroup/actions/lease-37";

/// Files the test changes between heartbeats.
struct Live(RefCell<FakeFiles>);

impl Live {
    fn set(&self, files: FakeFiles) {
        *self.0.borrow_mut() = files;
    }
}

impl FileReader for Live {
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

/// The node and one lease with numbers that no unit mix-up can reproduce: the node's
/// `usage_usec` is not its memory, the active file is not the inactive one.
fn files(node_cpu: u64, lease_cpu: u64, lease_current: u64, with_current: bool) -> FakeFiles {
    let stat = |usage: u64| format!("usage_usec {usage}\nuser_usec 1\n");
    let mem = |inactive: u64, active: u64| {
        format!("anon 1\ninactive_file {inactive}\nactive_file {active}\n")
    };
    let mut files = FakeFiles::default()
        .with(&format!("{ACTIONS}/cpu.stat"), &stat(node_cpu))
        .with(&format!("{ACTIONS}/memory.stat"), &mem(1_000_000_000, 250))
        .with(&format!("{ACTIONS}/cpu.max"), "max 100000\n")
        .with(&format!("{LEASE_DIR}/cpu.stat"), &stat(lease_cpu))
        .with(
            &format!("{LEASE_DIR}/memory.current"),
            &format!("{lease_current}\n"),
        )
        .with(&format!("{LEASE_DIR}/memory.stat"), &mem(1, 0));
    if with_current {
        files = files.with(&format!("{ACTIONS}/memory.current"), "8000000000\n");
    }
    files
}

fn running(id: &str, cgroup: Option<&str>, started_usec: Option<u64>) -> RunningLease {
    RunningLease {
        lease_id: id.to_owned(),
        cgroup: cgroup.map(PathBuf::from),
        started_usec,
        adopted: false,
    }
}

/// A rig: a worker that publishes `leases`, a clock at 500 ms, and a script of replies.
struct Rig {
    machine: FakeMachine,
    worker: Worker,
    _dir: Scratch,
}

fn rig(leases: Vec<RunningLease>) -> Rig {
    let machine = FakeMachine::new();
    let dir = Scratch::new("load");
    let snapshot = Snapshot {
        leases,
        ..Snapshot::default()
    };
    let worker = Worker::orphaned(machine.clock.clone(), snapshot, rescue(&dir, &machine));
    machine.clock.advance(500);
    Rig {
        machine,
        worker,
        _dir: dir,
    }
}

fn drain() -> Drain {
    Drain {
        wait: Duration::from_secs(1),
        settle: Duration::from_secs(1),
    }
}

fn note(step: &Step) -> Option<Note> {
    match step {
        Step::Wait { note, .. } => note.clone(),
        other => panic!("{other:?}"),
    }
}

/// Heartbeats are sent every 60 s in these tests, so a beat 60 s after the last is not a gap.
fn replies(beats: usize) -> Vec<crate::support::Reply> {
    let mut all = vec![welcome_with(1, 60_000, 180_000)];
    all.extend((0..beats).map(|_| beat_ok()));
    all
}

fn load_of(sent: &crate::support::Sent, n: usize) -> Option<Value> {
    sent.borrow()[n].1.get("load").cloned()
}

/// Asserts the load of two heartbeats against numbers worked out by hand: the first is a
/// baseline (no minutes, nothing covered), the second, 60 s later, covers the lease and
/// closes its minute 0 at 2000 thousandths of a core. A run without a control group is not
/// sampled. It catches a milliseconds-for-microseconds mix-up (the clock reads 1000 ms and
/// the body must say 1000000 us), a working set that forgets the inactive file, swapped
/// active and inactive file values, CPU time reported as a rate, a lease clock that is not
/// the run's start, and a missing lease being invented as zero.
#[test]
fn the_heartbeat_carries_the_load_of_the_actions_cgroup_in_the_wire_units() {
    let rig = rig(vec![
        running("3-7", Some(LEASE_DIR), Some(500_000)),
        running("3-8", None, Some(500_000)),
    ]);
    let live = Live(RefCell::new(files(5_000_000, 400_000, 3_000_000_001, true)));
    let clock: SharedClock = rig.machine.clock.clone();
    let (mut transport, sent) = Script::new(replies(2));
    let mut session = Session::new(hello("h1"), &mut transport, lowest())
        .with_jobs(&rig.worker, drain())
        .with_load(LoadReader::new(&live, ACTIONS, clock));
    session.tick();
    rig.machine.clock.advance(500);
    session.tick();
    assert_eq!(
        load_of(&sent, 1),
        Some(json!({
            "seq": 1,
            "mono_usec": 1_000_000,
            "cpu_usage_usec": 5_000_000,
            "working_set_bytes": 7_000_000_000u64,
            "active_file_bytes": 250,
            "actions_cpu_limited": false,
            "clocks": [{ "lease_id": "3-7", "started_mono_usec": 500_000 }],
        }))
    );
    live.set(files(125_000_000, 120_400_000, 4_000_000_001, true));
    rig.machine.clock.advance(60_000);
    session.tick();
    assert_eq!(
        load_of(&sent, 2),
        Some(json!({
            "seq": 2,
            "mono_usec": 61_000_000,
            "cpu_usage_usec": 125_000_000u64,
            "working_set_bytes": 7_000_000_000u64,
            "active_file_bytes": 250,
            "actions_cpu_limited": false,
            "covered": ["3-7"],
            "clocks": [{ "lease_id": "3-7", "started_mono_usec": 500_000 }],
            "minutes": [{
                "lease_id": "3-7",
                "minute": 0,
                "cpu_mean_millis": 2000,
                "cpu_peak_window_millis": 2000,
                "ws_max_bytes": 3_000_000_000u64,
            }],
        }))
    );
}

/// Asserts that a session with no reader (an agent without `--actions-cgroup`) sends no
/// `load` key at all, whatever runs the worker holds. It catches a zero load sent for a
/// host that measures nothing.
#[test]
fn without_a_reader_the_heartbeat_has_no_load() {
    let rig = rig(vec![running("3-7", Some(LEASE_DIR), Some(500_000))]);
    let (mut transport, sent) = Script::new(replies(2));
    let mut session =
        Session::new(hello("h1"), &mut transport, lowest()).with_jobs(&rig.worker, drain());
    session.tick();
    session.tick();
    session.tick();
    for n in 1..3 {
        assert!(sent.borrow()[n].1.get("load").is_none(), "heartbeat {n}");
    }
}

/// Asserts that a reading that cannot be taken sends no load (not zeros), says why once
/// for the run of failures, and says it again when a later failure follows a success. It
/// catches a made-up zero load, a log line on every heartbeat, and a reason that is never
/// repeated after recovery.
#[test]
fn a_failing_sample_sends_no_load_and_says_why_once() {
    let rig = rig(vec![running("3-7", Some(LEASE_DIR), Some(500_000))]);
    let good = files(5_000_000, 400_000, 3_000_000_001, true);
    let broken = files(5_000_000, 400_000, 3_000_000_001, false);
    let live = Live(RefCell::new(broken.clone()));
    let clock: SharedClock = rig.machine.clock.clone();
    let (mut transport, sent) = Script::new(replies(4));
    let mut session = Session::new(hello("h1"), &mut transport, lowest())
        .with_jobs(&rig.worker, drain())
        .with_load(LoadReader::new(&live, ACTIONS, clock));
    session.tick();
    let first = session.tick();
    let second = session.tick();
    live.set(good);
    let third = session.tick();
    live.set(broken);
    let fourth = session.tick();
    let said = Some(Note::NoLoad {
        reason: SampleError::NodeMemoryCurrent,
    });
    assert_eq!(note(&first), said);
    assert_eq!(note(&second), None);
    assert_eq!(note(&third), None);
    assert_eq!(note(&fourth), said);
    let loads: Vec<bool> = (1..5).map(|n| load_of(&sent, n).is_some()).collect();
    assert_eq!(loads, [false, false, true, false]);
}

/// Asserts that the closed minutes of a heartbeat the server did not take (status 500) are
/// sent again on the next one. It catches minutes lost with a failed heartbeat, which would
/// leave the server's estimate short of data it was owed.
#[test]
fn minutes_of_an_undelivered_heartbeat_go_out_on_the_next() {
    let rig = rig(vec![running("3-7", Some(LEASE_DIR), Some(500_000))]);
    let live = Live(RefCell::new(files(5_000_000, 400_000, 3_000_000_001, true)));
    let clock: SharedClock = rig.machine.clock.clone();
    let mut script = replies(1);
    script.push(reply(500, json!({ "error": "busy" })));
    script.push(beat_ok());
    let (mut transport, sent) = Script::new(script);
    let mut session = Session::new(hello("h1"), &mut transport, lowest())
        .with_jobs(&rig.worker, drain())
        .with_load(LoadReader::new(&live, ACTIONS, clock));
    session.tick();
    rig.machine.clock.advance(500);
    session.tick();
    live.set(files(125_000_000, 120_400_000, 4_000_000_001, true));
    rig.machine.clock.advance(60_000);
    session.tick();
    let lost = load_of(&sent, 2).unwrap();
    assert_eq!(lost["minutes"].as_array().map(Vec::len), Some(1));
    rig.machine.clock.advance(5_000);
    session.tick();
    let again = load_of(&sent, 3).unwrap();
    assert_eq!(again["minutes"], lost["minutes"]);
    assert_eq!(again["seq"], 3);
}
