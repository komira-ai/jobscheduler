//! The pin file (`taint.pinned`): a taint the agent pinned in memory stays pinned after a
//! restart, though the marker holds only the first taint. The runtime, the checks and the
//! clock are the in-memory ones; the marker and the pin file are real files in the state root.

use std::fs;

use js_agent::integrity::code as check;
use js_agent::lifecycle::taint::{PIN_FILE, PIN_TEMP_FILE, reason};
use js_agent::lifecycle::{CheckFinding, Event};
use js_agent_wire::Taint;
use serde_json::json;

use super::{Harness, Options, job, names, reply_json};

/// The wait between re-checks when `--taint-recheck-ms` is not given, in milliseconds.
const EVERY: u64 = 60_000;

fn residue() -> CheckFinding {
    CheckFinding::residue(check::DAEMONISED_CHILD, "pid 41")
}

/// The server acknowledges the report of the taint.
fn ack(h: &mut Harness) -> Vec<Event> {
    h.life.apply(&reply_json(&json!({ "taint_ack": true })))
}

/// The server acknowledges the report of the clear.
fn ack_clear(h: &mut Harness) -> Vec<Event> {
    h.life
        .apply(&reply_json(&json!({ "taint_cleared_ack": true })))
}

/// Moves the clock by `ms` and steps.
fn tick(h: &mut Harness, ms: u64) {
    h.clock.advance(ms);
    h.life.step();
}

/// How many re-checks have run.
fn rechecks(h: &Harness) -> usize {
    let log = h.log.borrow();
    log.iter()
        .filter(|line| line.starts_with("check taint-recheck"))
        .count()
}

/// The pin file, which may not exist.
fn pin_path(h: &Harness) -> std::path::PathBuf {
    h.marker().with_file_name(PIN_FILE)
}

/// An agent whose `r1` left a residue of a process (the marker's taint) and whose `r2` left a
/// new mount (a taint no re-check clears, sealed beside it), both over: the host is tainted,
/// pinned in memory, and the report acknowledged.
fn pinned_in_process(name: &str) -> Harness {
    let mut h = Harness::with(
        name,
        Options {
            findings: vec![
                ("r1", residue()),
                ("r2", CheckFinding::residue(check::NEW_MOUNT, "/mnt/x")),
            ],
            ..Options::default()
        },
    );
    h.start(job("r1"));
    h.start(job("r2"));
    h.fake.exit("r1", 0);
    h.life.step();
    ack(&mut h);
    h.fake.exit("r2", 0);
    h.life.step();
    h.life.step();
    h
}

/// Leaves the marker of a residue of a process, as an agent that stopped left it.
fn leave_residue(h: &Harness) -> Taint {
    let taint = Taint {
        reason: reason::RESIDUE.to_owned(),
        run_id: Some("r1".to_owned()),
        check: Some(check::DAEMONISED_CHILD.to_owned()),
    };
    h.config.state.write(&taint).unwrap();
    taint
}

/// Steps through `intervals` waits of `EVERY`: nothing is re-checked, and the host stays
/// tainted with no clear reported.
fn stays_tainted(h: &mut Harness, intervals: u32) {
    for _ in 0..intervals {
        tick(h, EVERY);
    }
    assert_eq!(rechecks(h), 0);
    assert!(h.life.is_tainted());
    assert_eq!(h.life.pending_taint_clear(), None);
    assert!(h.marker().exists());
}

/// Asserts that a pin made in memory survives a restart: a residue of a process first, then a
/// new mount sealed beside it, writes `taint.pinned` holding the marker's taint, byte for byte
/// as the marker holds it; the agent that starts over the same state root restores the
/// residue pinned, re-checks nothing across any number of waits and clear
/// acknowledgements, and stays tainted. Catches a restart that reads only the marker, takes a
/// baseline that already holds the new mount, and clears a host that holds a real change.
#[test]
fn a_pin_made_in_memory_survives_a_restart() {
    let mut h = pinned_in_process("pin-restart");
    assert_eq!(rechecks(&h), 0);
    let marker = fs::read(h.marker()).unwrap();
    assert_eq!(fs::read(pin_path(&h)).unwrap(), marker);
    assert!(!h.marker().with_file_name(PIN_TEMP_FILE).exists());

    h.restart().unwrap();
    assert_eq!(h.life.pending_taint().unwrap().reason, reason::RESIDUE);
    ack(&mut h);
    stays_tainted(&mut h, 4);
    assert_eq!(ack_clear(&mut h), []);
    assert!(h.life.is_tainted());
    assert_eq!(
        fs::read(pin_path(&h)).unwrap(),
        marker,
        "kept, not rewritten"
    );
}

