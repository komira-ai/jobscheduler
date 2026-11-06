//! The `launchctl` unit port: the command lines it runs, how it reads `launchctl print`, the
//! labels it refuses, the wait after a start and the token-file gate on a kickstart. Every
//! command goes through a scripted runner, so nothing here touches a real launchd.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::Path;
use std::time::Duration;

use js_testkit::dir::TestDir;
use js_update_hook::{
    Clock, CommandRunner, LAUNCHCTL, LaunchUnits, PortError, SETTLE_READS, UnitState, Units,
};

const LABEL: &str = "com.komira.js-agent";
const GONE: &str = "/bin/launchctl: exit status: 113";

fn print_line(label: &str) -> String {
    format!("{LAUNCHCTL} print system/{label}")
}

fn bootstrap_line() -> String {
    format!("{LAUNCHCTL} bootstrap system /Library/LaunchDaemons/{LABEL}.plist")
}

fn kick_line() -> String {
    format!("{LAUNCHCTL} kickstart -k system/{LABEL}")
}

fn bootout_line() -> String {
    format!("{LAUNCHCTL} bootout system/{LABEL}")
}

type Rule = (String, VecDeque<Result<String, String>>);

/// A runner that answers each command line from a script: the answers of one line are given in
/// order and the last one repeats. A line nobody scripted fails, and every line is recorded.
#[derive(Default)]
struct Script {
    rules: RefCell<Vec<Rule>>,
    ran: RefCell<Vec<String>>,
}

impl Script {
    fn on(self, line: &str, answers: Vec<Result<String, String>>) -> Self {
        self.rules
            .borrow_mut()
            .push((line.to_owned(), answers.into()));
        self
    }

    fn prints(self, texts: &[&str]) -> Self {
        let answers = texts.iter().map(|t| Ok((*t).to_owned())).collect();
        self.on(&print_line(LABEL), answers)
    }

    fn print_answers(self, answers: Vec<Result<String, String>>) -> Self {
        self.on(&print_line(LABEL), answers)
    }

    fn ok(self, line: &str) -> Self {
        self.on(line, vec![Ok(String::new())])
    }

    fn calls(&self) -> Vec<String> {
        self.ran.borrow().clone()
    }

    fn print_count(&self) -> usize {
        self.calls()
            .iter()
            .filter(|c| **c == print_line(LABEL))
            .count()
    }
}

impl CommandRunner for Script {
    fn run(&self, program: &str, args: &[&str]) -> Result<String, String> {
        let line = format!("{program} {}", args.join(" "));
        self.ran.borrow_mut().push(line.clone());
        let mut rules = self.rules.borrow_mut();
        match rules.iter_mut().find(|(l, _)| *l == line) {
            Some((_, answers)) if answers.len() > 1 => answers.pop_front().unwrap(),
            Some((_, answers)) => answers.front().unwrap().clone(),
            None => Err(format!("unscripted command: {line}")),
        }
    }

    fn run_in(&self, program: &str, _: &[&str], _: &[(&str, &str)]) -> Result<String, String> {
        Err(format!("{program}: run_in is not used by launchctl"))
    }
}

/// A clock that waits for nothing and records every pause it was asked for.
#[derive(Default)]
struct Pauses(RefCell<Vec<Duration>>);

impl Pauses {
    fn count(&self) -> usize {
        self.0.borrow().len()
    }
}

impl Clock for Pauses {
    fn now_ms(&self) -> u64 {
        0
    }

    fn pause(&self, span: Duration) {
        self.0.borrow_mut().push(span);
    }
}

/// What `launchctl print` prints of the service in `state`, with `extra` lines at the depth of
/// the service's own properties.
fn print_with(state: &str, extra: &str) -> String {
    format!("system/{LABEL} = {{\n\tstate = {state}\n{extra}}}\n")
}

fn state(active: &str, sub: &str) -> UnitState {
    UnitState {
        active: active.to_owned(),
        sub: sub.to_owned(),
    }
}

