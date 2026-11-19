//! The operator's clear of the taint, over a state root and a ledger in a test directory, a
//! unit manager that writes what it is asked, and a clock that only counts its pauses.

use std::cell::{Cell, RefCell};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use js_agent_wire::Taint;
use js_testkit::dir::TestDir;
use js_update_hook::{PortError, UnitState, is_reason_token};
use serde_json::{Value, json};

use super::*;

/// The time of every clear in these tests: 1700000000 s and a half.
const NOW_MS: u64 = 1_700_000_000_500;

/// What the kept files and the record are named after.
const TAIL: &str = "cleared-1700000000-by-ops.lead";

/// A state root (`state`, 0700), a scratch root (`scratch/runs`) and its ledger.
struct Rig {
    dir: TestDir,
}

impl Rig {
    fn new(name: &str) -> Self {
        let dir = TestDir::under(std::env::temp_dir(), name);
        let state = dir.path().join("state");
        fs::create_dir_all(&state).expect("make the state root");
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).expect("0700");
        fs::create_dir_all(dir.path().join("scratch/runs")).expect("make the scratch root");
        fs::create_dir_all(dir.path().join("scratch/oci/oci-labels")).expect("make the ledger");
        Self { dir }
    }

    fn state(&self) -> PathBuf {
        self.dir.path().join("state")
    }

    fn ledger(&self) -> PathBuf {
        self.dir.path().join("scratch/oci/oci-labels")
    }

    /// The marker holds `taint`.
    fn taint(&self, taint: &Taint) {
        let root = StateRoot::open(&self.state()).expect("open the state root");
        root.write(taint).expect("write the marker");
    }

    /// The pin file holds `taint`.
    fn pin(&self, taint: &Taint) {
        let root = StateRoot::open(&self.state()).expect("open the state root");
        root.write_pin(taint).expect("write the pin file");
    }

    /// The checks of `run` are owed.
    fn owe(&self, run: &str) {
        let root = StateRoot::open(&self.state()).expect("open the state root");
        root.owe(run).expect("write the owed record");
    }

    /// The ledger holds a sidecar for the lease `name`.
    fn lease(&self, name: &str) {
        fs::write(self.ledger().join(format!("{name}.json")), "{}").expect("write a sidecar");
    }

    /// The names in the state root, sorted.
    fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(self.state())
            .expect("list the state root")
            .map(|entry| {
                entry
                    .expect("an entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    }

    fn args(&self, extra: &[&str]) -> Vec<OsString> {
        let state = self.state().display().to_string();
        let scratch = self.dir.path().join("scratch/runs").display().to_string();
        let mut args: Vec<OsString> = [
            "js-agent",
            VERB,
            "--reason",
            "the leftover process was a host daemon",
            "--by",
            "ops.lead",
        ]
        .into_iter()
        .map(OsString::from)
        .collect();
        args.extend(["--state-root", &state, "--scratch-root", &scratch].map(OsString::from));
        args.extend(extra.iter().map(OsString::from));
        args
    }
}

fn residue() -> Taint {
    Taint {
        reason: "residue".to_owned(),
        run_id: Some("r1".to_owned()),
        check: Some("process-as-job-user".to_owned()),
    }
}

/// A unit manager and a clock: each call is written to the log; the unit goes down after
/// `stop_polls` reads that find it deactivating (never, when `None`), and is active after a
/// start once `start_polls` reads found it not yet.
struct Fake {
    log: RefCell<Vec<String>>,
    stop_polls: Cell<Option<u32>>,
    start_polls: Cell<Option<u32>>,
    fail_stop: bool,
    fail_start: bool,
    /// Done when the stop is asked for: what happens on the host while the unit stops.
    on_stop: RefCell<Option<Box<dyn FnOnce()>>>,
    pauses: Cell<u32>,
}

impl Default for Fake {
    fn default() -> Self {
        Self {
            log: RefCell::default(),
            stop_polls: Cell::new(Some(1)),
            start_polls: Cell::new(Some(0)),
            fail_stop: false,
            fail_start: false,
            on_stop: RefCell::default(),
            pauses: Cell::new(0),
        }
    }
}

impl Fake {
    fn note(&self, call: &str) {
        self.log.borrow_mut().push(call.to_owned());
    }

    fn log(&self) -> Vec<String> {
        self.log.borrow().clone()
    }

    fn host(&self) -> Host<'_> {
        Host {
            units: self,
            clock: self,
        }
    }
}

impl Units for Fake {
    fn show(&self, unit: &str) -> Result<UnitState, PortError> {
        self.note(&format!("show {unit}"));
        let active = match self.stop_polls.get() {
            Some(0) => "inactive",
            Some(left) => {
                self.stop_polls.set(Some(left - 1));
                "deactivating"
            }
            None => "deactivating",
        };
        Ok(UnitState {
            active: active.to_owned(),
            sub: "dead".to_owned(),
        })
    }

    fn stop_no_block(&self, unit: &str) -> Result<(), PortError> {
        self.note(&format!("stop {unit}"));
        if let Some(on_stop) = self.on_stop.borrow_mut().take() {
            on_stop();
        }
        match self.fail_stop {
            true => Err(PortError::Command("systemctl: stop refused".to_owned())),
            false => Ok(()),
        }
    }

    fn start(&self, unit: &str) -> Result<(), PortError> {
        self.note(&format!("start {unit}"));
        match self.fail_start {
            true => Err(PortError::Command("systemctl: start refused".to_owned())),
            false => Ok(()),
        }
    }

    fn is_active(&self, unit: &str) -> Result<bool, PortError> {
        self.note(&format!("is-active {unit}"));
        match self.start_polls.get() {
            Some(0) => Ok(true),
            Some(left) => {
                self.start_polls.set(Some(left - 1));
                Ok(false)
            }
            None => Ok(false),
        }
    }
}

impl Clock for Fake {
    fn now_ms(&self) -> u64 {
        NOW_MS
    }

    fn pause(&self, _span: Duration) {
        self.pauses.set(self.pauses.get() + 1);
    }
}

/// Runs the verb on `args` over `fake`: the exit status, the last line of standard output as
/// JSON, and standard error.
fn run_on(fake: &Fake, args: &[OsString]) -> (ExitCode, Value, String) {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let status = run(args, &mut out, &mut err, &fake.host());
    let out = String::from_utf8(out).expect("text");
    let last = out.lines().last().unwrap_or_default();
    let line = serde_json::from_str(last).unwrap_or(Value::Null);
    (status, line, String::from_utf8(err).expect("text"))
}

const UNIT: &str = "js-agent-crun.service";

fn stopped_and_started() -> Vec<String> {
    vec![
        format!("stop {UNIT}"),
        format!("show {UNIT}"),
        format!("show {UNIT}"),
        format!("start {UNIT}"),
        format!("is-active {UNIT}"),
    ]
}

/// Proves the whole clear: with a marker, a pin file and two owed records and no lease, the
/// unit is stopped and waited for, each owed record, the pin file and the marker are kept
/// under their `.cleared-<time>-by-<operator>` names (nothing is removed, and a kept record is
/// not read as owed), the record names when, who, why, the taint and each file, the journal
/// line says the same, the unit is started, and the last line is a success. Afterwards the
/// state root reads as untainted with nothing owed. Catches a clear that removes the files,
/// leaves a record the next start reads as owed, skips the pin file, forgets the record or
/// the restart, or reports before it is done.
#[test]
fn a_clear_keeps_every_file_aside_records_it_and_restarts_the_agent() {
    let rig = Rig::new("clear-taint-whole");
    rig.taint(&residue());
    rig.pin(&residue());
    rig.owe("r1");
    rig.owe("r2.x");
    let fake = Fake::default();
    let (status, line, err) = run_on(&fake, &rig.args(&[]));

    assert_eq!(status, ExitCode::SUCCESS);
    assert_eq!(fake.log(), stopped_and_started());
    assert_eq!(
        fake.pauses.get(),
        1,
        "one wait while the unit was deactivating"
    );
    let mut names = vec![
        LOCK_FILE.to_owned(),
        format!("owed.r1.{TAIL}"),
        format!("owed.r2.x.{TAIL}"),
        format!("taint.cleared-1700000000-by-ops.lead.json"),
        format!("taint.json.{TAIL}"),
        format!("taint.pinned.{TAIL}"),
    ];
    names.sort();
    assert_eq!(rig.names(), names);
    let root = StateRoot::open(&rig.state()).expect("open");
    assert_eq!(root.read(), None);
    assert_eq!(root.owed().expect("owed"), Vec::<String>::new());
    assert_eq!(
        fs::read_to_string(rig.state().join(format!("taint.json.{TAIL}"))).expect("kept marker"),
        serde_json::to_string(&residue()).expect("json")
    );

    let record: Value = serde_json::from_str(
        &fs::read_to_string(
            rig.state()
                .join("taint.cleared-1700000000-by-ops.lead.json"),
        )
        .expect("the record"),
    )
    .expect("json");
    assert_eq!(
        record,
        json!({
            "cleared_at_unix_secs": 1_700_000_000_u64,
            "by": "ops.lead",
            "reason": "the leftover process was a host daemon",
            "taint": {"reason": "residue", "run_id": "r1", "check": "process-as-job-user"},
            "kept": [
                {"file": "owed-r1", "kept_as": format!("owed.r1.{TAIL}")},
                {"file": "owed-r2.x", "kept_as": format!("owed.r2.x.{TAIL}")},
                {"file": "taint.pinned", "kept_as": format!("taint.pinned.{TAIL}")},
                {"file": "taint.json", "kept_as": format!("taint.json.{TAIL}")},
            ],
        })
    );
    let told = format!(
        "cleared the taint (reason residue, check process-as-job-user, run r1) at 1700000000 \
         by ops.lead: kept owed-r1 as owed.r1.{TAIL}, owed-r2.x as owed.r2.x.{TAIL}, \
         taint.pinned as taint.pinned.{TAIL}, taint.json as taint.json.{TAIL}; reason: the \
         leftover process was a host daemon; the lease server keeps its own record of the \
         taint until the operator's ClearHostTaint"
    );
    assert_eq!(err, format!("js-agent clear-taint: {told}\n"));
    assert_eq!(
        line,
        json!({
            "class": "release", "reversible": true, "promotion_pending": false,
            "detail": format!("{told}; recorded in taint.cleared-1700000000-by-ops.lead.json"),
        })
    );
}

/// Proves that a host with no marker, pin file or owed record is left alone: success, the
/// unit is not touched, nothing but the lock is written. Catches a clear that restarts a
/// healthy agent.
#[test]
fn an_untainted_host_is_left_alone() {
    let rig = Rig::new("clear-taint-clean");
    let fake = Fake::default();
    let (status, line, err) = run_on(&fake, &rig.args(&[]));

    assert_eq!(status, ExitCode::SUCCESS);
    assert!(fake.log().is_empty(), "{:?}", fake.log());
    assert_eq!(rig.names(), [LOCK_FILE]);
    assert_eq!(err, "");
    assert_eq!(
        line["detail"],
        "the host is not tainted: there is no marker, pin file or owed record"
    );
}

/// Proves that a lease in the ledger defers the clear: exit 75, `run_held` naming the
/// leases, a retry after, the unit untouched and every file where it was. Catches a clear
/// that stops an agent while it runs a job.
#[test]
fn a_run_that_lives_defers_the_clear_and_changes_nothing() {
    let rig = Rig::new("clear-taint-held");
    rig.taint(&residue());
    rig.lease("js-h1-r9");
    rig.lease("js-h1-r8");
    let fake = Fake::default();
    let (status, line, err) = run_on(&fake, &rig.args(&[]));

    assert_eq!(status, ExitCode::from(75));
    assert!(fake.log().is_empty(), "{:?}", fake.log());
    assert_eq!(rig.names(), [LOCK_FILE, MARKER_FILE]);
    let detail = "the agent holds a run that lives (lease js-h1-r8, js-h1-r9); nothing was changed";
    assert_eq!(
        line,
        json!({
            "class": "release", "reversible": true, "promotion_pending": false,
            "reason": "run_held", "retry_after_secs": 300, "detail": detail,
        })
    );
    assert_eq!(err, format!("js-agent clear-taint: run_held: {detail}\n"));
}

/// Proves that a lease made while the unit stopped (an agent that was not tainted in memory)
/// starts the unit again and defers the clear with every file where it was. Catches a clear
/// that reads the ledger only before the stop.
#[test]
fn a_lease_made_during_the_stop_starts_the_agent_again_and_defers() {
    let rig = Rig::new("clear-taint-race");
    rig.owe("r1");
    let ledger = rig.ledger();
    let fake = Fake::default();
    *fake.on_stop.borrow_mut() = Some(Box::new(move || {
        fs::write(ledger.join("js-h1-r3.json"), "{}").expect("a sidecar");
    }));
    let (status, line, _) = run_on(&fake, &rig.args(&[]));

    assert_eq!(status, ExitCode::from(75));
    assert_eq!(line["reason"], "run_held");
    assert_eq!(fake.log(), stopped_and_started());
    assert_eq!(rig.names(), [LOCK_FILE, "owed-r1"]);
}

/// Proves that a unit that does not go down within the wait, or whose stop is refused, fails
/// the clear with nothing moved and is started again; the wait is the stop bound in polls.
/// Catches a clear that moves the marker under a running agent, and one that leaves the unit
/// stopped.
#[test]
fn a_unit_that_does_not_stop_fails_the_clear_with_nothing_moved() {
    let rig = Rig::new("clear-taint-stuck");
    rig.taint(&residue());
    let fake = Fake {
        stop_polls: Cell::new(None),
        ..Fake::default()
    };
    let (status, line, _) = run_on(&fake, &rig.args(&["--unit", "agent.service"]));

    assert_eq!(status, ExitCode::from(1));
    assert_eq!(
        line["detail"],
        "agent.service did not stop within 300s; nothing was moved and the unit is started again"
    );
    assert_eq!(line["reason"], "stop_failed");
    let polls = STOP_WAIT.as_secs().div_ceil(POLL_SECS) + 1;
    assert_eq!(u64::from(fake.pauses.get()), polls);
    let log = fake.log();
    assert_eq!(log.first().map(String::as_str), Some("stop agent.service"));
    assert_eq!(log.last().map(String::as_str), Some("start agent.service"));
    assert_eq!(rig.names(), [LOCK_FILE, MARKER_FILE]);

    let refused = Fake {
        fail_stop: true,
        ..Fake::default()
    };
    let (status, line, _) = run_on(&refused, &rig.args(&[]));
    assert_eq!(status, ExitCode::from(1));
    assert_eq!(line["reason"], "stop_failed");
    assert_eq!(
        line["detail"],
        format!(
            "the stop of {UNIT} failed: systemctl: stop refused; nothing was moved and the unit \
             is started again"
        )
    );
    assert_eq!(
        refused.log(),
        [format!("stop {UNIT}"), format!("start {UNIT}")]
    );
    assert_eq!(rig.names(), [LOCK_FILE, MARKER_FILE]);
}

/// Proves that a kept name that is taken stops the clear without writing over it: the owed
/// record before it is kept, the marker stays (the host stays tainted), the detail says what
/// was kept, and the unit is left stopped; a first move that fails says nothing was kept.
/// Catches a move that writes over an earlier kept file, one that goes on after a failed move,
/// and an agent started with its owed records or pin file gone and its marker left, which
/// could then clear a taint that must stay.
#[test]
fn a_taken_name_stops_the_clear_and_keeps_the_marker() {
    let rig = Rig::new("clear-taint-taken");
    rig.taint(&residue());
    rig.owe("r1");
    let taken = rig.state().join(format!("taint.json.{TAIL}"));
    fs::write(&taken, "earlier").expect("an earlier kept marker");
    let fake = Fake::default();
    let (status, line, _) = run_on(&fake, &rig.args(&[]));

    assert_eq!(status, ExitCode::from(1));
    assert_eq!(line["reason"], "clear_failed");
    let detail = line["detail"].as_str().unwrap_or_default();
    assert!(
        detail.starts_with(&format!(
            "taint.json could not be kept as taint.json.{TAIL}: "
        )),
        "{detail}"
    );
    assert!(
        detail.ends_with(&format!(
            "; kept owed-r1 as owed.r1.{TAIL}; {UNIT} is left stopped: run the clear again once \
             the cause is fixed"
        )),
        "{detail}"
    );
    assert_eq!(fs::read_to_string(&taken).expect("kept"), "earlier");
    assert!(rig.state().join(MARKER_FILE).exists());
    assert_eq!(fake.log()[..], stopped_and_started()[..3]);

    let rig = Rig::new("clear-taint-taken-first");
    rig.taint(&residue());
    fs::write(rig.state().join(format!("taint.json.{TAIL}")), "earlier").expect("taken");
    let fake = Fake::default();
    let (status, line, _) = run_on(&fake, &rig.args(&[]));
    assert_eq!(status, ExitCode::from(1));
    let detail = line["detail"].as_str().unwrap_or_default();
    assert!(detail.contains("; nothing was kept aside; "), "{detail}");
}

/// Proves that a record that cannot be written fails the clear after the files were kept,
/// says so, and still starts the unit. Catches a clear that is not recorded and says it
/// succeeded.
#[test]
fn a_record_that_cannot_be_written_fails_the_clear_but_starts_the_agent() {
    let rig = Rig::new("clear-taint-record");
    rig.taint(&residue());
    fs::write(
        rig.state()
            .join("taint.cleared-1700000000-by-ops.lead.json.tmp"),
        "",
    )
    .expect("a temporary file in the way");
    let fake = Fake::default();
    let (status, line, err) = run_on(&fake, &rig.args(&[]));

    assert_eq!(status, ExitCode::from(1));
    assert_eq!(line["reason"], "record_failed");
    let detail = line["detail"].as_str().unwrap_or_default();
    assert!(
        detail.starts_with(
            "cleared the taint (reason residue, check process-as-job-user, run r1) at 1700000000 \
             by ops.lead: kept taint.json as"
        ),
        "{detail}"
    );
    assert!(
        detail.contains("; the record: taint.cleared-1700000000-by-ops.lead.json: "),
        "{detail}"
    );
    assert!(
        err.contains("js-agent clear-taint: record_failed: "),
        "{err}"
    );
    assert_eq!(fake.log(), stopped_and_started());
    assert!(!rig.state().join(MARKER_FILE).exists());
}

/// Proves that a unit that will not start after a clear fails it, whether the start is
/// refused or the unit is not active within the wait, and that the clear itself stands.
/// Catches a success reported for a host left without its agent.
#[test]
fn a_unit_that_does_not_start_fails_a_clear_that_stands() {
    let rig = Rig::new("clear-taint-nostart");
    rig.taint(&residue());
    let fake = Fake {
        fail_start: true,
        ..Fake::default()
    };
    let (status, line, _) = run_on(&fake, &rig.args(&[]));
    assert_eq!(status, ExitCode::from(1));
    assert_eq!(line["reason"], "start_failed");
    let detail = line["detail"].as_str().unwrap_or_default();
    assert!(
        detail.ends_with(&format!("; {UNIT} did not start: systemctl: start refused")),
        "{detail}"
    );
    assert!(!rig.state().join(MARKER_FILE).exists());

    let rig = Rig::new("clear-taint-slow");
    rig.taint(&residue());
    let fake = Fake {
        start_polls: Cell::new(None),
        ..Fake::default()
    };
    let (status, line, _) = run_on(&fake, &rig.args(&[]));
    assert_eq!(status, ExitCode::from(1));
    let detail = line["detail"].as_str().unwrap_or_default();
    assert!(
        detail.ends_with(&format!(
            "; {UNIT} did not start: not active within {START_WAIT_SECS}s"
        )),
        "{detail}"
    );
    let polls = START_WAIT_SECS.div_ceil(POLL_SECS) + 1;
    // One pause while the unit stopped, and one after each read that found it not active.
    assert_eq!(u64::from(fake.pauses.get()), 1 + polls);
}

/// Proves that a clear another clear holds the lock of is refused with nothing done, and
/// that a lock that cannot be opened is a state root that cannot be used. Catches two clears
/// that move the same files at once.
#[test]
fn a_held_lock_refuses_a_second_clear() {
    let rig = Rig::new("clear-taint-locked");
    rig.taint(&residue());
    let held = File::create(rig.state().join(LOCK_FILE)).expect("the lock file");
    held.try_lock().expect("lock it");
    let fake = Fake::default();
    let (status, line, _) = run_on(&fake, &rig.args(&[]));
    assert_eq!(status, ExitCode::from(1));
    assert_eq!(line["reason"], "locked");
    assert_eq!(line["detail"], format!("another clear holds {LOCK_FILE}"));
    assert!(fake.log().is_empty());
    drop(held);

    let rig = Rig::new("clear-taint-lockdir");
    fs::create_dir(rig.state().join(LOCK_FILE)).expect("a directory in the way");
    let (status, line, _) = run_on(&fake, &rig.args(&[]));
    assert_eq!(status, ExitCode::from(1));
    assert_eq!(line["reason"], "state_invalid");
    let detail = line["detail"].as_str().unwrap_or_default();
    assert!(detail.starts_with(&format!("{LOCK_FILE}: ")), "{detail}");
}

/// Proves the refusals of what the clear reads: a state root others may enter, more owed
/// records than the agent reads, and a ledger that is not a directory each fail it as
/// `state_invalid` with nothing moved and the unit untouched. Catches a clear that goes on
/// without knowing what is owed or whether a run lives.
#[test]
fn a_state_root_or_a_ledger_that_cannot_be_read_fails_the_clear() {
    let rig = Rig::new("clear-taint-open");
    fs::set_permissions(rig.state(), fs::Permissions::from_mode(0o750)).expect("0750");
    let fake = Fake::default();
    let (status, line, _) = run_on(&fake, &rig.args(&[]));
    assert_eq!(status, ExitCode::from(1));
    assert_eq!(line["reason"], "state_invalid");
    let detail = line["detail"].as_str().unwrap_or_default();
    assert!(detail.starts_with("--state-root: "), "{detail}");

    let rig = Rig::new("clear-taint-owed-many");
    for run in 0..=crate::lifecycle::taint::OWED_MAX {
        fs::write(rig.state().join(format!("owed-r{run}")), "").expect("an owed record");
    }
    let (status, line, _) = run_on(&fake, &rig.args(&[]));
    assert_eq!(status, ExitCode::from(1));
    assert_eq!(line["reason"], "state_invalid");
    assert_eq!(
        line["detail"],
        "the owed records: more than 1024 records of owed checks"
    );

    let rig = Rig::new("clear-taint-ledger");
    rig.taint(&residue());
    fs::remove_dir(rig.ledger()).expect("no ledger");
    fs::write(rig.ledger(), "not a directory").expect("a file in the way");
    let (status, line, _) = run_on(&fake, &rig.args(&[]));
    assert_eq!(status, ExitCode::from(1));
    assert_eq!(line["reason"], "state_invalid");
    let detail = line["detail"].as_str().unwrap_or_default();
    assert!(
        detail.starts_with(&format!("the ledger {}: ", rig.ledger().display())),
        "{detail}"
    );
    assert!(fake.log().is_empty());
    assert!(rig.state().join(MARKER_FILE).exists());
}

/// Proves that a marker that cannot be read is cleared like any other, and the record says
/// the taint it was read as. Catches a clear that refuses the one taint only an operator can
/// end.
#[test]
fn an_unreadable_marker_is_cleared_and_recorded_as_read() {
    let rig = Rig::new("clear-taint-garbage");
    fs::write(rig.state().join(MARKER_FILE), "not json").expect("a broken marker");
    let fake = Fake::default();
    let (status, line, _) = run_on(&fake, &rig.args(&[]));
    assert_eq!(status, ExitCode::SUCCESS);
    let detail = line["detail"].as_str().unwrap_or_default();
    assert!(
        detail.starts_with("cleared the taint (reason marker-unreadable, check none, run none)"),
        "{detail}"
    );
    let record: Value = serde_json::from_str(
        &fs::read_to_string(
            rig.state()
                .join("taint.cleared-1700000000-by-ops.lead.json"),
        )
        .expect("the record"),
    )
    .expect("json");
    assert_eq!(record["taint"]["reason"], "marker-unreadable");
}

/// Proves the refusals of the flags, each with nothing done and the unit untouched: an
/// operator name that is empty, too long, starts with `.` or `-` or holds another byte; a
/// reason that is empty, too long or holds a control character; a missing or unknown flag.
/// The longest operator and reason are taken. `--help` prints the help and succeeds.
/// Catches a name that escapes the state root or breaks the kept names, and a record that
/// carries a line break.
#[test]
fn the_flags_are_checked_before_anything_is_done() {
    let rig = Rig::new("clear-taint-flags");
    rig.taint(&residue());
    let fake = Fake::default();
    let long_by = "a".repeat(65);
    let long_reason = "r".repeat(MAX_REASON + 1);
    let by_rule = "--by: 1 to 64 of A-Z a-z 0-9 . _ @ -, not starting with . or -";
    let reason_rule = "--reason: 1 to 512 bytes with no control character";
    let cases: [(&str, &str, &str); 8] = [
        ("", "fine", by_rule),
        (&long_by, "fine", by_rule),
        (".hidden", "fine", by_rule),
        ("-dash", "fine", by_rule),
        ("a/b", "fine", by_rule),
        ("ops", "", reason_rule),
        ("ops", &long_reason, reason_rule),
        ("ops", "two\nlines", reason_rule),
    ];
    for (by, reason, rule) in cases {
        let state = rig.state().display().to_string();
        let (by_flag, reason_flag) = (format!("--by={by}"), format!("--reason={reason}"));
        let args: Vec<OsString> = [
            "js-agent",
            VERB,
            &by_flag,
            &reason_flag,
            "--state-root",
            &state,
        ]
        .map(OsString::from)
        .to_vec();
        let (status, line, err) = run_on(&fake, &args);
        assert_eq!(status, ExitCode::from(1), "{by:?} {reason:?}");
        assert_eq!(line["reason"], "bad_args");
        assert_eq!(line["detail"], rule);
        assert_eq!(err, format!("js-agent clear-taint: bad_args: {rule}\n"));
    }
    assert!(fake.log().is_empty());
    assert!(rig.state().join(MARKER_FILE).exists());

    let missing: Vec<OsString> = ["js-agent", VERB, "--by", "ops"]
        .map(OsString::from)
        .to_vec();
    let (status, line, _) = run_on(&fake, &missing);
    assert_eq!(status, ExitCode::from(1));
    assert_eq!(line["reason"], "bad_args");
    assert!(
        line["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("--reason")
    );
    let (status, line, _) = run_on(&fake, &rig.args(&["--force"]));
    assert_eq!(status, ExitCode::from(1));
    assert!(
        line["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("--force")
    );

    let longest = Rig::new("clear-taint-longest");
    longest.owe("r1");
    let by = "b".repeat(64);
    let reason = "r".repeat(MAX_REASON);
    let state = longest.state().display().to_string();
    let scratch = longest
        .dir
        .path()
        .join("scratch/runs")
        .display()
        .to_string();
    let args: Vec<OsString> = [
        "js-agent",
        VERB,
        "--by",
        &by,
        "--reason",
        &reason,
        "--state-root",
        &state,
        "--scratch-root",
        &scratch,
    ]
    .map(OsString::from)
    .to_vec();
    let (status, _, _) = run_on(&Fake::default(), &args);
    assert_eq!(status, ExitCode::SUCCESS);

    let (mut out, mut err) = (Vec::new(), Vec::new());
    let help: Vec<OsString> = ["js-agent", VERB, "--help"].map(OsString::from).to_vec();
    let status = run(&help, &mut out, &mut err, &fake.host());
    assert_eq!(status, ExitCode::SUCCESS);
    let out = String::from_utf8(out).expect("text");
    assert!(out.contains("Usage: js-agent clear-taint"), "{out}");
    assert!(err.is_empty());
}

/// Proves every reason token is one the updater takes. Catches a token it would read as
/// none.
#[test]
fn every_token_is_a_reason_the_updater_takes() {
    for token in TOKENS {
        assert!(is_reason_token(token.as_str()), "{token:?}");
    }
}

/// Proves the machine's wiring on a host with nothing to clear: it answers without calling
/// the unit manager. Catches a verb whose machine side does not run the clear.
#[test]
fn the_machines_verb_leaves_an_untainted_host_alone() {
    let rig = Rig::new("clear-taint-system");
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let status = run_system(&rig.args(&[]), &mut out, &mut err);
    assert_eq!(status, ExitCode::SUCCESS);
    let out = String::from_utf8(out).expect("text");
    assert!(out.contains("the host is not tainted"), "{out}");
}

/// Proves that a host job's taint is refused with nothing done and the unit untouched: that
/// machine is reimaged. Catches a clear that puts a host a job could change at will back in
/// service.
#[test]
fn a_host_jobs_taint_is_refused() {
    let rig = Rig::new("clear-taint-host-job");
    rig.taint(&Taint {
        reason: "host-job".to_owned(),
        run_id: Some("r1".to_owned()),
        check: None,
    });
    let fake = Fake::default();
    let (status, line, _) = run_on(&fake, &rig.args(&[]));
    assert_eq!(status, ExitCode::from(1));
    assert_eq!(line["reason"], "host_job");
    assert_eq!(
        line["detail"],
        "the taint is a host job's: the machine is reimaged, not cleared \
         (docs/runbooks/host-jobs.md); nothing was done"
    );
    assert!(fake.log().is_empty());
    assert_eq!(rig.names(), [LOCK_FILE, MARKER_FILE]);
}

/// Proves that a ledger that is not there (a `--scratch-root` that is not the agent's) fails
/// the clear with nothing moved and the unit untouched. Catches a mistyped scratch root read
/// as an agent with no run, which would stop an agent that runs jobs.
#[test]
fn a_ledger_that_is_not_there_fails_the_clear() {
    let rig = Rig::new("clear-taint-no-ledger");
    rig.taint(&residue());
    fs::remove_dir(rig.ledger()).expect("no ledger");
    let fake = Fake::default();
    let (status, line, _) = run_on(&fake, &rig.args(&[]));
    assert_eq!(status, ExitCode::from(1));
    assert_eq!(line["reason"], "state_invalid");
    let detail = line["detail"].as_str().unwrap_or_default();
    assert!(
        detail.starts_with(&format!("the ledger {}: ", rig.ledger().display())),
        "{detail}"
    );
    assert!(fake.log().is_empty());
    assert_eq!(rig.names(), [LOCK_FILE, MARKER_FILE]);
}

/// Proves that a pin file alone (no marker, no owed record: a stale one) is kept aside and
/// recorded with no taint, and the agent, which is not tainted, is not stopped; a kept name
/// that is taken fails it with the pin file where it was. Catches a clear that restarts a
/// healthy agent for a file it ignores.
#[test]
fn a_stale_pin_file_is_kept_aside_without_touching_the_agent() {
    let rig = Rig::new("clear-taint-pin-alone");
    rig.pin(&residue());
    let fake = Fake::default();
    let (status, line, err) = run_on(&fake, &rig.args(&[]));
    assert_eq!(status, ExitCode::SUCCESS);
    assert!(fake.log().is_empty(), "{:?}", fake.log());
    let mut names = vec![
        LOCK_FILE.to_owned(),
        "taint.cleared-1700000000-by-ops.lead.json".to_owned(),
        format!("taint.pinned.{TAIL}"),
    ];
    names.sort();
    assert_eq!(rig.names(), names);
    let detail = line["detail"].as_str().unwrap_or_default();
    assert!(
        detail.starts_with(&format!(
            "cleared the taint (no marker) at 1700000000 by ops.lead: kept taint.pinned as \
             taint.pinned.{TAIL}"
        )),
        "{detail}"
    );
    assert!(
        err.starts_with("js-agent clear-taint: cleared the taint (no marker)"),
        "{err}"
    );
    let record: Value = serde_json::from_str(
        &fs::read_to_string(
            rig.state()
                .join("taint.cleared-1700000000-by-ops.lead.json"),
        )
        .expect("the record"),
    )
    .expect("json");
    assert_eq!(record["taint"], Value::Null);

    let rig = Rig::new("clear-taint-pin-taken");
    rig.pin(&residue());
    fs::write(rig.state().join(format!("taint.pinned.{TAIL}")), "earlier").expect("taken");
    let (status, line, _) = run_on(&fake, &rig.args(&[]));
    assert_eq!(status, ExitCode::from(1));
    assert_eq!(line["reason"], "clear_failed");
    assert!(rig.state().join(PIN_FILE).exists());
    assert!(fake.log().is_empty());
}
