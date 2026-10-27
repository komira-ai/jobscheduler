//! The readiness hook: what each way the hook can end reports, what it is given, when it
//! runs, and that the real runner starts it as the module says. The scripted runner
//! starts no process, and no test waits for a clock: the hook's deadline is the runner
//! saying it timed out.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::time::Duration;

use js_agent::lifecycle::Quiet;
use js_agent::{
    EXIT_NOISE_OVER_BOUND, EXIT_NOT_QUIET, EXIT_SETUP_FAILED, EXIT_UNSUPPORTED, HookRunner, Hooked,
    Note, PROBE_OUTPUT_CAP, Quiesce, Ran, ReadinessHook, Session, Verdict, hook_runner,
};
use js_agent_wire::{RefusalReason, limits};

use crate::exclusive::{LEASE, Probe, TIMEOUT, lines, names, note, unsafe_host};
use crate::lifecycle::fake::ManualClock;
use crate::support::{Scratch, hello, lowest, welcome};

/// One call the scripted runner saw: the program, its arguments and its environment.
type Call = (String, Vec<String>, Vec<(String, String)>);

/// A runner that answers from a list and records every call.
struct Scripted {
    answers: RefCell<VecDeque<Ran>>,
    calls: RefCell<Vec<Call>>,
}

impl Scripted {
    fn new(answers: Vec<Ran>) -> Self {
        Self {
            answers: RefCell::new(answers.into()),
            calls: RefCell::new(Vec::new()),
        }
    }
}

impl HookRunner for Scripted {
    fn execute(&self, program: &str, args: &[&str], env: &[(&str, &str)]) -> Ran {
        let own = |text: &str| text.to_owned();
        self.calls.borrow_mut().push((
            own(program),
            args.iter().map(|arg| own(arg)).collect(),
            env.iter().map(|(k, v)| (own(k), own(v))).collect(),
        ));
        self.answers
            .borrow_mut()
            .pop_front()
            .expect("the hook was run more often than the test scripted")
    }
}

fn exited(code: i32, stdout: &str) -> Ran {
    Ran::Exited {
        code: Some(code),
        text: format!("exit status: {code}"),
        stdout: stdout.to_owned(),
    }
}

fn hook(args: &[&str]) -> ReadinessHook {
    let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
    ReadinessHook::new("/bin/sh", &args).unwrap()
}

fn refused(code: RefusalReason, detail: &str) -> Verdict {
    Verdict::Refused {
        code,
        detail: detail.to_owned(),
    }
}

/// Asserts the exit-code table, one row per code, through a whole session: a host the
/// agent found quiet, a hook that ends with the code, and the heartbeat after it carries
/// exactly the typed refusal (with the hook's first line as the detail), the note says so,
/// and the refusal is resent without running the hook again. The unmapped codes, and a
/// hook ended by a signal, are `setup-failed`. It catches a mapping that is off by one
/// row, and a code that is lost on its way to the heartbeat.
#[test]
fn each_exit_code_is_its_typed_refusal() {
    let rows: [(Option<i32>, RefusalReason); 9] = [
        (Some(EXIT_NOISE_OVER_BOUND), RefusalReason::NoiseOverBound),
        (Some(EXIT_NOT_QUIET), RefusalReason::NotQuiet),
        (Some(EXIT_SETUP_FAILED), RefusalReason::SetupFailed),
        (Some(EXIT_UNSUPPORTED), RefusalReason::Unsupported),
        (Some(1), RefusalReason::SetupFailed),
        (Some(74), RefusalReason::SetupFailed),
        (Some(79), RefusalReason::SetupFailed),
        (Some(255), RefusalReason::SetupFailed),
        (None, RefusalReason::SetupFailed),
    ];
    for (code, expected) in rows {
        let ran = Ran::Exited {
            code,
            text: "ended".to_owned(),
            stdout: "why it said so\nnot this line\n".to_owned(),
        };
        let runner = Scripted::new(vec![ran]);
        let hook = hook(&[]);
        let (mut script, sent) = crate::support::Script::new(vec![
            welcome(1),
            names(LEASE),
            names(LEASE),
            names(LEASE),
            names(LEASE),
        ]);
        let (mut probe, _) = Probe::new(vec![Quiet::Ready], Vec::new());
        let clock = ManualClock::default();
        let mut host = Hooked::new(&mut probe, &hook, &runner);
        let mut session = Session::new(hello("h1"), &mut script, lowest())
            .with_quiesce(&mut host, &clock, TIMEOUT);
        let steps: Vec<_> = (0..5).map(|_| session.tick()).collect();
        let word = String::from(expected);
        let said = format!("preparing refused={word}:why it said so");
        assert_eq!(lines(&sent)[2..], [said.clone(), said], "{code:?}");
        assert_eq!(note(&steps[3]), Some(Note::Refused { code: expected }));
        assert_eq!(runner.calls.borrow().len(), 1, "{code:?}");
    }
}

