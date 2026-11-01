//! The `launchctl` unit port: the command lines it runs, how it reads `launchctl print`, and
//! the labels it refuses. What a unit manager must do whichever it is lives in
//! `unit_conformance`; this is what only `launchctl` has: services that are booted out when
//! stopped, a start that bootstraps a plist or kicks a service that is loaded, and a state
//! text of its own.

use std::cell::{Cell, RefCell};
use std::path::Path;
use std::time::Duration;

use js_agent::CommandRunner;
use js_agent::update_hook::{
    BOOTOUT_MARGIN, Clock, LAUNCHCTL, LaunchUnits, PortError, SETTLE_READS, StopRunner, UnitState,
    Units,
};
use js_mac_provision::daemon::Daemon;
use js_testkit::dir::TestDir;

use super::unit_conformance::{Exit, Launchctl, Service, Then, plist_of};
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

/// A clock that waits for nothing and counts the waits.
#[derive(Default)]
struct Pauses(Cell<u32>);

impl Clock for Pauses {
    fn now_ms(&self) -> u64 {
        0
    }

    fn pause(&self, _: Duration) {
        self.0.set(self.0.get() + 1);
    }
}

/// What `launchctl print` prints of a service in `state`, with `extra` lines at the depth of
/// the service's own properties.
fn print_with(state: &str, extra: &str) -> String {
    format!("system/{LABEL} = {{\n\tstate = {state}\n{extra}}}\n")
}

fn read(state: &str, extra: &str) -> UnitState {
    let tool = FakeCommands::default().with(&print_line(LABEL), Ok(&print_with(state, extra)));
    LaunchUnits::new(&tool).show(LABEL).unwrap()
}

#[test]
fn a_service_that_is_not_running_reads_by_how_it_last_ended() {
    let died = "\truns = 2\n\tlast exit code = 1\n";
    let misconfigured = "\truns = 1\n\tlast exit code = 78: EX_CONFIG\n";
    let signalled =
        "\truns = 2\n\tlast exit code = (never exited)\n\tlast terminating signal = Killed: 9\n";
    let clean = "\truns = 1\n\tlast exit code = 0\n";
    let never = "\truns = 0\n\tlast exit code = (never exited)\n";
    for (state, extra, active, sub) in [
        // The program ran and died: what systemd calls `failed`.
        ("not running", died, "failed", "failed"),
        ("not running", misconfigured, "failed", "failed"),
        ("not running", signalled, "failed", "failed"),
        ("exited", died, "failed", "failed"),
        // It ended cleanly, never ran, or launchd printed nothing of it: not a crash.
        ("not running", clean, "inactive", "dead"),
        ("not running", never, "inactive", "dead"),
        ("not running", "", "inactive", "dead"),
        (
            "not running",
            "\tlast exit code = banana\n",
            "inactive",
            "dead",
        ),
        ("waiting", clean, "inactive", "dead"),
        // Waiting out the throttle after a crash is `auto-restart`; after a clean exit it is
        // only a start that is coming.
        ("spawn scheduled", died, "activating", "auto-restart"),
        ("spawn scheduled", signalled, "activating", "auto-restart"),
        ("spawn scheduled", "", "activating", "auto-restart"),
        ("spawn scheduled", clean, "activating", "start"),
        // The exit of an earlier run says nothing of one that runs.
        ("running", died, "active", "running"),
    ] {
        let got = read(state, extra);
        assert_eq!(
            (got.active.as_str(), got.sub.as_str()),
            (active, sub),
            "{state} {extra:?}"
        );
        assert_eq!(got.crashed(), active == "failed" || sub == "auto-restart");
    }
}

#[test]
fn the_exit_of_an_endpoint_or_a_trigger_is_not_the_exit_of_the_service() {
    let nested = "\tendpoints = {\n\t\t\"x\" = {\n\t\t\tlast exit code = 1\n\t\t}\n\t}\n";
    assert_eq!(read("not running", nested).active, "inactive");
}