fn show_text(text: &str) -> Result<UnitState, PortError> {
    let tool = Script::default().prints(&[text]);
    LaunchUnits::new(&tool).show(LABEL)
}

fn reads(printed: &str, extra: &str) -> (String, String) {
    let got = show_text(&print_with(printed, extra)).unwrap();
    (got.active, got.sub)
}

fn assert_reads(printed: &str, extra: &str, active: &str, sub: &str) {
    assert_eq!(
        reads(printed, extra),
        (active.to_owned(), sub.to_owned()),
        "{printed} {extra:?}"
    );
}

const DIED: &str = "\truns = 2\n\tlast exit code = 1\n";
const CONFIG: &str = "\truns = 1\n\tlast exit code = 78: EX_CONFIG\n";
const SIGNALLED: &str =
    "\truns = 2\n\tlast exit code = (never exited)\n\tlast terminating signal = Killed: 9\n";
const CLEAN: &str = "\truns = 1\n\tlast exit code = 0\n";
const NEVER: &str = "\truns = 0\n\tlast exit code = (never exited)\n";

/// `show` runs one `print` of the system domain and reads the service's own `state`.
#[test]
fn show_prints_the_service_of_the_system_domain_and_reads_its_state() {
    let tool =
        Script::default().prints(&["system/com.komira.js-agent = {\n\tstate = running\n}\n"]);
    assert_eq!(
        LaunchUnits::new(&tool).show(LABEL),
        Ok(state("active", "running"))
    );
    assert_eq!(tool.calls(), [print_line(LABEL)]);
}

/// Every state launchd prints maps to the systemd words the steps know, whatever the exit.
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
        assert_reads(printed, "", active, sub);
    }
}

/// The table of the module documentation, row by row: not running, exited and waiting read by
/// how the service last ended.
#[test]
fn a_service_that_is_not_running_reads_by_how_it_last_ended() {
    for (printed, extra, active, sub) in [
        ("not running", DIED, "failed", "failed"),
        ("not running", CONFIG, "failed", "failed"),
        ("not running", SIGNALLED, "failed", "failed"),
        ("exited", DIED, "failed", "failed"),
        ("waiting", DIED, "failed", "failed"),
        ("waiting", SIGNALLED, "failed", "failed"),
        ("not running", CLEAN, "inactive", "dead"),
        ("not running", NEVER, "inactive", "dead"),
        ("not running", "", "inactive", "dead"),
        (
            "not running",
            "\tlast exit code = banana\n",
            "inactive",
            "dead",
        ),
        ("waiting", CLEAN, "inactive", "dead"),
        ("exited", CLEAN, "inactive", "dead"),
    ] {
        assert_reads(printed, extra, active, sub);
    }
}

/// Waiting out the throttle after a crash is `auto-restart`; after a clean exit it is only a
/// start; a running service ignores the exit of an earlier run.
#[test]
fn a_spawn_scheduled_service_reads_by_how_it_last_ended_and_a_running_one_ignores_it() {
    for (printed, extra, active, sub) in [
        ("spawn scheduled", DIED, "activating", "auto-restart"),
        ("spawn scheduled", SIGNALLED, "activating", "auto-restart"),
        ("spawn scheduled", NEVER, "activating", "auto-restart"),
        ("spawn scheduled", "", "activating", "auto-restart"),
        ("spawn scheduled", CLEAN, "activating", "start"),
        ("running", DIED, "active", "running"),
        ("running", "", "active", "running"),
    ] {
        assert_reads(printed, extra, active, sub);
    }
}

/// Only the states that mean a crash are `crashed()`: failed ones and the throttle wait after
/// a bad end.
#[test]
fn a_crash_reads_as_crashed_and_a_clean_end_or_a_start_does_not() {
    let crashed = |printed: &str, extra: &str| show_text(&print_with(printed, extra)).unwrap();
    assert!(crashed("not running", DIED).crashed());
    assert!(crashed("spawn scheduled", DIED).crashed());
    assert!(!crashed("not running", CLEAN).crashed());
    assert!(!crashed("spawn scheduled", CLEAN).crashed());
    assert!(!crashed("running", DIED).crashed());
}