/// Asserts that exit 0 reports ready, that the hook is given exactly its program, its
/// arguments in order and an environment of a fixed `PATH` and nothing else (so nothing of
/// the agent's own environment, and no lease id or secret, reaches it), that ready is
/// resent without running the hook again, and that a new grant runs it again. It catches a
/// hook that inherits the agent's environment or is run on every beat.
#[test]
fn exit_zero_reports_ready_and_the_hook_gets_nothing_of_the_scheduler() {
    let runner = Scripted::new(vec![exited(0, "ignored\n"), exited(0, "")]);
    let hook = hook(&["-c", "--flag"]);
    let (mut script, sent) = crate::support::Script::new(vec![
        welcome(1),
        names(LEASE),
        names(LEASE),
        names(LEASE),
        names(LEASE),
        names("whole-2"),
        names("whole-2"),
        names("whole-2"),
        names("whole-2"),
    ]);
    let (mut probe, _) = Probe::new(vec![Quiet::Ready, Quiet::Ready], Vec::new());
    let clock = ManualClock::default();
    let mut host = Hooked::new(&mut probe, &hook, &runner);
    let mut session =
        Session::new(hello("h1"), &mut script, lowest()).with_quiesce(&mut host, &clock, TIMEOUT);
    (0..9).for_each(|_| {
        session.tick();
    });
    let beats = lines(&sent);
    assert_eq!(beats[2], format!("serving ready={LEASE}"));
    assert_eq!(beats[3], format!("serving ready={LEASE}"));
    assert_eq!(beats[beats.len() - 1], "serving ready=whole-2", "{beats:?}");
    let calls = runner.calls.borrow();
    assert_eq!(calls.len(), 2);
    let expected: Call = (
        "/bin/sh".to_owned(),
        vec!["-c".to_owned(), "--flag".to_owned()],
        vec![(
            "PATH".to_owned(),
            "/usr/bin:/bin:/usr/sbin:/sbin".to_owned(),
        )],
    );
    assert_eq!(calls[0], expected);
    assert!(!format!("{calls:?}").contains(LEASE));
}

/// Asserts that a hook that did not finish is `not-quiet`, one that printed too much is
/// `setup-failed`, and one that could not be started is `unsupported`, each with a fixed
/// text when the hook said nothing, and that a hook that ran for a host the checks found
/// quiet is the only way to these. It catches a deadline that reports the wrong reason.
#[test]
fn a_timeout_an_overflow_and_a_spawn_failure_are_typed() {
    let timed = hook(&[]).run(&Scripted::new(vec![Ran::TimedOut]));
    assert_eq!(
        timed,
        refused(
            RefusalReason::NotQuiet,
            "the readiness hook did not finish before prepare_timeout"
        )
    );
    let over = hook(&[]).run(&Scripted::new(vec![Ran::Overflow]));
    assert_eq!(
        over,
        refused(
            RefusalReason::SetupFailed,
            &format!("the readiness hook printed more than {PROBE_OUTPUT_CAP} bytes")
        )
    );
    let spawn = hook(&[]).run(&Scripted::new(vec![Ran::Spawn("no such file".to_owned())]));
    assert_eq!(
        spawn,
        refused(
            RefusalReason::Unsupported,
            "the readiness hook could not be started: no such file"
        )
    );
    let silent = hook(&[]).run(&Scripted::new(vec![exited(EXIT_NOT_QUIET, "\n")]));
    assert_eq!(
        silent,
        refused(
            RefusalReason::NotQuiet,
            "the readiness hook ended: exit status: 76"
        )
    );
}

