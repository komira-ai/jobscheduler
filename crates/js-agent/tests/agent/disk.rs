//! The free scratch disk after the hello: the session tells the server when it changed, and
//! the gauge it asks reads the volume again, no more often than its interval. The programs,
//! the clock and the bundle store are fakes the test moves; no disk is read.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use js_agent::bundle::Settings;
use js_agent::disk::{DiskGauge, REREAD_MS, Reserve, ScratchDisk};
use js_agent::{Cli, CommandRunner, HEARTBEAT_PATH, Session};

use crate::bundle::fake::FakeHost;
use crate::jobs::fake::{FakeMachine, TestClock};
use crate::support::{
    Lowest, Reply, Scratch, Script, ScriptConnector, Sent, beat_ok, code, flags, hello, mac_host,
    platform, refused, run_with_store, welcome,
};

const GIB: u64 = 1 << 30;

/// Readings in order; the last one repeats.
struct Readings(RefCell<VecDeque<Option<u64>>>);

impl Readings {
    fn of(values: &[Option<u64>]) -> Self {
        Self(RefCell::new(values.iter().copied().collect()))
    }
}

impl DiskGauge for Readings {
    fn gib(&self) -> Option<u64> {
        let mut left = self.0.borrow_mut();
        match left.len() {
            0 | 1 => left.front().copied().flatten(),
            _ => left.pop_front().flatten(),
        }
    }
}

/// The disk-gib of every heartbeat sent, `None` where the body has no such field.
fn told(sent: &Sent) -> Vec<Option<u64>> {
    sent.borrow()
        .iter()
        .filter(|(path, _)| path == HEARTBEAT_PATH)
        .map(|(_, body)| body.get("disk_gib").map(|n| n.as_u64().unwrap()))
        .collect()
}

/// A session that said hello with `hello_gib` free (none: the hello had no disk-gib), over
/// `replies`, asking `gauge` before each heartbeat; ticked `ticks` times.
fn run(hello_gib: Option<&str>, gauge: &dyn DiskGauge, replies: Vec<Reply>, ticks: usize) -> Sent {
    let (mut transport, sent) = Script::new(replies);
    let mut first = hello("h1");
    if let Some(gib) = hello_gib {
        first.capabilities.insert("disk-gib".into(), gib.into());
    }
    let mut session =
        Session::new(first, &mut transport, crate::support::lowest()).with_disk(gauge);
    for _ in 0..ticks {
        let _ = session.tick();
    }
    sent
}

/// Asserts that a heartbeat carries the free disk when it differs from what the server was
/// last told (the hello's first), and carries nothing when it is the same, so a quiet host
/// adds no bytes to its beats. It catches a session that sends the field every time (the
/// server cannot tell a change from a repeat) and one that never sends it.
#[test]
fn a_changed_disk_rides_the_next_heartbeat_and_an_unchanged_one_does_not() {
    let gauge = Readings::of(&[Some(40), Some(12), Some(12), Some(12)]);
    let replies = vec![welcome(1), beat_ok(), beat_ok(), beat_ok(), beat_ok()];
    let sent = run(Some("40"), &gauge, replies, 5);
    assert_eq!(told(&sent), vec![None, Some(12), None, None]);
}

/// Asserts that a value is counted as told only once a heartbeat that carried it was
/// answered: a beat that never reached the server, and one the server refused with a
/// status that is not a success, are followed by a beat that says it again. It catches a
/// session that marks the disk as told when it sends, so a lost beat loses the change.
#[test]
fn a_disk_the_server_did_not_take_is_sent_again() {
    let gauge = Readings::of(&[Some(12)]);
    let down: Reply = Err("down".to_owned());
    let replies = vec![welcome(1), down, refused(503, "busy"), beat_ok(), beat_ok()];
    let sent = run(Some("40"), &gauge, replies, 5);
    assert_eq!(told(&sent), vec![Some(12), Some(12), Some(12), None]);
}

/// Asserts that after the server loses the session and the agent says hello again, the
/// disk is told again: the new session knows only the hello's value. It catches a session
/// that keeps the old session's idea of what the server knows.
#[test]
fn a_new_session_is_told_the_disk_again() {
    let gauge = Readings::of(&[Some(12)]);
    let replies = vec![
        welcome(1),
        beat_ok(),
        refused(409, "stale-epoch"),
        welcome(2),
        beat_ok(),
    ];
    let sent = run(Some("40"), &gauge, replies, 5);
    assert_eq!(told(&sent), vec![Some(12), None, Some(12)]);
}