/// A terminating-signal line makes the end bad whatever the exit code says.
#[test]
fn an_exit_code_of_zero_with_a_terminating_signal_line_reads_as_a_bad_end() {
    let both = "\truns = 3\n\tlast exit code = 0\n\tlast terminating signal = Killed: 9\n";
    assert_reads("not running", both, "failed", "failed");
    assert_reads("exited", both, "failed", "failed");
    assert_reads("spawn scheduled", both, "activating", "auto-restart");
    assert_reads("running", both, "active", "running");
}

/// A negative code, with or without text after the colon, is a bad end.
#[test]
fn a_negative_exit_code_is_a_bad_end() {
    for code in ["-1", "-9", "-15: Terminated"] {
        let extra = format!("\truns = 2\n\tlast exit code = {code}\n");
        assert_reads("not running", &extra, "failed", "failed");
        assert_reads("spawn scheduled", &extra, "activating", "auto-restart");
    }
}

/// A spawning service is a start under way, however the last run ended.
#[test]
fn a_spawning_service_reads_as_a_start_however_it_last_ended() {
    for extra in [DIED, SIGNALLED, CLEAN, NEVER, ""] {
        assert_reads("spawning", extra, "activating", "start");
    }
}

/// A state launchd never printed before is an error that names it, exactly.
#[test]
fn a_state_launchd_never_printed_before_is_refused_by_name() {
    assert_eq!(
        show_text(&print_with("frobnicating", "")),
        Err(PortError::Parse(
            "launchctl print printed the unknown state \"frobnicating\"".to_owned()
        ))
    );
}

/// Output with no state of the service's own is an error, even if an endpoint has one.
#[test]
fn only_the_services_own_state_line_counts_never_one_of_its_endpoints() {
    let text = format!(
        "system/{LABEL} = {{\n\tendpoints = {{\n\t\t\"x\" = {{\n\t\t\tstate = active\n\t\t}}\n\t}}\n\
         \tstate = not running\n}}\n"
    );
    assert_eq!(show_text(&text).unwrap().active, "inactive");
    let text = format!("system/{LABEL} = {{\n\tendpoints = {{\n\t\t\tstate = active\n\t}}\n}}\n");
    assert_eq!(
        show_text(&text),
        Err(PortError::Parse(
            "launchctl print printed no state".to_owned()
        ))
    );
    assert_eq!(
        show_text(""),
        Err(PortError::Parse(
            "launchctl print printed no state".to_owned()
        ))
    );
}

/// A property whose name only begins with the key, and a line with no tab, are not the key.
#[test]
fn a_property_that_only_starts_with_the_key_is_not_the_key() {
    let text = format!("system/{LABEL} = {{\n\tstate_note = running\n\tstate = not running\n}}\n");
    assert_eq!(show_text(&text).unwrap().active, "inactive");
    let text = format!("system/{LABEL} = {{\n\tstate_note = running\n}}\n");
    assert!(matches!(show_text(&text), Err(PortError::Parse(_))));
}

/// An exit code of an endpoint or trigger is not the exit of the service.
#[test]
fn the_exit_of_an_endpoint_or_a_trigger_is_not_the_exit_of_the_service() {
    let nested = "\tendpoints = {\n\t\t\"x\" = {\n\t\t\tlast exit code = 1\n\t\t}\n\t}\n";
    assert_reads("not running", nested, "inactive", "dead");
}

