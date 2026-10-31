//! `cargo xtask crun-tests` as a workflow would run it: the real binary, with `xtask-fake` as cargo
//! (printing the JSON messages each test writes for it) and as sudo (recording the
//! `systemd-run` it was asked to run).

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use xtask::crun_tests::{
    BUSYBOX_AMD64, BUSYBOX_ARM64, ROOT_TARGETS, TARGETS, arch_for, reset_args, show_args, unit_name,
};

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");
const FAKE: &str = env!("CARGO_BIN_EXE_xtask-fake");
const PATH: &str = "/usr/bin:/bin";
const HOME: &str = "/srv/runner";

/// This machine's pinned busybox manifest.
fn this_manifest() -> &'static str {
    let machine = rustix::system::uname()
        .machine()
        .to_string_lossy()
        .into_owned();
    match arch_for(&machine).expect("a machine these tests run on") {
        "amd64" => BUSYBOX_AMD64,
        _ => BUSYBOX_ARM64,
    }
}

/// One run's directories: the repository root cargo runs in, and the fakes' directory.
struct Run {
    root: PathBuf,
    bin: PathBuf,
    /// The process id of the last `xtask` run: its units are named after it.
    pid: Cell<u32>,
}

impl Run {
    /// An empty root for one test, a fake sudo, and the messages the fake cargo prints:
    /// each target's test binary (`<root>/target/debug/deps/<target>-0f1e`, from the
    /// package at `<root>/crates/js-contain`), after a decoy.
    fn new(name: &str) -> Run {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("xpt")
            .join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).expect("scratch");
        std::fs::hard_link(FAKE, bin.join("sudo")).expect("link the fake");
        let package = root.join("crates/js-contain");
        std::fs::create_dir_all(&package).expect("package directory");
        let mut messages = String::from(
            r#"{"reason":"compiler-artifact","target":{"kind":["lib"],"name":"js_contain"},"profile":{"test":false},"executable":null,"manifest_path":"/w/Cargo.toml"}"#,
        );
        messages.push('\n');
        for target in TARGETS.iter().chain(ROOT_TARGETS) {
            messages.push_str(&format!(
                r#"{{"reason":"compiler-artifact","target":{{"kind":["test"],"name":"{target}"}},"profile":{{"test":true}},"executable":"{}","manifest_path":"{}"}}"#,
                root.join(format!("target/debug/deps/{target}-0f1e")).display(),
                package.join("Cargo.toml").display()
            ));
            messages.push('\n');
        }
        std::fs::write(root.join("cargo-messages"), messages).expect("messages");
        Run {
            root,
            bin,
            pid: Cell::new(0),
        }
    }

    /// `crun-tests` with the fakes, the cargo `cargo`, and the sudo `program` (`None`:
    /// the fake), failing as `defect` says (or not): exit code, stdout and stderr.
    fn xtask(
        &self,
        cargo: &Path,
        program: Option<&Path>,
        defect: Option<&str>,
    ) -> (Option<i32>, String, String) {
        self.xtask_with(&[], cargo, program, defect)
    }

    /// [`Run::xtask`] with more flags on the command line.
    fn xtask_with(
        &self,
        flags: &[&str],
        cargo: &Path,
        program: Option<&Path>,
        defect: Option<&str>,
    ) -> (Option<i32>, String, String) {
        let fake = self.bin.join("sudo");
        let mut cmd = Command::new(XTASK);
        cmd.arg("crun-tests").arg("--root").arg(&self.root);
        cmd.args(flags);
        cmd.arg("--sudo").arg(program.unwrap_or(fake.as_path()));
        cmd.env("CARGO", cargo).env("HOME", HOME).env("PATH", PATH);
        match defect {
            Some(d) => cmd.env("XTASK_FAKE_DEFECT", d),
            None => cmd.env_remove("XTASK_FAKE_DEFECT"),
        };
        let child = cmd
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("run xtask");
        self.pid.set(child.id());
        let out = child.wait_with_output().expect("wait for xtask");
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    fn log(&self, path: PathBuf) -> String {
        std::fs::read_to_string(path).unwrap_or_default()
    }

    /// What the fake cargo was asked to do, one line per run; empty when it never ran.
    fn cargo_args(&self) -> String {
        self.log(self.root.join("target/debug/cargo-args"))
    }

    /// What the fake sudo was asked to run, one `<dir>: <args>` line per run.
    fn sudo_args(&self) -> String {
        self.log(self.bin.join("sudo-args"))
    }

    /// The `sudo-args` lines of the unit that runs `target`'s binary: the `systemd-run`, the
    /// read of the unit's result and the reset of the unit.
    fn unit(&self, target: &str) -> String {
        let uid = rustix::process::getuid().as_raw();
        let gid = rustix::process::getgid().as_raw();
        let package = self.root.join("crates/js-contain");
        let name = unit_name(target, self.pid.get());
        format!(
            "{cwd}: -n systemd-run --quiet --wait --pipe \
             --unit={name} --slice=js-agent.slice -p Delegate=yes \
             -p OOMScoreAdjust=-900 --uid={uid} --gid={gid} --working-directory={package} \
             -E HOME={HOME} -E PATH={PATH} \
             -E JS_TEST_IMAGE=docker://docker.io/library/busybox@{manifest} {exe} \
             --include-ignored\n{after}",
            cwd = std::env::current_dir().expect("cwd").display(),
            package = package.display(),
            manifest = this_manifest(),
            exe = self
                .root
                .join(format!("target/debug/deps/{target}-0f1e"))
                .display(),
            after = self.after(&name),
        )
    }

    /// The `sudo-args` lines after a unit's `systemd-run`: the read of its result, then its reset.
    fn after(&self, name: &str) -> String {
        let cwd = std::env::current_dir().expect("cwd");
        let line = |args: Vec<std::ffi::OsString>| {
            let words: Vec<String> = args
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            format!("{}: -n {}\n", cwd.display(), words.join(" "))
        };
        line(show_args(name)) + &line(reset_args(name))
    }

    /// Every unit's lines, in the order the targets run.
    fn units(&self) -> String {
        TARGETS.iter().map(|t| self.unit(t)).collect()
    }
}

