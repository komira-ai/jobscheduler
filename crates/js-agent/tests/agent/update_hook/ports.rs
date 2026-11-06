//! The machine's side of the ports: the command lines of the unit manager, the atomic swap of
//! the release link, the clock and the gate.

use std::cell::Cell;
use std::fs;
use std::rc::Rc;
use std::time::Duration;

use js_agent::update_hook::{
    Clock, Gate, Link, NoGate, PlatformUnits, PortError, SystemClock, SystemLink, SystemUnits,
    Units, read_link,
};
use js_testkit::dir::TestDir;

use crate::support::FakeCommands;

const SHOW: &str = "/usr/bin/systemctl show -p ActiveState,SubState -- js-agent-crun.service";
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
    let stop = format!("/usr/bin/systemctl stop --no-block -- {UNIT}");
    let start = format!("/usr/bin/systemctl start -- {UNIT}");
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
    let line = format!("/usr/bin/systemctl is-active -- {UNIT}");
    for (answer, want) in [
        (Ok("active\n"), true),
        (Ok("activating\n"), false),
        (Err("systemctl: exit 3"), false),
    ] {
        let runner = FakeCommands::default().with(&line, answer);
        assert_eq!(SystemUnits::new(&runner).is_active(UNIT), Ok(want));
        assert_eq!(runner.ran.borrow().as_slice(), std::slice::from_ref(&line));
    }
}

#[test]
fn show_refuses_an_empty_value_and_a_property_printed_twice() {
    for (text, why) in [
        (
            "ActiveState=\nSubState=dead\n",
            "systemctl show printed an empty ActiveState",
        ),
        (
            "ActiveState=active\nSubState=\n",
            "systemctl show printed an empty SubState",
        ),
        (
            "ActiveState=active\nSubState=running\nSubState=dead\n",
            "systemctl show printed SubState twice",
        ),
    ] {
        let runner = FakeCommands::default().with(SHOW, Ok(text));
        let got = SystemUnits::new(&runner).show(UNIT).unwrap_err();
        assert_eq!(got, PortError::Parse(why.to_owned()), "{text:?}");
    }
}

#[test]
fn a_unit_named_like_an_option_is_passed_after_the_end_of_options() {
    let runner = FakeCommands::default();
    let units = SystemUnits::new(&runner);
    let _ = (
        units.show("-x"),
        units.stop_no_block("-x"),
        units.start("-x"),
    );
    let _ = units.is_active("-x");
    assert_eq!(
        *runner.ran.borrow(),
        [
            "/usr/bin/systemctl show -p ActiveState,SubState -- -x",
            "/usr/bin/systemctl stop --no-block -- -x",
            "/usr/bin/systemctl start -- -x",
            "/usr/bin/systemctl is-active -- -x",
        ]
    );
}

#[test]
fn the_unit_port_of_the_platform_is_the_one_of_the_build() {
    let want = if cfg!(target_os = "macos") {
        "/bin/launchctl print system/agent"
    } else {
        "/usr/bin/systemctl show -p ActiveState,SubState -- agent"
    };
    let runner = FakeCommands::default();
    let _ = PlatformUnits::new(&runner).show("agent");
    assert_eq!(*runner.ran.borrow(), [want]);
}

/// The hook reads the release link and never writes it: the target is what the link says, as
/// written (a relative one stays relative), a missing link has none, and something that is not
/// a link is an error.
///
/// Catches a read that follows the link, or that takes a missing link for an error.
#[test]
fn the_target_of_a_link_is_read_as_written_and_something_that_is_not_a_link_is_an_error() {
    let tmp = dir("hook-link-target");
    let link = tmp.path().join("current");
    assert_eq!(SystemLink.target(&link), Ok(None));
    std::os::unix::fs::symlink("1.4.0", &link).unwrap();
    assert_eq!(SystemLink.target(&link), Ok(Some("1.4.0".into())));
    let file = tmp.path().join("plain");
    fs::write(&file, "x").unwrap();
    assert!(read_link(&file).is_err());
    assert!(matches!(SystemLink.target(&file), Err(PortError::Io(_))));
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
