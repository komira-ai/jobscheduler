//! A start that launchd refuses: `start` chose `bootstrap` or `kickstart` from a `print`, the
//! command failed, and the start reads the service again to see what launchd holds now (the
//! module documentation's "When launchd refuses the start"). Each test runs one start over the
//! scripted `launchctl` and names every command line it ran.

use super::{
    GONE, LABEL, Pauses, PortError, Script, TestDir, Units, bootstrap_line, kick_line, print_line,
    print_with,
};
use js_update_hook::LaunchUnits;

/// What `launchctl bootstrap` exits with when the domain already holds the label
/// ("Bootstrap failed: 5: Input/output error").
const HELD: &str = "/bin/launchctl: exit status: 5";

/// A bootstrap refused because launchd already holds the label, which runs: the start succeeds
/// with no second command, and with no kickstart that would kill the running instance.
///
/// Catches a hook that halts with `/bin/launchctl: exit status: 5` when the service it starts
/// was loaded after the `print` that found nothing.
#[test]
fn a_bootstrap_refused_for_a_label_launchd_runs_is_a_start() {
    for state in ["running", "spawning", "xpcproxy"] {
        let tool = Script::default()
            .print_answers(vec![Err(GONE.to_owned()), Ok(print_with(state, ""))])
            .on(&bootstrap_line(), vec![Err(HELD.to_owned())]);
        assert_eq!(LaunchUnits::new(&tool).start(LABEL), Ok(()), "{state}");
        assert_eq!(
            tool.calls(),
            [print_line(LABEL), bootstrap_line(), print_line(LABEL)],
            "{state}"
        );
    }
}

/// A bootstrap refused because launchd holds the label idle: the start kicks it, through the
/// stop runner, with the `runs` the second print showed, and reads until it has begun.
///
/// Catches a loaded idle service left idle (the agent never reads its new flags) and a kick
/// judged begun by the `runs` of the print before the bootstrap.
#[test]
fn a_bootstrap_refused_for_a_label_launchd_holds_idle_kicks_it() {
    let tool = Script::default()
        .print_answers(vec![
            Err(GONE.to_owned()),
            Ok(print_with("not running", "\truns = 3\n")),
            Ok(print_with("not running", "\truns = 3\n")),
            Ok(print_with("running", "\truns = 4\n")),
        ])
        .on(&bootstrap_line(), vec![Err(HELD.to_owned())]);
    let stopper = Script::default().ok(&kick_line());
    let pauses = Pauses::default();
    let units = LaunchUnits::new(&tool)
        .with_stop_runner(&stopper)
        .with_clock(&pauses);
    assert_eq!(units.start(LABEL), Ok(()));
    // One read still showed the service idle with the kick's `runs`, so the wait paused once.
    assert_eq!(pauses.count(), 1);
    assert_eq!(
        tool.calls(),
        [
            print_line(LABEL),
            bootstrap_line(),
            print_line(LABEL),
            print_line(LABEL),
            print_line(LABEL),
        ]
    );
    assert_eq!(stopper.calls(), [kick_line()]);
}

/// The kick after a refused bootstrap is held back by the gate file like any kick: absent, the
/// start fails with the gate's message and nothing is kicked.
///
/// Catches a fallback that bypasses the `KeepAlive` gate and runs the agent with no flags file.
#[test]
fn the_kick_after_a_refused_bootstrap_waits_for_the_gate_file() {
    let tmp = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "launchd-refused-gate");
    let gate = tmp.path().join("flags");
    let tool = Script::default()
        .print_answers(vec![
            Err(GONE.to_owned()),
            Ok(print_with("not running", "")),
        ])
        .on(&bootstrap_line(), vec![Err(HELD.to_owned())]);
    assert_eq!(
        LaunchUnits::new(&tool).with_gate(&gate).start(LABEL),
        Err(PortError::Command(format!(
            "not kickstarting {LABEL}: {} is absent, KeepAlive holds the agent back until it \
             exists, and kickstart would run it without",
            gate.display()
        )))
    );
    assert_eq!(
        tool.calls(),
        [print_line(LABEL), bootstrap_line(), print_line(LABEL)]
    );
}

/// A kickstart refused because the label has left the domain: the start bootstraps the plist
/// and reads until the service has begun.
///
/// Catches a start that fails when the service it found loaded was booted out before the kick.
#[test]
fn a_kickstart_refused_for_a_label_that_left_the_domain_bootstraps_it() {
    let tool = Script::default()
        .print_answers(vec![
            Ok(print_with("not running", "\truns = 1\n")),
            Err(GONE.to_owned()),
            Ok(print_with("running", "\truns = 1\n")),
        ])
        .on(&kick_line(), vec![Err(GONE.to_owned())])
        .ok(&bootstrap_line());
    assert_eq!(LaunchUnits::new(&tool).start(LABEL), Ok(()));
    assert_eq!(
        tool.calls(),
        [
            print_line(LABEL),
            kick_line(),
            print_line(LABEL),
            bootstrap_line(),
            print_line(LABEL),
        ]
    );
}

/// The second command's failure is the start's error, and no third command is made.
///
/// Catches a start that goes back and forth between the two commands, and one that reports the
/// first refusal when the command that decided the outcome was the second.
#[test]
fn when_the_second_command_fails_too_its_error_is_the_starts() {
    let tool = Script::default()
        .print_answers(vec![
            Ok(print_with("not running", "")),
            Err(GONE.to_owned()),
        ])
        .on(&kick_line(), vec![Err(GONE.to_owned())])
        .on(&bootstrap_line(), vec![Err(HELD.to_owned())]);
    assert_eq!(
        LaunchUnits::new(&tool).start(LABEL),
        Err(PortError::Command(HELD.to_owned()))
    );
    assert_eq!(
        tool.calls(),
        [
            print_line(LABEL),
            kick_line(),
            print_line(LABEL),
            bootstrap_line(),
        ]
    );
}

/// A kickstart that exits non-zero while launchd starts the service anyway (it now runs) is a
/// start.
///
/// Catches the exit status of the kick taken as the verdict when the service it asked for runs.
#[test]
fn a_kickstart_that_fails_but_leaves_the_service_running_is_a_start() {
    let tool = Script::default()
        .print_answers(vec![
            Ok(print_with("not running", "")),
            Ok(print_with("running", "")),
        ])
        .on(&kick_line(), vec![Err(HELD.to_owned())]);
    assert_eq!(LaunchUnits::new(&tool).start(LABEL), Ok(()));
    assert_eq!(
        tool.calls(),
        [print_line(LABEL), kick_line(), print_line(LABEL)]
    );
}

/// When the service cannot be read after the refusal (the print fails, or prints no state), the
/// refusal itself is the start's error, text unchanged.
///
/// Catches the error of the read after the refusal reported instead of the refusal, and a
/// second command made on a print that could not be understood.
#[test]
fn a_refusal_followed_by_a_print_that_cannot_be_read_is_the_starts_error() {
    for after in [
        Err("/bin/launchctl: exit status: 1".to_owned()),
        Ok("system/x = {\n}\n".to_owned()),
    ] {
        let tool = Script::default()
            .print_answers(vec![Err(GONE.to_owned()), after.clone()])
            .on(&bootstrap_line(), vec![Err(HELD.to_owned())]);
        assert_eq!(
            LaunchUnits::new(&tool).start(LABEL),
            Err(PortError::Command(HELD.to_owned())),
            "{after:?}"
        );
        assert_eq!(
            tool.calls(),
            [print_line(LABEL), bootstrap_line(), print_line(LABEL)],
            "{after:?}"
        );
    }
}