/// Exit status 113 of `print` means not loaded, and reads inactive.
#[test]
fn a_service_launchd_does_not_know_is_inactive_and_any_other_failure_is_an_error() {
    let tool = Script::default().print_answers(vec![Err(GONE.to_owned())]);
    assert_eq!(
        LaunchUnits::new(&tool).show(LABEL),
        Ok(state("inactive", "dead"))
    );
    for failure in [
        "/bin/launchctl: exit status: 1",
        "/bin/launchctl: timed out after 10s",
    ] {
        let tool = Script::default().print_answers(vec![Err(failure.to_owned())]);
        assert_eq!(
            LaunchUnits::new(&tool).show(LABEL),
            Err(PortError::Command(failure.to_owned()))
        );
    }
}

/// Only exactly status 113 means not loaded, in every operation; a stop that could not look
/// must not report success.
#[test]
fn only_the_exact_exit_status_113_means_the_service_is_not_loaded() {
    let gone = Script::default().print_answers(vec![Err(GONE.to_owned())]);
    let units = LaunchUnits::new(&gone);
    assert_eq!(units.is_active(LABEL), Ok(false));
    assert_eq!(units.stop_no_block(LABEL), Ok(()));
    assert_eq!(gone.calls(), vec![print_line(LABEL); 2]);
    for other in [
        "exit status: 11",
        "exit status: 1130",
        "exit status: 2113",
        "exit status: 13",
    ] {
        let why = format!("/bin/launchctl: {other}");
        let tool = Script::default().print_answers(vec![Err(why.clone())]);
        let units = LaunchUnits::new(&tool);
        assert_eq!(units.show(LABEL), Err(PortError::Command(why.clone())));
        assert_eq!(
            units.stop_no_block(LABEL),
            Err(PortError::Command(why.clone()))
        );
        assert_eq!(units.start(LABEL), Err(PortError::Command(why.clone())));
        assert_eq!(tool.calls(), vec![print_line(LABEL); 3], "{other}");
    }
}

/// A label that is not one plain visible-ASCII file name is refused before any command, by
/// every operation, with the rule in the message.
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
        let tool = Script::default();
        let units = LaunchUnits::new(&tool);
        let refused = PortError::Command(format!(
            "{label:?} is not a launchd label (1 to 128 visible ASCII characters, \
             no / or \\, not . or ..)"
        ));
        assert_eq!(units.show(label), Err(refused));
        assert!(units.stop_no_block(label).is_err(), "{label:?}");
        assert!(units.start(label).is_err(), "{label:?}");
        assert_eq!(units.is_active(label), Ok(false), "{label:?}");
        assert_eq!(tool.calls(), Vec::<String>::new(), "{label:?}");
    }
    for label in [
        LABEL,
        "-x",
        "a.b",
        "..a",
        "a..",
        "!",
        "~",
        "[",
        "]",
        long.as_str(),
    ] {
        let tool = Script::default().on(&print_line(label), vec![Err(GONE.to_owned())]);
        assert_eq!(
            LaunchUnits::new(&tool).show(label),
            Ok(state("inactive", "dead"))
        );
    }
}

/// A stop boots a loaded service out of the system domain; one that is not loaded gets no
/// bootout.
#[test]
fn stop_boots_a_loaded_service_out_of_the_system_domain_and_does_nothing_to_one_not_loaded() {
    let tool = Script::default()
        .print_answers(vec![Ok(print_with("running", "")), Err(GONE.to_owned())])
        .ok(&bootout_line());
    let units = LaunchUnits::new(&tool);
    units.stop_no_block(LABEL).unwrap();
    units.stop_no_block(LABEL).unwrap();
    assert_eq!(
        tool.calls(),
        [print_line(LABEL), bootout_line(), print_line(LABEL)]
    );
}

/// A stop that cannot look at the service boots nothing out and fails.
#[test]
fn a_stop_does_not_boot_out_a_service_it_could_not_look_at() {
    let tool =
        Script::default().print_answers(vec![Err("/bin/launchctl: exit status: 1".to_owned())]);
    assert_eq!(
        LaunchUnits::new(&tool).stop_no_block(LABEL),
        Err(PortError::Command(
            "/bin/launchctl: exit status: 1".to_owned()
        ))
    );
    assert_eq!(tool.calls(), [print_line(LABEL)]);
}