/// Asserts the detail is the first line only, cut to the wire's bound at a character
/// boundary: ASCII over the bound is cut to exactly it, two-byte characters cut on a
/// boundary, three-byte characters cut below the bound when the bound falls inside one,
/// and a short line is kept. The cut text is always a valid string within the bound. It
/// catches a slice in the middle of a character (a panic) and a bound off by one.
#[test]
fn the_detail_is_the_first_line_cut_to_a_valid_bound() {
    let bound = limits::MAX_REFUSAL_DETAIL;
    let cases = [
        ("a".repeat(300), "a".repeat(bound)),
        ("é".repeat(200), "é".repeat(bound / 2)),
        (
            format!("aa{}", "€".repeat(100)),
            format!("aa{}", "€".repeat(84)),
        ),
        ("short".to_owned(), "short".to_owned()),
        ("a".repeat(bound), "a".repeat(bound)),
    ];
    for (line, cut) in cases {
        let ran = exited(EXIT_NOISE_OVER_BOUND, &format!("{line}\nsecond line\n"));
        let verdict = hook(&[]).run(&Scripted::new(vec![ran]));
        assert_eq!(verdict, refused(RefusalReason::NoiseOverBound, &cut));
        assert!(cut.len() <= bound);
    }
}

/// Asserts that the hook is not run when the agent's own checks did not find the host
/// quiet and clean: a host still busy, and one with a finding, never start it, and the
/// finding is still `host-unsafe`. It catches a hook run before the checks, which would
/// measure a host that is not yet quiet and could mask the typed finding.
#[test]
fn the_hook_runs_only_after_the_agents_own_checks() {
    let runner = Scripted::new(Vec::new());
    let hook = hook(&[]);
    let mut probe = Probe::new(vec![Quiet::Busy, unsafe_host("found")], Vec::new()).0;
    let mut host = Hooked::new(&mut probe, &hook, &runner);
    assert_eq!(host.prepare(), Quiet::Busy);
    assert_eq!(host.prepare(), unsafe_host("found"));
    assert!(runner.calls.borrow().is_empty());
}

/// Asserts that restoring is the host's own and does not touch the hook.
#[test]
fn restoring_is_the_hosts_own() {
    let runner = Scripted::new(Vec::new());
    let hook = hook(&[]);
    let mut probe = Probe::new(Vec::new(), vec![js_agent::lifecycle::Restored::Done]).0;
    let mut host = Hooked::new(&mut probe, &hook, &runner);
    assert_eq!(host.restore(), js_agent::lifecycle::Restored::Done);
    assert!(runner.calls.borrow().is_empty());
}

/// Asserts the real runner: a program that exits 75 with a line is `noise-over-bound` with
/// that line; the program sees `PATH` as the module fixes it and none of the agent's own
/// variables; a program killed by a signal is `setup-failed`; one that prints more than the
/// cap is `setup-failed`; one that cannot be started (an interpreter that does not exist)
/// is `unsupported`; and exit 0 is ready. It catches a runner that loses the exit status,
/// passes the agent's environment on, or reports a spawn failure as a refusal of the host.
#[test]
fn the_real_runner_starts_the_hook_as_documented() {
    let runner = hook_runner(Duration::from_secs(3600));
    let env = hook(&["-c", "echo \"p=$PATH h=${HOME:-unset}\"; exit 75"]);
    assert_eq!(
        env.run(&runner),
        refused(
            RefusalReason::NoiseOverBound,
            "p=/usr/bin:/bin:/usr/sbin:/sbin h=unset"
        )
    );
    let killed = hook(&["-c", "kill -9 $$"]).run(&runner);
    let Verdict::Refused { code, detail } = killed else {
        panic!("a killed hook is not ready");
    };
    assert_eq!(code, RefusalReason::SetupFailed);
    assert!(detail.starts_with("the readiness hook ended: "), "{detail}");
    let loud = hook(&["-c", "head -c 100000 /dev/zero"]).run(&runner);
    assert_eq!(
        loud,
        refused(
            RefusalReason::SetupFailed,
            &format!("the readiness hook printed more than {PROBE_OUTPUT_CAP} bytes")
        )
    );
    assert_eq!(hook(&["-c", "exit 0"]).run(&runner), Verdict::Ready);

    let dir = Scratch::new("readiness-spawn");
    let broken = dir.write("hook", "#!/nonexistent/interpreter\n", 0o755);
    let broken = ReadinessHook::new(broken.to_str().unwrap(), &[]).unwrap();
    let Verdict::Refused { code, .. } = broken.run(&runner) else {
        panic!("a hook that cannot start is not ready");
    };
    assert_eq!(code, RefusalReason::Unsupported);
}
