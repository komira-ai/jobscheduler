//! The machine's side of the ports: the command lines of the unit manager, the atomic swap of
//! the release link, the clock and the gate.

use std::cell::Cell;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use js_agent::update_hook::{
    Clock, Gate, Link, NoGate, PortError, SystemClock, SystemLink, SystemUnits, Units, read_link,
    swap_link,
};
use js_testkit::dir::TestDir;

use crate::support::FakeCommands;

const SHOW: &str = "/usr/bin/systemctl show -p ActiveState,SubState js-agent-crun.service";
const UNIT: &str = "js-agent-crun.service";

fn dir(name: &str) -> TestDir {
    TestDir::under(env!("CARGO_TARGET_TMPDIR"), name)
}

#[test]
fn show_asks_for_the_two_properties_and_reads_them_in_either_order() {
    for text in [
        "ActiveState=deactivating\nSubState=stop-sigterm\n",
        "SubState=stop-sigterm\nActiveState=deactivating\n",
    ] {
        let runner = FakeCommands::default().with(SHOW, Ok(text));
        let state = SystemUnits::new(&runner).show(UNIT).unwrap();
        assert_eq!(
            (state.active.as_str(), state.sub.as_str()),
            ("deactivating", "stop-sigterm")
        );
        assert_eq!(*runner.ran.borrow(), [SHOW]);
    }
}

#[test]
fn show_names_what_is_missing_and_what_failed() {
    let runner = FakeCommands::default().with(SHOW, Ok("ActiveState=active\n"));
    let why = SystemUnits::new(&runner).show(UNIT).unwrap_err();
    assert_eq!(
        why,
        PortError::Parse("systemctl show printed no SubState".to_owned())
    );
    let runner = FakeCommands::default().with(SHOW, Err("systemctl: exit 1"));
    let why = SystemUnits::new(&runner).show(UNIT).unwrap_err();
    assert_eq!(why, PortError::Command("systemctl: exit 1".to_owned()));
}

#[test]
fn stop_is_asked_without_waiting_and_start_by_name() {
    let stop = format!("/usr/bin/systemctl stop --no-block {UNIT}");
    let start = format!("/usr/bin/systemctl start {UNIT}");
    let runner = FakeCommands::default()
        .with(&stop, Ok(""))
        .with(&start, Ok(""));
    let units = SystemUnits::new(&runner);
    units.stop_no_block(UNIT).unwrap();
    units.start(UNIT).unwrap();
    assert_eq!(*runner.ran.borrow(), [stop, start]);
}

#[test]
fn a_failed_stop_or_start_is_an_error_naming_the_program() {
    let runner = FakeCommands::default();
    let units = SystemUnits::new(&runner);
    for why in [units.stop_no_block(UNIT), units.start(UNIT)] {
        let PortError::Command(text) = why.unwrap_err() else {
            panic!("not a command error");
        };
        assert!(text.contains("/usr/bin/systemctl"), "{text}");
    }
}

#[test]
fn is_active_is_true_only_for_active_and_a_failure_reads_as_not_active() {
    let line = format!("/usr/bin/systemctl is-active {UNIT}");
    for (answer, want) in [
        (Ok("active\n"), true),
        (Ok("activating\n"), false),
        (Err("systemctl: exit 3"), false),
    ] {
        let runner = FakeCommands::default().with(&line, answer);
        assert_eq!(SystemUnits::new(&runner).is_active(UNIT), Ok(want));
        assert_eq!(*runner.ran.borrow(), [line.clone()]);
    }
}

fn target_of(link: &Path) -> PathBuf {
    fs::read_link(link).unwrap()
}

#[test]
fn relink_makes_the_link_and_repoints_it_without_leaving_the_new_name_behind() {
    let tmp = dir("hook-relink");
    let link = tmp.path().join("current");
    assert_eq!(SystemLink.target(&link), Ok(None));
    SystemLink.relink(&link, Path::new("releases/1.0")).unwrap();
    assert_eq!(SystemLink.target(&link), Ok(Some("releases/1.0".into())));
    SystemLink.relink(&link, Path::new("releases/1.1")).unwrap();
    assert_eq!(target_of(&link), Path::new("releases/1.1"));
    let names: Vec<_> = fs::read_dir(tmp.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(names, ["current"]);
}

#[test]
fn relink_to_the_target_it_has_changes_nothing() {
    let tmp = dir("hook-relink-idem");
    let link = tmp.path().join("current");
    swap_link(&link, Path::new("a")).unwrap();
    let before = fs::symlink_metadata(&link).unwrap();
    swap_link(&link, Path::new("a")).unwrap();
    let after = fs::symlink_metadata(&link).unwrap();
    assert_eq!(
        (before.ino(), before.ctime_nsec()),
        (after.ino(), after.ctime_nsec())
    );
}

#[test]
fn relink_replaces_a_leftover_of_an_interrupted_swap() {
    let tmp = dir("hook-relink-leftover");
    let link = tmp.path().join("current");
    swap_link(&link, Path::new("old")).unwrap();
    std::os::unix::fs::symlink("stale", tmp.path().join("current.relink")).unwrap();
    swap_link(&link, Path::new("new")).unwrap();
    assert_eq!(target_of(&link), Path::new("new"));
    assert!(!tmp.path().join("current.relink").exists());
}

#[test]
fn a_swap_that_cannot_complete_leaves_the_link_as_it_was() {
    let tmp = dir("hook-relink-fails");
    let link = tmp.path().join("current");
    swap_link(&link, Path::new("old")).unwrap();
    // A directory where the new link goes: it cannot be removed as a file.
    fs::create_dir(tmp.path().join("current.relink")).unwrap();
    assert!(swap_link(&link, Path::new("new")).is_err());
    assert_eq!(target_of(&link), Path::new("old"));
    let err = SystemLink.relink(&link, Path::new("new")).unwrap_err();
    assert!(matches!(err, PortError::Io(_)), "{err:?}");
}

#[test]
fn the_target_of_something_that_is_not_a_link_is_an_error() {
    let tmp = dir("hook-relink-file");
    let file = tmp.path().join("plain");
    fs::write(&file, "x").unwrap();
    assert!(read_link(&file).is_err());
    assert!(matches!(SystemLink.target(&file), Err(PortError::Io(_))));
    assert!(swap_link(&tmp.path().join("missing/dir/link"), Path::new("t")).is_err());
}

#[test]
fn the_machine_clock_tells_the_time_and_pausing_for_nothing_returns() {
    let clock = SystemClock;
    // After 2020 and before the year 5000, in milliseconds.
    assert!((1_600_000_000_000..95_000_000_000_000).contains(&clock.now_ms()));
    clock.pause(Duration::ZERO);
}

struct Counting(Rc<Cell<u32>>);

impl Gate for Counting {
    fn point(&self) {
        self.0.set(self.0.get() + 1);
    }
}

#[test]
fn the_machine_gate_does_nothing_and_the_ports_point_goes_through_the_gate() {
    NoGate.point();
    let count = Rc::new(Cell::new(0));
    let gate = Counting(Rc::clone(&count));
    let tmp = dir("hook-ports-point");
    let world = super::fakes::World::new(
        &tmp.path().join("world"),
        &tmp.path().join("control"),
        Box::new(NoGate),
    );
    let state = world.state_root();
    let ports = js_agent::update_hook::Ports {
        units: &world,
        link: &world,
        lease: &world,
        state: &state,
        clock: &world,
        gate: &gate,
    };
    ports.point();
    ports.point();
    assert_eq!(count.get(), 2);
}
