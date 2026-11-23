//! The development verb `js-agent __native-proof` through its command line: its flags, what it
//! prints and its exit status, over a pool of a fake host and the fake launcher given through
//! its opener seam (`proof::run_with`). The production opener is the real supervisor and the
//! Mac's pool, which the Mac proof runs.

use std::ffi::OsString;
use std::fs;
use std::process::ExitCode;

use js_agent::native::proof::{self, VERB, status};
use js_agent::native::{NativeConfig, NativeRuntime};

use super::{FakeAccounts, FakeLauncher, Scratch, TERM, open_pool, pool_settings};

/// A directory with the pool's layout, and the opener that makes the runtime over it: the
/// pool on a fake host and the fake launcher, with the configuration the verb's flags gave
/// (its homes made the pool's).
struct Proof {
    dir: Scratch,
}

impl Proof {
    fn new(name: &str) -> Self {
        let dir = Scratch::new(&format!("native-proof-{name}"));
        fs::create_dir_all(dir.path("state")).unwrap();
        Self { dir }
    }

    /// Runs the verb with `words` after the verb, over the pool; the exit status, and what it
    /// printed on its output and its error stream.
    fn run(&self, words: &[&str]) -> (ExitCode, String, String) {
        let settings = pool_settings(&self.dir, 1);
        let host = FakeAccounts::new();
        let pool = open_pool(&self.dir, &settings, &host);
        let launcher = FakeLauncher::new();
        let open = move |mut config: NativeConfig| -> Result<NativeRuntime, String> {
            config.homes = settings.homes.clone();
            NativeRuntime::over(Some(pool.clone()), launcher.clone(), config, TERM)
                .map(|rt| rt.with_hand_over(owner_writable()))
                .map_err(|why| why.0)
        };
        self.run_over(words, &open)
    }