/// A failing bootout is the error of the stop, text unchanged.
#[test]
fn a_bootout_that_fails_is_the_error_of_the_stop() {
    let tool = Script::default().prints(&[&print_with("running", "")]).on(
        &bootout_line(),
        vec![Err("/bin/launchctl: exit status: 5".to_owned())],
    );
    assert_eq!(
        LaunchUnits::new(&tool).stop_no_block(LABEL),
        Err(PortError::Command(
            "/bin/launchctl: exit status: 5".to_owned()
        ))
    );
}

/// `bootout` goes to the stop runner when one was given, and nothing else does.
#[test]
fn bootout_is_run_by_the_stop_runner_and_nothing_else_is() {
    let tool = Script::default().prints(&[&print_with("running", "")]);
    let stopper = Script::default().ok(&bootout_line());
    let units = LaunchUnits::new(&tool).with_stop_runner(&stopper);
    units.stop_no_block(LABEL).unwrap();
    assert_eq!(stopper.calls(), [bootout_line()]);
    assert_eq!(tool.calls(), [print_line(LABEL)]);
    units.start(LABEL).unwrap();
    assert_eq!(stopper.calls().len(), 1);
}

/// Without a stop runner the main runner runs `bootout`.
#[test]
fn without_a_stop_runner_the_main_runner_boots_out() {
    let tool = Script::default()
        .prints(&[&print_with("running", "")])
        .ok(&bootout_line());
    LaunchUnits::new(&tool).stop_no_block(LABEL).unwrap();
    assert_eq!(tool.calls(), [print_line(LABEL), bootout_line()]);
}

/// A start with nothing loaded bootstraps the label's plist from the daemons directory and
/// reads once to see the service begin.
#[test]
fn start_bootstraps_the_plist_of_the_label_when_nothing_is_loaded() {
    let tool = Script::default()
        .print_answers(vec![Err(GONE.to_owned()), Ok(print_with("running", ""))])
        .ok(&bootstrap_line());
    let pauses = Pauses::default();
    LaunchUnits::new(&tool)
        .with_clock(&pauses)
        .start(LABEL)
        .unwrap();
    assert_eq!(
        tool.calls(),
        [print_line(LABEL), bootstrap_line(), print_line(LABEL)]
    );
    assert_eq!(pauses.count(), 0);
}

/// A loaded idle service is kicked, and a running one is left alone.
#[test]
fn start_kicks_a_loaded_service_that_is_not_running_and_leaves_a_running_one() {
    let idle = print_with("not running", "\truns = 1\n");
    let tool = Script::default()
        .print_answers(vec![Ok(idle), Ok(print_with("running", "\truns = 2\n"))])
        .ok(&kick_line());
    LaunchUnits::new(&tool).start(LABEL).unwrap();
    assert_eq!(
        tool.calls(),
        [print_line(LABEL), kick_line(), print_line(LABEL)]
    );
    let tool = Script::default().prints(&[&print_with("running", "")]);
    LaunchUnits::new(&tool).start(LABEL).unwrap();
    assert_eq!(tool.calls(), [print_line(LABEL)]);
}

/// A start of a service whose print has no state fails with the parse error before any change.
#[test]
fn a_start_of_a_service_whose_state_cannot_be_read_changes_nothing() {
    let tool = Script::default().prints(&["system/x = {\n}\n"]);
    assert_eq!(
        LaunchUnits::new(&tool).start(LABEL),
        Err(PortError::Parse(
            "launchctl print printed no state".to_owned()
        ))
    );
    assert_eq!(tool.calls(), [print_line(LABEL)]);
}