/// Asserts that a gauge with no reading adds no field, and that a hello with no disk-gib
/// (the volume could not be read at start) is followed by a heartbeat that says the disk
/// once the gauge can read it. It catches a missing reading sent as zero (the server would
/// read a host with no disk) and a baseline that is taken as zero when the hello had none.
#[test]
fn no_reading_sends_nothing_and_a_first_reading_after_a_blind_hello_is_sent() {
    let gauge = Readings::of(&[None, Some(0), Some(0)]);
    let replies = vec![welcome(1), beat_ok(), beat_ok(), beat_ok()];
    let sent = run(None, &gauge, replies, 4);
    assert_eq!(told(&sent), vec![None, Some(0), None]);
}

/// Asserts that a session given no gauge sends no `disk_gib`. It catches a default that
/// reports a disk the agent never read.
#[test]
fn a_session_with_no_gauge_says_nothing_of_disk() {
    let (mut transport, sent) = Script::new(vec![welcome(1), beat_ok()]);
    let mut session = Session::new(hello("h1"), &mut transport, crate::support::lowest());
    let _ = (session.tick(), session.tick());
    assert_eq!(told(&sent), vec![None]);
}

/// A `df` that reports a free space the test changes, and counts its runs.
struct Df {
    free_kib: Cell<Option<u64>>,
    runs: Cell<usize>,
}

impl Df {
    fn gib(gib: u64) -> Self {
        Self {
            free_kib: Cell::new(Some(gib * GIB / 1024 + 1023)),
            runs: Cell::new(0),
        }
    }
}

impl CommandRunner for Df {
    fn run(&self, program: &str, args: &[&str]) -> Result<String, String> {
        self.runs.set(self.runs.get() + 1);
        assert_eq!(
            (program, args),
            ("/bin/df", ["-P", "-k", "/scratch"].as_slice())
        );
        match self.free_kib.get() {
            Some(kib) => Ok(format!(
                "Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/x 9 9 {kib} 11% /\n"
            )),
            None => Err("df: failed".to_owned()),
        }
    }

    fn run_in(
        &self,
        program: &str,
        args: &[&str],
        _env: &[(&str, &str)],
    ) -> Result<String, String> {
        self.run(program, args)
    }
}

fn gauge<'a>(
    df: &'a Df,
    clock: &Arc<TestClock>,
    low: Option<u64>,
    store: Option<Reserve>,
) -> ScratchDisk<'a> {
    ScratchDisk::new(PathBuf::from("/scratch"), low, df, store, clock.clone())
}

/// Asserts that the gauge says the free space of the volume in whole GiB, rounded down,
/// less the low-water mark, and none below it. It catches a rounding up (a host promising
/// a byte it lacks) and a low-water mark that is left out here and kept in the hello.
#[test]
fn the_gauge_reads_whole_gib_less_the_low_water_mark() {
    let clock = Arc::new(TestClock::default());
    let df = Df::gib(50);
    assert_eq!(gauge(&df, &clock, None, None).gib(), Some(50));
    assert_eq!(gauge(&df, &clock, Some(10), None).gib(), Some(40));
    assert_eq!(gauge(&df, &clock, Some(60), None).gib(), Some(0));
}

/// Asserts that the part of the bundle store's budget the store has not used is left out of
/// what the gauge says, as in the hello, and follows the store as it fills. It catches a
/// gauge that offers disk the store will take.
#[test]
fn the_gauge_leaves_out_the_unused_budget_of_the_store() {
    let clock = Arc::new(TestClock::default());
    let df = Df::gib(100);
    let host = FakeHost::new();
    host.set(|s| s.used = Ok(5 * GIB));
    let cli = Cli::try_parse_from([
        "js-agent",
        "--server",
        "http://127.0.0.1:1",
        "--pool",
        "macs",
        "--scratch-root",
        "/scratch",
        "--bundle-store",
        "/Volumes/work/bundles",
        "--bundle-budget-gib",
        "20",
        "--bundle-trust",
        "digest",
    ])
    .unwrap();
    let settings = Settings::from_cli(&cli, &platform("macos", "aarch64"), &*host)
        .unwrap()
        .unwrap();
    let reserve = Reserve {
        settings,
        host: host.clone(),
    };
    let disk = gauge(&df, &clock, None, Some(reserve));
    assert_eq!(disk.gib(), Some(85));
    host.set(|s| s.used = Ok(20 * GIB));
    clock.advance(REREAD_MS);
    assert_eq!(disk.gib(), Some(100));
}