    /// As [`Proof::run`], with `open` as the opener.
    fn run_over(&self, words: &[&str], open: proof::Opener<'_>) -> (ExitCode, String, String) {
        let args: Vec<OsString> = ["js-agent", VERB]
            .iter()
            .chain(words)
            .map(OsString::from)
            .collect();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = proof::run_with(&args, &mut out, &mut err, open);
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    /// The pool flags of a run in this directory, then `rest`.
    fn flags(&self, rest: &[&str]) -> Vec<String> {
        let mut words = vec![
            "--accounts".to_owned(),
            self.dir.path("accounts").display().to_string(),
            "--range".to_owned(),
            "600-609".to_owned(),
            "--state".to_owned(),
            self.dir.path("state").display().to_string(),
        ];
        words.extend(rest.iter().map(|word| (*word).to_owned()));
        words
    }
}

/// The agent's hand-over (`lchown`), leaving a staged directory writable by its owner, so the
/// test's user can retire an account whose run staged an input (only root can empty a `0500`
/// directory).
fn owner_writable() -> js_agent::native::HandOver {
    std::sync::Arc::new(|path: &std::path::Path, uid: u32, gid: u32| {
        std::os::unix::fs::lchown(path, Some(uid), Some(gid))?;
        match fs::symlink_metadata(path)?.is_dir() {
            true => {
                use std::os::unix::fs::PermissionsExt as _;
                fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            }
            false => Ok(()),
        }
    })
}

fn words(owned: &[String]) -> Vec<&str> {
    owned.iter().map(String::as_str).collect()
}

/// Asserts that a run of `/usr/bin/true` exits 0 and prints the eight steps in order, the
/// job's end, the removal and the retirement's proof of the account it was lent.
///
/// Catches a verb that does not run the start's steps, does not remove the run, or does not
/// show the proof the Mac proof reads.
#[test]
fn a_run_that_ends_well_exits_0_and_prints_its_steps_and_the_proof() {
    let proof = Proof::new("ok");
    let flags = proof.flags(&["--", "/usr/bin/true"]);
    let (code, out, err) = proof.run(&words(&flags));
    assert_eq!(code, ExitCode::from(status::OK), "{out}{err}");
    let steps: Vec<&str> = out
        .lines()
        .filter_map(|line| line.strip_prefix("step: "))
        .map(|line| line.split(" at ").next().unwrap())
        .collect();
    assert_eq!(steps, js_agent::native::STEPS);
    assert!(
        out.contains("ended: Exited(Exit { code: 0, oom_killed: false })\n"),
        "{out}"
    );
    assert!(out.contains("remove: ok\n"), "{out}");
    assert!(out.contains("retired: kbf-acct-"), "{out}");
    assert!(
        out.contains("retired.gone: Proof { record: true, processes: true, home: true, folders: true, places: true, ipc: true }"),
        "{out}"
    );
}

/// Asserts that a job that fails makes the verb exit 1 with the job's code printed, and that
/// an input staged with `--input` and an output named with `--output` reach the job and the
/// run's collected outputs.
///
/// Catches a verb that reports success for a failed job, or that drops its inputs or outputs.
#[test]
fn a_failed_job_exits_1_and_inputs_and_outputs_travel() {
    let proof = Proof::new("io");
    let input = proof.dir.path("input");
    fs::create_dir_all(&input).unwrap();
    fs::write(input.join("f"), "staged\n").unwrap();
    let given = format!("{}=in", input.display());
    let flags = proof.flags(&[
        "--input",
        &given,
        "--output",
        "out.txt",
        "--",
        "/bin/sh",
        "-c",
        "cat in/f > out.txt",
    ]);
    let (code, out, err) = proof.run(&words(&flags));
    assert_eq!(code, ExitCode::from(status::OK), "{out}{err}");
    let collected = proof.dir.path("state/proof-run/outputs/out.txt");
    assert_eq!(fs::read_to_string(collected).unwrap(), "staged\n");

    let flags = proof.flags(&["--", "/usr/bin/false"]);
    let (code, out, _) = proof.run(&words(&flags));
    assert_eq!(code, ExitCode::from(status::FAILED), "{out}");
    assert!(
        out.contains("ended: Exited(Exit { code: 1, oom_killed: false })\n"),
        "{out}"
    );
    assert!(out.contains("remove: ok\n"), "{out}");
}

/// Asserts that `--stop-after` stops the job with the given grace: a job that ends on
/// `SIGTERM` ends 128 + 15, and the verb exits 1.
///
/// Catches a stop option that never stops, which the sweep's proof relies on.
#[test]
fn stop_after_stops_the_job_with_its_grace() {
    let proof = Proof::new("stop");
    // A long grace: the job ends on the SIGTERM, never on the kill after the grace.
    let flags = proof.flags(&[
        "--stop-after",
        "1",
        "--grace",
        "600",
        "--",
        "/bin/sleep",
        "600",
    ]);
    let (code, out, _) = proof.run(&words(&flags));
    assert_eq!(code, ExitCode::from(status::FAILED), "{out}");
    assert!(out.contains("stop: grace 600 s\n"), "{out}");
    assert!(
        out.contains("ended: Exited(Exit { code: 143, oom_killed: false })\n"),
        "{out}"
    );
}

/// Asserts that a runtime that cannot be opened, or that has no pool, makes the verb exit 2
/// with the reason, and starts no job.
///
/// Catches a verb that reports an open or a start it never made as a job's failure.
#[test]
fn a_runtime_that_cannot_open_or_start_exits_2() {
    let proof = Proof::new("refused");
    let flags = proof.flags(&["--", "/usr/bin/true"]);
    let refuse =
        |_: NativeConfig| -> Result<NativeRuntime, String> { Err("open: no pool here".to_owned()) };
    let (code, out, _) = proof.run_over(&words(&flags), &refuse);
    assert_eq!(code, ExitCode::from(status::REFUSED));
    assert!(out.contains("open: no pool here\n"), "{out}");

    let launcher = FakeLauncher::new();
    let launched = launcher.clone();
    let no_pool = move |config: NativeConfig| -> Result<NativeRuntime, String> {
        NativeRuntime::over(None, launcher.clone(), config, TERM).map_err(|why| why.0)
    };
    let (code, out, _) = proof.run_over(&words(&flags), &no_pool);
    assert_eq!(code, ExitCode::from(status::REFUSED));
    assert!(
        out.contains(
            "start: the account pool is not open: the agent does not hold Full Disk Access\n"
        ),
        "{out}"
    );
    assert!(launched.launched().is_empty());
}

/// Asserts the command line's refusals, each exit 64 with the verb's name and nothing run: a
/// missing program, an unknown flag, a flag without its value, a number that is not one, no
/// `--state`, no `--accounts`, and a slot range the pool refuses (named by its agent flag).
///
/// Catches a verb that runs with a command line it did not understand.
#[test]
fn a_wrong_command_line_exits_64_and_says_why() {
    let proof = Proof::new("usage");
    let cases: Vec<(Vec<String>, String)> = vec![
        (proof.flags(&[]), "no program after --".to_owned()),
        (
            vec!["--bogus".to_owned()],
            "--bogus: not a flag of __native-proof".to_owned(),
        ),
        (
            vec!["--range".to_owned()],
            "--range needs a value".to_owned(),
        ),
        (
            vec!["--ready".to_owned(), "x".to_owned()],
            "--ready x: not a number".to_owned(),
        ),
        (
            vec!["--".to_owned(), "/usr/bin/true".to_owned()],
            "--state is required".to_owned(),
        ),
        (
            vec![
                "--state".to_owned(),
                proof.dir.path("state").display().to_string(),
                "--".to_owned(),
                "/usr/bin/true".to_owned(),
            ],
            "--accounts is required".to_owned(),
        ),
    ];
    for (given, said) in cases {
        let (code, out, err) = proof.run(&words(&given));
        assert_eq!(code, ExitCode::from(status::USAGE), "{given:?}");
        assert_eq!(err, format!("js-agent: {VERB}: {said}\n"), "{given:?}");
        assert!(out.is_empty(), "{given:?}: {out}");
    }
    let mut bad = proof.flags(&["--", "/usr/bin/true"]);
    bad[3] = "100-200".to_owned();
    let (code, _, err) = proof.run(&words(&bad));
    assert_eq!(code, ExitCode::from(status::USAGE));
    assert!(
        err.starts_with(&format!("js-agent: {VERB}: --native-account-range: ")),
        "{err}"
    );
}