/// The build the verb asks cargo for: every real-`crun` target, as JSON, as this user.
fn build() -> String {
    let tests: String = TARGETS.iter().map(|t| format!(" --test {t}")).collect();
    format!("test --locked --no-run --message-format=json -p js-contain{tests}\n")
}

/// Catches any step dropped or reordered: the targets not built (or built without
/// --locked, or not all of them) before any unit starts, the machine's pinned busybox
/// manifest not named, and a target's unit missing, run with the wrong binary, or with the
/// wrong settings (see the unit tests of `systemd_run_args` for each one).
#[test]
fn every_target_runs_in_its_own_delegated_unit() {
    let run = Run::new("ok");
    let (code, out, err) = run.xtask(Path::new(FAKE), None, None);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(run.cargo_args(), build());
    assert_eq!(run.sudo_args(), run.units());
    assert!(out.starts_with("kernel "), "{out}");
    let machine = rustix::system::uname()
        .machine()
        .to_string_lossy()
        .into_owned();
    let arch = arch_for(&machine).expect("arch");
    assert!(
        out.contains(&format!(
            "\nbusybox manifest for {arch}: {}\n",
            this_manifest()
        )),
        "{out}"
    );
    assert!(
        out.ends_with(
            "ok: the real-crun tests of -p js-contain passed, each target in its own \
             Delegate=yes unit\n"
        ),
        "{out}"
    );
}

/// Catches a failed target that is passed over (the status of a unit not taken for the
/// run's) or that stops the run: the unit fails, the verb still ran it and fails naming
/// the target and its binary.
#[test]
fn a_failed_target_fails_the_verb_after_it_ran() {
    let run = Run::new("fail");
    let (code, _, err) = run.xtask(Path::new(FAKE), None, Some("fails:--include-ignored"));
    assert_eq!(code, Some(1), "{err}");
    assert_eq!(run.sudo_args(), run.units());
    let failed = |target: &str| {
        format!(
            "the tests of -p js-contain --test {target} failed in their unit ({}): \
             systemd-run: exit status: 101\n",
            run.root
                .join(format!("target/debug/deps/{target}-0f1e"))
                .display()
        )
    };
    let want: String = TARGETS.iter().map(|t| failed(t)).collect();
    assert!(err.ends_with(&format!("xtask: {want}")), "{err}");
}

