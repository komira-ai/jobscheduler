//! The `launchctl` unit port: the command lines it runs, how it reads `launchctl print`, and
//! the labels it refuses. What a unit manager must do whichever it is lives in
//! `unit_conformance`; this is what only `launchctl` has: services that are booted out when
//! stopped, a start that bootstraps a plist or kicks a service that is loaded, and a state
//! text of its own.

use js_agent::update_hook::{LAUNCHCTL, LaunchUnits, PortError, UnitState, Units};

use super::unit_conformance::{Launchctl, plist_of};
use crate::support::FakeCommands;

const LABEL: &str = "com.komira.js-agent";

fn print_line(label: &str) -> String {
    format!("{LAUNCHCTL} print system/{label}")
}

fn world() -> Launchctl {
    let tool = Launchctl::default();
    tool.plists.borrow_mut().insert(LABEL.to_owned());
    tool
}

#[test]
fn show_prints_the_service_of_the_system_domain_and_reads_its_state() {
    let tool = FakeCommands::default().with(
        &print_line(LABEL),
        Ok("system/com.komira.js-agent = {\n\tstate = running\n}\n"),
    );
    let state = LaunchUnits::new(&tool).show(LABEL).unwrap();
    assert_eq!(
        state,
        UnitState {
            active: "active".to_owned(),
            sub: "running".to_owned()
        }
    );
    assert_eq!(*tool.ran.borrow(), [print_line(LABEL)]);
}

#[test]
fn the_states_launchd_prints_map_to_the_states_the_steps_know() {
    for (printed, active, sub) in [
        ("running", "active", "running"),
        ("not running", "inactive", "dead"),
        ("waiting", "inactive", "dead"),
        ("exited", "inactive", "dead"),
        ("spawn scheduled", "activating", "auto-restart"),
        ("spawning", "activating", "start"),
    ] {
        let text = format!("system/{LABEL} = {{\n\tstate = {printed}\n}}\n");
        let tool = FakeCommands::default().with(&print_line(LABEL), Ok(&text));
        let got = LaunchUnits::new(&tool).show(LABEL).unwrap();
        assert_eq!(
            (got.active.as_str(), got.sub.as_str()),
            (active, sub),
            "{printed}"
        );
    }
}

#[test]
fn only_the_services_own_state_line_counts_never_one_of_its_endpoints() {
    let text = format!(
        "system/{LABEL} = {{\n\tendpoints = {{\n\t\t\"x\" = {{\n\t\t\tstate = active\n\t\t}}\n\t}}\n\
         \tstate = not running\n}}\n"
    );
    let tool = FakeCommands::default().with(&print_line(LABEL), Ok(&text));
    assert_eq!(
        LaunchUnits::new(&tool).show(LABEL).unwrap().active,
        "inactive"
    );
    // No state of its own: the nested one is not read in its place.
    let text = format!("system/{LABEL} = {{\n\tendpoints = {{\n\t\t\tstate = active\n\t}}\n}}\n");
    let tool = FakeCommands::default().with(&print_line(LABEL), Ok(&text));
    let PortError::Parse(why) = LaunchUnits::new(&tool).show(LABEL).unwrap_err() else {
        panic!("not a parse error");
    };
    assert!(why.contains("state"), "{why}");
}

#[test]
fn a_state_launchd_never_printed_before_is_refused_by_name() {
    let text = format!("system/{LABEL} = {{\n\tstate = frobnicating\n}}\n");
    let tool = FakeCommands::default().with(&print_line(LABEL), Ok(&text));
    let PortError::Parse(why) = LaunchUnits::new(&tool).show(LABEL).unwrap_err() else {
        panic!("not a parse error");
    };
    assert!(why.contains("frobnicating"), "{why}");
}

#[test]
fn a_service_launchd_does_not_know_is_inactive_and_any_other_failure_is_an_error() {
    let tool =
        FakeCommands::default().with(&print_line(LABEL), Err("/bin/launchctl: exit status: 113"));
    let units = LaunchUnits::new(&tool);
    assert_eq!(
        units.show(LABEL),
        Ok(UnitState {
            active: "inactive".to_owned(),
            sub: "dead".to_owned()
        })
    );
    for failure in [
        "/bin/launchctl: exit status: 1",
        "/bin/launchctl: timed out after 10s",
    ] {
        let tool = FakeCommands::default().with(&print_line(LABEL), Err(failure));
        assert_eq!(
            LaunchUnits::new(&tool).show(LABEL),
            Err(PortError::Command(failure.to_owned()))
        );
    }
}