#[test]
fn a_label_is_one_plain_file_name_of_visible_ascii() {
    let long = "a".repeat(128);
    let too_long = "a".repeat(129);
    for label in [
        "",
        "a/b",
        "a\\b",
        "a b",
        "a\tb",
        "a\nb",
        "a\x1fb",
        "a\x7fb",
        "a\0b",
        "caf\u{e9}",
        "\u{202e}gnp",
        ".",
        "..",
        too_long.as_str(),
    ] {
        let tool = FakeCommands::default();
        let units = LaunchUnits::new(&tool);
        assert!(
            matches!(units.show(label), Err(PortError::Command(_))),
            "{label:?}"
        );
        assert!(units.stop_no_block(label).is_err(), "{label:?}");
        assert!(units.start(label).is_err(), "{label:?}");
        assert_eq!(units.is_active(label), Ok(false), "{label:?}");
        assert_eq!(*tool.ran.borrow(), Vec::<String>::new(), "{label:?}");
    }
    for label in [LABEL, "-x", "a.b", "..a", "a..", long.as_str()] {
        let tool = FakeCommands::default().with(&print_line(label), Err("exit status: 113"));
        assert!(LaunchUnits::new(&tool).show(label).is_ok(), "{label:?}");
    }
}

fn launchd_with(service: Service) -> Launchctl {
    let tool = world();
    tool.load(LABEL, service);
    tool
}

fn prints(tool: &Launchctl) -> usize {
    tool.calls()
        .iter()
        .filter(|c| **c == print_line(LABEL))
        .count()
}

#[test]
fn start_waits_for_a_service_launchd_has_bootstrapped_and_not_yet_started() {
    let tool = world();
    tool.settle.set(3);
    let pauses = Pauses::default();
    let units = LaunchUnits::new(&tool).with_clock(&pauses);
    units.start(LABEL).unwrap();
    assert_eq!(pauses.0.get(), 3);
    assert_eq!(
        units.show(LABEL),
        Ok(UnitState {
            active: "active".to_owned(),
            sub: "running".to_owned()
        })
    );
}

#[test]
fn start_does_not_wait_for_a_service_that_runs_at_once_or_already_does() {
    let tool = world();
    let pauses = Pauses::default();
    LaunchUnits::new(&tool)
        .with_clock(&pauses)
        .start(LABEL)
        .unwrap();
    let tool = launchd_with(Service::running());
    LaunchUnits::new(&tool)
        .with_clock(&pauses)
        .start(LABEL)
        .unwrap();
    assert_eq!(pauses.0.get(), 0);
}

#[test]
fn the_wait_after_a_start_is_a_bounded_number_of_reads() {
    let tool = world();
    tool.settle.set(1000);
    let pauses = Pauses::default();
    LaunchUnits::new(&tool)
        .with_clock(&pauses)
        .start(LABEL)
        .unwrap();
    assert_eq!(pauses.0.get(), SETTLE_READS);
    assert_eq!(prints(&tool), 1 + SETTLE_READS as usize);
}

#[test]
fn a_program_that_dies_at_launch_ends_the_wait_and_reads_as_crashed() {
    for settle in [0, 2] {
        let tool = world();
        tool.settle.set(settle);
        tool.next_start.set(Then::Dies(Exit::Code(1)));
        let pauses = Pauses::default();
        let units = LaunchUnits::new(&tool).with_clock(&pauses);
        units.start(LABEL).unwrap();
        assert_eq!(pauses.0.get(), settle);
        assert!(units.show(LABEL).unwrap().crashed());
    }
}

#[test]
fn the_exit_of_the_run_before_does_not_end_the_wait_after_a_kickstart() {
    let tool = launchd_with(Service::new("not running", 2, Exit::Code(1)));
    tool.settle.set(2);
    let pauses = Pauses::default();
    let units = LaunchUnits::new(&tool).with_clock(&pauses);
    units.start(LABEL).unwrap();
    assert_eq!(pauses.0.get(), 2);
    assert!(!units.show(LABEL).unwrap().crashed());
    assert!(units.is_active(LABEL).unwrap());
}