/// Catches a failed build run anyway: no unit starts when cargo failed.
#[test]
fn a_failed_build_starts_no_unit() {
    let run = Run::new("build");
    let (code, _, err) = run.xtask(Path::new(FAKE), None, Some("build-fails"));
    assert_eq!(code, Some(1));
    assert_eq!(run.cargo_args(), build());
    assert_eq!(run.sudo_args(), "");
    let tests: String = TARGETS.iter().map(|t| format!(" --test {t}")).collect();
    assert!(
        err.ends_with(&format!(
            "xtask: cargo test --no-run -p js-contain{tests} failed: exit status: 101\n"
        )),
        "{err}"
    );
}

/// Catches a build whose messages lack the target (a target renamed in the package but
/// not here) run without it: no unit starts.
#[test]
fn a_target_missing_from_the_build_starts_no_unit() {
    let run = Run::new("notgt");
    let messages = std::fs::read_to_string(run.root.join("cargo-messages")).expect("messages");
    let without: String = messages
        .lines()
        .filter(|l| !l.contains(r#""name":"crun""#))
        .map(|l| format!("{l}\n"))
        .collect();
    std::fs::write(run.root.join("cargo-messages"), without).expect("messages");
    let (code, _, err) = run.xtask(Path::new(FAKE), None, None);
    assert_eq!(code, Some(1));
    assert_eq!(run.sudo_args(), "");
    assert!(
        err.ends_with("xtask: cargo built 0 test binaries for the target `crun`, not 1\n"),
        "{err}"
    );
}

/// Catches a sudo that cannot be started reported as anything but that program not
/// starting.
#[test]
fn a_sudo_that_cannot_start_fails_the_verb() {
    let run = Run::new("noprog");
    let missing = run.bin.join("missing");
    let (code, _, err) = run.xtask(Path::new(FAKE), Some(&missing), None);
    assert_eq!(code, Some(1));
    assert!(
        err.contains("in their unit") && err.contains(&format!("{}: ", missing.display())),
        "{err}"
    );
}

/// Catches `--job-user` running anything but the root daemon's targets, or running them as
/// the invoking user: cargo builds only [`ROOT_TARGETS`], each one's unit has no `--uid` or
/// `--gid` (so it runs as root) and names the job user and the `PATH` in its environment,
/// and the verb says whom the containers ran as.
#[test]
fn a_job_user_runs_the_root_daemon_targets_as_root() {
    let run = Run::new("jobuser");
    let (code, out, err) = run.xtask_with(&["--job-user", "js-job"], Path::new(FAKE), None, None);
    assert_eq!(code, Some(0), "{err}");
    let tests: String = ROOT_TARGETS
        .iter()
        .map(|t| format!(" --test {t}"))
        .collect();
    assert_eq!(
        run.cargo_args(),
        format!("test --locked --no-run --message-format=json -p js-contain{tests}\n")
    );
    let package = run.root.join("crates/js-contain");
    let want: String = ROOT_TARGETS
        .iter()
        .map(|target| {
            format!(
                "{cwd}: -n systemd-run --quiet --wait --pipe \
                 --unit={name} --slice=js-agent.slice -p Delegate=yes \
                 -p OOMScoreAdjust=-900 --working-directory={package} -E PATH={PATH} \
                 -E JS_JOB_USER=js-job {exe} --include-ignored\n{after}",
                name = unit_name(target, run.pid.get()),
                after = run.after(&unit_name(target, run.pid.get())),
                cwd = std::env::current_dir().expect("cwd").display(),
                package = package.display(),
                exe = run
                    .root
                    .join(format!("target/debug/deps/{target}-0f1e"))
                    .display(),
            )
        })
        .collect();
    assert_eq!(run.sudo_args(), want);
    assert!(
        out.ends_with(
            "ok: the real-crun tests of -p js-contain passed, each target in its own \
             Delegate=yes unit, as root with job user js-job\n"
        ),
        "{out}"
    );
}

/// Catches a failed root target that is passed over: the unit fails, the verb still ran it
/// and fails naming the target and its binary.
#[test]
fn a_failed_root_target_fails_the_verb_after_it_ran() {
    let run = Run::new("jobuserfail");
    let (code, _, err) = run.xtask_with(
        &["--job-user", "js-job"],
        Path::new(FAKE),
        None,
        Some("fails:--include-ignored"),
    );
    assert_eq!(code, Some(1), "{err}");
    let want: String = ROOT_TARGETS
        .iter()
        .map(|target| {
            format!(
                "the tests of -p js-contain --test {target} failed in their unit ({}): \
                 systemd-run: exit status: 101\n",
                run.root
                    .join(format!("target/debug/deps/{target}-0f1e"))
                    .display()
            )
        })
        .collect();
    assert!(err.ends_with(&format!("xtask: {want}")), "{err}");
}

/// The bug itself, through the real binary: `systemd-run` exits 0 while the unit's test
/// binary exited 101. Catches a verb that takes `systemd-run`'s exit status for the unit's
/// result: it must fail, naming the target and what the unit did, for the rootless and the
/// root targets, and must still reset the unit.
#[test]
fn a_unit_that_failed_under_a_systemd_run_that_exited_0_fails_the_verb() {
    for flags in [&[][..], &["--job-user", "js-job"][..]] {
        let run = Run::new("hole");
        let (code, _, err) = run.xtask_with(flags, Path::new(FAKE), None, Some("unit-fails"));
        assert_eq!(code, Some(1), "{err}");
        let sudo = run.sudo_args();
        assert!(
            sudo.contains("systemctl reset-failed js-crun-tests-"),
            "{sudo}"
        );
        let targets = if flags.is_empty() {
            TARGETS
        } else {
            ROOT_TARGETS
        };
        for target in targets {
            assert!(
                err.contains(&format!(
                    "the tests of -p js-contain --test {target} failed in their unit"
                )),
                "{err}"
            );
        }
        assert!(
            err.contains("Result=exit-code, ExecMainCode=1, ExecMainStatus=101"),
            "{err}"
        );
    }
}

/// Catches a result that cannot be read (the command fails, or prints what is not
/// systemd's properties) counted as a pass.
#[test]
fn a_unit_result_that_cannot_be_read_fails_the_verb() {
    for (defect, reason) in [
        (
            "show-fails",
            "could not read the result of unit js-crun-tests-crun-",
        ),
        ("show-garbled", "that is not Key=Value"),
    ] {
        let run = Run::new(defect);
        let (code, _, err) = run.xtask(Path::new(FAKE), None, Some(defect));
        assert_eq!(code, Some(1), "{err}");
        assert!(err.contains(reason), "{defect}: {err}");
    }
}

/// `unit-result` as a workflow step runs it: passes on a unit that exited 0, fails on one
/// that did not, and resets the unit either way.
#[test]
fn unit_result_reads_the_unit_a_step_started() {
    let run = Run::new("unitresult");
    let fake = run.bin.join("sudo");
    let check = |defect: Option<&str>| {
        let mut cmd = Command::new(XTASK);
        cmd.args(["unit-result", "--unit", "js-crun-runtime", "--sudo"])
            .arg(&fake);
        match defect {
            Some(d) => cmd.env("XTASK_FAKE_DEFECT", d),
            None => cmd.env_remove("XTASK_FAKE_DEFECT"),
        };
        let out = cmd.output().expect("run xtask");
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };
    let (code, out, err) = check(None);
    assert_eq!(
        (code, out.as_str()),
        (Some(0), "ok: unit js-crun-runtime exited 0\n"),
        "{err}"
    );
    let (code, _, err) = check(Some("unit-fails"));
    assert_eq!(code, Some(1));
    assert!(
        err.contains("unit js-crun-runtime: the unit did not exit 0"),
        "{err}"
    );
    let sudo = run.sudo_args();
    assert_eq!(
        sudo.matches("systemctl reset-failed js-crun-runtime")
            .count(),
        2,
        "{sudo}"
    );
}