#[test]
fn stop_boots_a_loaded_service_out_of_the_system_domain_and_does_nothing_to_one_not_loaded() {
    let tool = world();
    tool.load_idle(LABEL, "running");
    let units = LaunchUnits::new(&tool);
    units.stop_no_block(LABEL).unwrap();
    assert!(tool.loaded.borrow().is_empty());
    units.stop_no_block(LABEL).unwrap();
    assert_eq!(
        tool.calls(),
        [
            print_line(LABEL),
            format!("{LAUNCHCTL} bootout system/{LABEL}"),
            print_line(LABEL),
        ]
    );
}

#[test]
fn a_stop_does_not_boot_out_a_service_it_could_not_look_at() {
    let tool =
        FakeCommands::default().with(&print_line(LABEL), Err("/bin/launchctl: exit status: 1"));
    assert!(LaunchUnits::new(&tool).stop_no_block(LABEL).is_err());
    assert_eq!(*tool.ran.borrow(), [print_line(LABEL)]);
}

#[test]
fn start_bootstraps_the_plist_of_the_label_when_nothing_is_loaded() {
    let tool = world();
    LaunchUnits::new(&tool).start(LABEL).unwrap();
    assert_eq!(
        tool.calls(),
        [
            print_line(LABEL),
            format!("{LAUNCHCTL} bootstrap system {}", plist_of(LABEL)),
        ]
    );
}

#[test]
fn start_kicks_a_loaded_service_that_is_not_running_and_leaves_a_running_one() {
    let tool = world();
    tool.load_idle(LABEL, "not running");
    LaunchUnits::new(&tool).start(LABEL).unwrap();
    assert_eq!(
        tool.calls(),
        [
            print_line(LABEL),
            format!("{LAUNCHCTL} kickstart -k system/{LABEL}"),
        ]
    );
    let tool = world();
    tool.load_idle(LABEL, "running");
    LaunchUnits::new(&tool).start(LABEL).unwrap();
    assert_eq!(tool.calls(), [print_line(LABEL)]);
}

#[test]
fn is_active_is_true_only_for_a_running_service() {
    for (printed, want) in [
        ("running", true),
        ("not running", false),
        ("spawn scheduled", false),
    ] {
        let text = format!("system/{LABEL} = {{\n\tstate = {printed}\n}}\n");
        let tool = FakeCommands::default().with(&print_line(LABEL), Ok(&text));
        assert_eq!(
            LaunchUnits::new(&tool).is_active(LABEL),
            Ok(want),
            "{printed}"
        );
    }
}

#[test]
fn a_label_that_would_leave_the_directory_of_the_plists_is_refused_before_any_command() {
    let tool = FakeCommands::default();
    let units = LaunchUnits::new(&tool);
    for label in ["", "a/b", "../escape", "nul\0byte"] {
        assert!(
            matches!(units.show(label), Err(PortError::Command(_))),
            "{label:?}"
        );
        assert!(units.stop_no_block(label).is_err(), "{label:?}");
        assert!(units.start(label).is_err(), "{label:?}");
        assert_eq!(units.is_active(label), Ok(false), "{label:?}");
    }
    assert_eq!(*tool.ran.borrow(), Vec::<String>::new());
}

#[test]
fn a_label_that_begins_with_a_hyphen_is_never_an_option() {
    let tool = world();
    tool.plists.borrow_mut().insert("-x".to_owned());
    let units = LaunchUnits::new(&tool);
    units.start("-x").unwrap();
    units.stop_no_block("-x").unwrap();
    for line in tool.calls() {
        let words: Vec<_> = line.split(' ').skip(2).collect();
        assert!(
            words.iter().all(|word| *word != "-x"),
            "a bare `-x` could be read as an option: {line}"
        );
    }
}