#[test]
fn a_service_that_could_not_be_read_after_the_start_ends_the_wait() {
    let tool = FakeCommands::default()
        .with(&print_line(LABEL), Err("/bin/launchctl: exit status: 113"))
        .with(
            &format!("{LAUNCHCTL} bootstrap system {}", plist_of(LABEL)),
            Ok(""),
        );
    let pauses = Pauses::default();
    LaunchUnits::new(&tool)
        .with_clock(&pauses)
        .start(LABEL)
        .unwrap();
    assert_eq!(pauses.0.get(), 0);
}

/// Runs the commands in a world and records which of them it was given.
struct Recording<'a> {
    world: &'a Launchctl,
    seen: RefCell<Vec<String>>,
}

impl CommandRunner for Recording<'_> {
    fn run(&self, program: &str, args: &[&str]) -> Result<String, String> {
        self.seen
            .borrow_mut()
            .push(format!("{program} {}", args.join(" ")));
        self.world.run(program, args)
    }

    fn run_in(&self, program: &str, args: &[&str], env: &[(&str, &str)]) -> Result<String, String> {
        self.world.run_in(program, args, env)
    }
}

#[test]
fn bootout_is_run_by_the_stop_runner_and_nothing_else_is() {
    let tool = launchd_with(Service::running());
    let stopper = Recording {
        world: &tool,
        seen: RefCell::default(),
    };
    let units = LaunchUnits::new(&tool).with_stop_runner(&stopper);
    units.stop_no_block(LABEL).unwrap();
    assert_eq!(
        *stopper.seen.borrow(),
        [format!("{LAUNCHCTL} bootout system/{LABEL}")]
    );
    units.start(LABEL).unwrap();
    assert_eq!(stopper.seen.borrow().len(), 1);
    assert!(tool.calls().contains(&print_line(LABEL)));
}

#[test]
fn the_stop_runner_waits_longer_than_the_exit_timeout_of_the_agent_daemon() {
    let exit = Duration::from_secs(u64::from(
        Daemon::agent("https://x", "pool").exit_timeout_secs,
    ));
    assert!(BOOTOUT_MARGIN > Duration::ZERO);
    assert!(StopRunner::new(exit).limit() >= exit + BOOTOUT_MARGIN);
}

#[test]
fn the_stop_runner_runs_programs() {
    let runner = StopRunner::new(Duration::from_secs(5));
    assert_eq!(runner.run("/bin/echo", &["hi"]), Ok("hi\n".to_owned()));
    assert_eq!(
        runner.run_in("/bin/echo", &["there"], &[]),
        Ok("there\n".to_owned())
    );
}

fn gate_in(tmp: &TestDir) -> std::path::PathBuf {
    tmp.path().join("token")
}

#[test]
fn a_kickstart_of_a_loaded_idle_agent_is_refused_while_the_token_file_is_absent() {
    let tmp = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "launchd-gate");
    let gate = gate_in(&tmp);
    let tool = world();
    tool.load_idle(LABEL, "not running");
    let units = LaunchUnits::new(&tool).with_gate(&gate);
    let Err(PortError::Command(why)) = units.start(LABEL) else {
        panic!("a kickstart without the token must be refused");
    };
    assert!(why.contains(gate.to_str().unwrap()), "{why}");
    assert!(why.contains("kickstart"), "{why}");
    assert_eq!(tool.calls(), [print_line(LABEL)]);
    // With the file there, launchd would run the agent by its own rules: the kick goes.
    std::fs::write(&gate, "token").unwrap();
    units.start(LABEL).unwrap();
    assert!(
        tool.calls()
            .contains(&format!("{LAUNCHCTL} kickstart -k system/{LABEL}"))
    );
}

#[test]
fn a_bootstrap_is_left_to_launchds_own_gate_on_the_token_file() {
    let tmp = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "launchd-gate-bootstrap");
    let tool = world();
    let gate = gate_in(&tmp);
    assert!(!Path::new(&gate).exists());
    LaunchUnits::new(&tool)
        .with_gate(&gate)
        .start(LABEL)
        .unwrap();
    assert!(
        tool.calls()
            .contains(&format!("{LAUNCHCTL} bootstrap system {}", plist_of(LABEL)))
    );
}