/// A failing bootstrap or kickstart is the error of the start and is not followed by reads.
#[test]
fn a_bootstrap_or_kickstart_that_fails_is_the_error_of_the_start() {
    let tool = Script::default()
        .print_answers(vec![Err(GONE.to_owned())])
        .on(
            &bootstrap_line(),
            vec![Err("/bin/launchctl: exit status: 5".to_owned())],
        );
    assert_eq!(
        LaunchUnits::new(&tool).start(LABEL),
        Err(PortError::Command(
            "/bin/launchctl: exit status: 5".to_owned()
        ))
    );
    assert_eq!(tool.calls(), [print_line(LABEL), bootstrap_line()]);
    let tool = Script::default()
        .prints(&[&print_with("not running", "")])
        .on(
            &kick_line(),
            vec![Err("/bin/launchctl: exit status: 3".to_owned())],
        );
    assert_eq!(
        LaunchUnits::new(&tool).start(LABEL),
        Err(PortError::Command(
            "/bin/launchctl: exit status: 3".to_owned()
        ))
    );
    assert_eq!(tool.calls(), [print_line(LABEL), kick_line()]);
}

/// `is_active` is true only for a running service; an unreadable, missing or stateless one
/// and a bad label are false.
#[test]
fn is_active_is_true_only_for_a_running_service() {
    for (printed, want) in [
        ("running", true),
        ("not running", false),
        ("spawn scheduled", false),
    ] {
        let tool = Script::default().prints(&[&print_with(printed, "")]);
        assert_eq!(
            LaunchUnits::new(&tool).is_active(LABEL),
            Ok(want),
            "{printed}"
        );
    }
    let tool = Script::default().prints(&["system/x = {\n}\n"]);
    assert_eq!(LaunchUnits::new(&tool).is_active(LABEL), Ok(false));
    let tool =
        Script::default().print_answers(vec![Err("/bin/launchctl: exit status: 1".to_owned())]);
    assert_eq!(LaunchUnits::new(&tool).is_active(LABEL), Ok(false));
    let tool = Script::default().print_answers(vec![Err(GONE.to_owned())]);
    assert_eq!(LaunchUnits::new(&tool).is_active(LABEL), Ok(false));
    let tool = Script::default();
    assert_eq!(LaunchUnits::new(&tool).is_active("a/b"), Ok(false));
    assert!(tool.calls().is_empty());
}

/// A label that begins with a hyphen is never passed as a bare word an option parser could
/// take.
#[test]
fn a_label_that_begins_with_a_hyphen_is_never_an_option() {
    let print = print_line("-x");
    let tool = Script::default()
        .on(
            &print,
            vec![Err(GONE.to_owned()), Ok(print_with("running", ""))],
        )
        .ok(&format!(
            "{LAUNCHCTL} bootstrap system /Library/LaunchDaemons/-x.plist"
        ))
        .ok(&format!("{LAUNCHCTL} bootout system/-x"));
    let units = LaunchUnits::new(&tool);
    units.start("-x").unwrap();
    units.stop_no_block("-x").unwrap();
    for line in tool.calls() {
        assert!(
            line.split(' ').skip(2).all(|word| word != "-x"),
            "a bare `-x` could be read as an option: {line}"
        );
    }
}

/// A service bootstrapped but not yet started is read again, with a half-second pause
/// between reads, until it has begun.
#[test]
fn start_waits_for_a_service_launchd_has_bootstrapped_and_not_yet_started() {
    let idle = print_with("not running", "\truns = 0\n");
    let tool = Script::default()
        .print_answers(vec![
            Err(GONE.to_owned()),
            Ok(idle.clone()),
            Ok(idle.clone()),
            Ok(idle),
            Ok(print_with("running", "\truns = 1\n")),
        ])
        .ok(&bootstrap_line());
    let pauses = Pauses::default();
    LaunchUnits::new(&tool)
        .with_clock(&pauses)
        .start(LABEL)
        .unwrap();
    assert_eq!(*pauses.0.borrow(), vec![Duration::from_millis(500); 3]);
    assert_eq!(tool.print_count(), 5);
}