/// Asserts that a pin file that cannot be written is a finding like a marker that cannot be
/// written, and that the pin stands: the host stays tainted, the failed write is noted
/// (`taint-write-failed`) and the slot of the job whose checks passed is held for it; once the
/// file can be written the next step writes it, and a restart then finds the taint pinned.
/// Catches a pin that is lost when its write fails once, and a failure that nobody hears of.
#[test]
fn a_pin_file_that_cannot_be_written_is_a_finding_and_is_tried_again() {
    let mut h = Harness::with(
        "pin-write-fails",
        Options {
            findings: vec![
                ("r1", residue()),
                ("r2", CheckFinding::residue(check::NEW_MOUNT, "/mnt/x")),
            ],
            ..Options::default()
        },
    );
    fs::create_dir(h.marker().with_file_name(PIN_TEMP_FILE)).unwrap();
    h.start(job("r1"));
    h.start(job("r2"));
    h.fake.exit("r1", 0);
    h.life.step();
    ack(&mut h);
    h.fake.exit("r2", 0);
    let events = names(&h.life.step());
    assert!(
        events.contains(&"finding r2 taint-write-failed".to_owned()),
        "{events:?}"
    );
    assert!(!pin_path(&h).exists());
    assert!(h.life.is_tainted());
    assert_eq!(
        h.life.free().cpu_millis,
        2000,
        "the slot is held for the pin file"
    );

    fs::remove_dir(h.marker().with_file_name(PIN_TEMP_FILE)).unwrap();
    h.life.step();
    assert_eq!(
        fs::read(pin_path(&h)).unwrap(),
        fs::read(h.marker()).unwrap()
    );
    h.restart().unwrap();
    ack(&mut h);
    stays_tainted(&mut h, 4);
}

/// Asserts that a pin file that names another taint, the stale file of a taint an operator
/// cleared, does not pin a restored residue: it clears after two clean re-checks and the
/// server's acknowledgement, as it does with no pin file. Catches a pin file that pins any
/// restored taint whatever it names.
#[test]
fn a_pin_file_naming_another_taint_does_not_pin() {
    let mut h = Harness::new("pin-stale");
    leave_residue(&h);
    h.config
        .state
        .write_pin(&Taint {
            reason: reason::RESIDUE.to_owned(),
            run_id: Some("r0".to_owned()),
            check: Some(check::DAEMONISED_CHILD.to_owned()),
        })
        .unwrap();
    h.restart().unwrap();
    ack(&mut h);
    h.life.step();
    tick(&mut h, EVERY);
    tick(&mut h, EVERY);
    assert_eq!(rechecks(&h), 2);
    assert!(h.life.pending_taint_clear().is_some());
    assert_eq!(names(&ack_clear(&mut h)), ["finding - taint-cleared"]);
    assert!(!h.life.is_tainted());
}

/// Asserts that a pin file that cannot be read pins, whether it is a file that is not a taint
/// or a directory: the restored residue stays tainted across any number of waits. A pin file
/// that is there is never taken for an absent one. Catches a reader that treats a damaged pin
/// file as no pin.
#[test]
fn a_pin_file_that_cannot_be_read_pins() {
    for (n, damage) in ["not json", ""].into_iter().enumerate() {
        let mut h = Harness::new(&format!("pin-unreadable-{n}"));
        leave_residue(&h);
        if damage.is_empty() {
            fs::create_dir(pin_path(&h)).unwrap();
        } else {
            fs::write(pin_path(&h), damage).unwrap();
        }
        h.restart().unwrap();
        ack(&mut h);
        stays_tainted(&mut h, 4);
    }
}

/// Asserts that a pin file that names the marker's taint pins it even when it was written by
/// an earlier process and the marker is the only other trace: the restored residue stays
/// tainted. Catches a pin file that is compared by anything less than the whole taint.
#[test]
fn a_pin_file_naming_the_markers_taint_pins() {
    let mut h = Harness::new("pin-names");
    let taint = leave_residue(&h);
    h.config.state.write_pin(&taint).unwrap();
    h.restart().unwrap();
    ack(&mut h);
    stays_tainted(&mut h, 4);
}