/// Asserts the interval: a second look inside it runs no program and repeats the last
/// reading, one at the interval runs `df` again and sees the new free space. It catches a
/// gauge that runs `df` on every heartbeat, and one that never looks again.
#[test]
fn the_gauge_reads_again_only_after_its_interval() {
    let clock = Arc::new(TestClock::default());
    let df = Df::gib(50);
    let disk = gauge(&df, &clock, None, None);
    assert_eq!(disk.gib(), Some(50));
    df.free_kib.set(Some(7 * GIB / 1024));
    clock.advance(REREAD_MS - 1);
    assert_eq!((disk.gib(), df.runs.get()), (Some(50), 1));
    clock.advance(1);
    assert_eq!((disk.gib(), df.runs.get()), (Some(7), 2));
}

/// Asserts that a volume that cannot be read says nothing (not zero), that the failure is
/// held for the interval like a reading, and that the next successful read is reported. It
/// catches a failure read as an empty disk, which would have the server stop placing work
/// on a host over a flaky `df`.
#[test]
fn a_volume_that_cannot_be_read_says_nothing_until_it_can() {
    let clock = Arc::new(TestClock::default());
    let df = Df::gib(50);
    df.free_kib.set(None);
    let disk = gauge(&df, &clock, None, None);
    assert_eq!((disk.gib(), disk.gib(), df.runs.get()), (None, None, 1));
    df.free_kib.set(Some(3 * GIB / 1024));
    clock.advance(REREAD_MS);
    assert_eq!(disk.gib(), Some(3));
}

/// A sleeper that, at its first wait, moves the clock on and fills the store, then stops
/// at its second.
struct Fills {
    clock: Arc<TestClock>,
    store: Arc<FakeHost>,
    waits: usize,
}

impl js_agent::Sleeper for Fills {
    fn sleep(&mut self, _duration: std::time::Duration) -> js_agent::Wake {
        self.waits += 1;
        match self.waits {
            1 => {
                self.store.set(|s| s.used = Ok(40 * GIB));
                self.clock.advance(REREAD_MS);
                js_agent::Wake::Elapsed
            }
            _ => js_agent::Wake::Stop,
        }
    }
}

/// Asserts the whole path through the program: a Mac agent with a store budget of 40 GiB
/// says 60 GiB in its hello, the store then uses its whole budget, and the first heartbeat
/// after the interval says 100, with no field in the hello changing. It catches a gauge
/// that is never built from the flags, one built over another volume or store than the
/// hello's, and one the session is never given.
#[test]
fn the_program_tells_the_server_the_disk_its_store_gave_back() {
    let dir = Scratch::new("disk-gauge");
    let scratch = dir.path("scratch");
    std::fs::create_dir(&scratch).unwrap();
    std::fs::set_permissions(&scratch, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut host = mac_host();
    let row = format!(
        "Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/disk3 9 9 {} 11% /\n",
        100 * GIB / 1024
    );
    host.commands = host
        .commands
        .with(&format!("/bin/df -P -k {}", scratch.display()), Ok(&row));
    let store = FakeHost::new();
    let machine = FakeMachine::new();
    let mut sleeper = Fills {
        clock: machine.clock.clone(),
        store: store.clone(),
        waits: 0,
    };
    let (script, sent) = Script::new(vec![welcome(1), beat_ok()]);
    let connector = ScriptConnector::of(script);
    let scratch_flag = scratch.display().to_string();
    let all = flags(&[
        "--scratch-root",
        scratch_flag.as_str(),
        "--bundle-store",
        "/Volumes/work/bundles",
        "--bundle-budget-gib",
        "40",
        "--bundle-trust",
        "digest",
    ]);
    let jobs: crate::support::Jobs<'_> = (Arc::new(machine), None);
    let ran = run_with_store(
        &all,
        &host,
        &connector,
        &mut sleeper,
        &mut Lowest,
        jobs,
        store,
    );
    assert_eq!(ran.status(), code(0), "{}", ran.err);
    let hello = sent.borrow()[0].1.clone();
    assert_eq!(hello["capabilities"]["disk-gib"], "60");
    // The drain heartbeat after the stop may say it once more.
    assert_eq!(told(&sent)[0], Some(100));
}