/// The wait is at most `SETTLE_READS` reads, then the start returns Ok.
#[test]
fn the_wait_after_a_start_is_a_bounded_number_of_reads() {
    let tool = Script::default()
        .print_answers(vec![
            Err(GONE.to_owned()),
            Ok(print_with("not running", "")),
        ])
        .ok(&bootstrap_line());
    let pauses = Pauses::default();
    LaunchUnits::new(&tool)
        .with_clock(&pauses)
        .start(LABEL)
        .unwrap();
    assert_eq!(pauses.count(), SETTLE_READS as usize);
    assert_eq!(tool.print_count(), 1 + SETTLE_READS as usize);
}

/// A program that dies at launch has `runs` up and ends the wait; the service then reads as
/// crashed.
#[test]
fn a_program_that_dies_at_launch_ends_the_wait_and_reads_as_crashed() {
    let idle = print_with("not running", "");
    let died = print_with("not running", "\truns = 1\n\tlast exit code = 1\n");
    let tool = Script::default()
        .print_answers(vec![
            Err(GONE.to_owned()),
            Ok(idle),
            Ok(died.clone()),
            Ok(died),
        ])
        .ok(&bootstrap_line());
    let pauses = Pauses::default();
    let units = LaunchUnits::new(&tool).with_clock(&pauses);
    units.start(LABEL).unwrap();
    assert_eq!(pauses.count(), 1);
    assert!(units.show(LABEL).unwrap().crashed());
}

/// After a kickstart the exit of the run before does not end the wait: only more `runs` or
/// another state does.
#[test]
fn the_runs_of_the_run_before_do_not_end_the_wait_after_a_kickstart() {
    let before = print_with("not running", "\truns = 2\n\tlast exit code = 1\n");
    let running = print_with("running", "\truns = 3\n");
    let tool = Script::default()
        .print_answers(vec![
            Ok(before.clone()),
            Ok(before.clone()),
            Ok(before),
            Ok(running),
        ])
        .ok(&kick_line());
    let pauses = Pauses::default();
    let units = LaunchUnits::new(&tool).with_clock(&pauses);
    units.start(LABEL).unwrap();
    assert_eq!(pauses.count(), 2);
    assert!(units.is_active(LABEL).unwrap());
}

/// A kickstarted service whose `runs` went up while still printing "not running" has begun
/// and ended already, so the wait ends.
#[test]
fn more_runs_than_before_end_the_wait_though_the_state_is_still_not_running() {
    let before = print_with("not running", "\truns = 2\n");
    let after = print_with("not running", "\truns = 3\n");
    let tool = Script::default()
        .print_answers(vec![Ok(before), Ok(after)])
        .ok(&kick_line());
    let pauses = Pauses::default();
    LaunchUnits::new(&tool)
        .with_clock(&pauses)
        .start(LABEL)
        .unwrap();
    assert_eq!(pauses.count(), 0);
}

/// A runs value that is not a number counts as 0.
#[test]
fn a_runs_value_that_is_not_a_number_counts_as_no_runs() {
    let before = print_with("not running", "\truns = lots\n");
    let after = print_with("not running", "\truns = 1\n");
    let tool = Script::default()
        .print_answers(vec![Ok(before), Ok(after)])
        .ok(&kick_line());
    let pauses = Pauses::default();
    LaunchUnits::new(&tool)
        .with_clock(&pauses)
        .start(LABEL)
        .unwrap();
    assert_eq!(pauses.count(), 0);
}

/// A service in any state but "not running" has begun, so the wait ends at once.
#[test]
fn a_service_that_is_spawning_after_the_start_ends_the_wait() {
    let tool = Script::default()
        .print_answers(vec![Err(GONE.to_owned()), Ok(print_with("spawning", ""))])
        .ok(&bootstrap_line());
    let pauses = Pauses::default();
    LaunchUnits::new(&tool)
        .with_clock(&pauses)
        .start(LABEL)
        .unwrap();
    assert_eq!(pauses.count(), 0);
    assert_eq!(tool.print_count(), 2);
}

/// A service that cannot be read after the start (not loaded, a failing print, a print with no
/// state) ends the wait with the start still Ok; `show` says why.
#[test]
fn a_service_that_could_not_be_read_after_the_start_ends_the_wait() {
    for after in [
        Err(GONE.to_owned()),
        Err("/bin/launchctl: exit status: 1".to_owned()),
        Ok("system/x = {\n}\n".to_owned()),
    ] {
        let tool = Script::default()
            .print_answers(vec![Err(GONE.to_owned()), after])
            .ok(&bootstrap_line());
        let pauses = Pauses::default();
        LaunchUnits::new(&tool)
            .with_clock(&pauses)
            .start(LABEL)
            .unwrap();
        assert_eq!(pauses.count(), 0);
        assert_eq!(tool.print_count(), 2);
    }
}

/// The default clock is the machine's; a start that needs no wait never pauses on it.
#[test]
fn the_default_clock_is_used_when_none_is_given() {
    let tool = Script::default()
        .print_answers(vec![Err(GONE.to_owned()), Ok(print_with("running", ""))])
        .ok(&bootstrap_line());
    LaunchUnits::new(&tool).start(LABEL).unwrap();
    assert_eq!(tool.print_count(), 2);
}

/// A kickstart of an idle loaded agent is refused, with the exact message, while the gate file
/// is absent; once it exists the kick goes.
#[test]
fn a_kickstart_of_a_loaded_idle_agent_is_refused_while_the_token_file_is_absent() {
    let tmp = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "launchd-gate");
    let gate = tmp.path().join("token");
    let tool = Script::default()
        .print_answers(vec![
            Ok(print_with("not running", "")),
            Ok(print_with("not running", "")),
            Ok(print_with("running", "")),
        ])
        .ok(&kick_line());
    let units = LaunchUnits::new(&tool).with_gate(&gate);
    assert_eq!(
        units.start(LABEL),
        Err(PortError::Command(format!(
            "not kickstarting {LABEL}: {} is absent, KeepAlive holds the agent back until it \
             exists, and kickstart would run it without",
            gate.display()
        )))
    );
    assert_eq!(tool.calls(), [print_line(LABEL)]);
    std::fs::write(&gate, "token").unwrap();
    units.start(LABEL).unwrap();
    assert_eq!(
        tool.calls()[1..],
        [print_line(LABEL), kick_line(), print_line(LABEL)]
    );
}

/// A bootstrap is not checked against the gate: launchd applies the condition itself.
#[test]
fn a_bootstrap_is_left_to_launchds_own_gate_on_the_token_file() {
    let tmp = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "launchd-gate-bootstrap");
    let gate = tmp.path().join("token");
    let tool = Script::default()
        .print_answers(vec![
            Err(GONE.to_owned()),
            Ok(print_with("not running", "")),
            Ok(print_with("running", "")),
        ])
        .ok(&bootstrap_line());
    let pauses = Pauses::default();
    LaunchUnits::new(&tool)
        .with_clock(&pauses)
        .with_gate(&gate)
        .start(LABEL)
        .unwrap();
    assert!(tool.calls().contains(&bootstrap_line()));
}

/// A running service is not kicked and its gate is not consulted.
#[test]
fn a_running_service_is_left_alone_though_the_token_file_is_absent() {
    let tmp = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "launchd-gate-running");
    let gate = tmp.path().join("token");
    let tool = Script::default().prints(&[&print_with("running", "")]);
    LaunchUnits::new(&tool)
        .with_gate(&gate)
        .start(LABEL)
        .unwrap();
    assert_eq!(tool.calls(), [print_line(LABEL)]);
}

/// `gate()` reports the file given to `with_gate`, and none by default.
#[test]
fn gate_reports_the_file_that_was_given_and_none_by_default() {
    let tool = Script::default();
    assert_eq!(LaunchUnits::new(&tool).gate(), None);
    let path = Path::new("/etc/js-agent/token");
    assert_eq!(LaunchUnits::new(&tool).with_gate(path).gate(), Some(path));
}
