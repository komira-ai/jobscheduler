//! A first install of the Linux package's own manifest (packaging/js-agent/hostkeeper), run as
//! hostkeeper's daemon runs one: its manifest loader (`hostkeeper_config`) reads the manifest, its
//! step runner (`hostkeeper_steps::StepRunner`) writes the params file and orders the steps, and
//! its systemd launcher builds each step's `systemd-run` line and keeps what the hook printed. All
//! of it is hostkeeper's code at the revision the workspace pins (v0.1.194). Only `systemd-run` is a stand-in ([`StandIn`]): it runs the command line the launcher
//! gave it, with `js-update-hook-fake` (the hook's own steps over the fake ports,
//! tests/bin/update_hook_fake.rs) in place of the release's `bin/js-agent`, and the hook's state
//! directory where the fake keeps it (`--state-dir`).
//!
//! Nothing but the hook makes its state directory: hostkeeper treats `state_dir` as one more
//! value of `[params]`, and neither the package nor the host setup lists it, so the first step
//! of a first install finds none.

use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use hostkeeper_config::manifest::{Expected, HostkeeperVersion, Manifest};
use hostkeeper_config::names::{ServiceName, Version};
use hostkeeper_config::platform::Platform;
use hostkeeper_hook::{Params, Verdict};
use hostkeeper_steps::{
    BootId, BootIdError, Bounds, Captured, Clock, Program, RecordStore, Ring, Spawner, Staged,
    StepRunner, SystemdLauncher, UpdateOrder,
};
use js_agent::update_hook::NoGate;
use js_testkit::dir::TestDir;

use super::conformance::BIN;
use super::fakes::World;
use super::fakes::harness::OWNED_CORDON;

/// The version the first install installs.
const VERSION: &str = "0.1.208";

/// The hostkeeper release that runs the update, which the manifest's `min_hostkeeper` is judged
/// against.
const HOSTKEEPER: &str = "0.1.194";

/// The `min_hostkeeper` the release fills in (crates/xtask/src/release/hostkeeper_package.rs).
const MIN_HOSTKEEPER: &str = "0.1.92";

/// The digest of the release's tarball: the order's, which hostkeeper passes on as
/// `updater.artifact_digest`.
const TARBALL: &str = "4a5b6c7d8e9f00112233445566778899aabbccddeeff00112233445566778899";

/// The package's directory in the repository.
fn package() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/js-agent/hostkeeper")
}

/// A clock that moves only when the runner waits.
struct StillClock(AtomicU64);

impl Clock for StillClock {
    fn mono_secs(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }

    fn boot_id(&self) -> Result<BootId, BootIdError> {
        BootId::new("the test's clock", "boot-1")
    }

    fn sleep_secs(&self, secs: u64) {
        self.0.fetch_add(secs, Ordering::SeqCst);
    }
}

/// One step as the stand-in ran it.
#[derive(Clone, Debug)]
struct Ran {
    /// The program the launcher asked `systemd-run` to run, and its arguments.
    line: Vec<OsString>,
    /// The params file the step was given, as it was when the step started.
    params: String,
    /// The exit status.
    code: Option<i32>,
    /// What the hook printed on standard error, which the launcher keeps as the hook's log.
    stderr: String,
    /// The last line of its standard output.
    last_line: Option<String>,
}

/// `systemd-run`: runs the program after `--` in its place, as `js-update-hook-fake` over the
/// fakes of `root`.
struct StandIn {
    root: PathBuf,
    state_dir: PathBuf,
    ran: Arc<Mutex<Vec<Ran>>>,
}

impl Spawner for StandIn {
    fn spawn(&self, program: Program, args: &[OsString]) -> io::Result<Captured> {
        assert_eq!(program, Program::SystemdRun, "{args:?}: no unit is stopped");
        let at = args.iter().position(|arg| arg == "--").expect("a `--`");
        let line = args[at + 1..].to_vec();
        let flag = line
            .iter()
            .position(|arg| arg == "--params")
            .expect("--params");
        let params = fs::read_to_string(&line[flag + 1])?;
        let output = Command::new(BIN)
            .arg("--world")
            .arg(self.root.join("world"))
            .arg("--control")
            .arg(self.root.join("control"))
            .arg("--state-dir")
            .arg(&self.state_dir)
            .args(&line[1..])
            .output()?;
        let (mut stdout, mut stderr) = (Ring::new(), Ring::new());
        stdout.write_all(&output.stdout)?;
        stderr.write_all(&output.stderr)?;
        let captured = Captured {
            code: output.status.code(),
            stdout,
            stderr,
        };
        self.ran
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Ran {
                line,
                params,
                code: captured.code,
                stderr: String::from_utf8_lossy(&captured.stderr.bytes()).into_owned(),
                last_line: captured.stdout.last_line(),
            });
        Ok(captured)
    }
}

/// The host of one test, below a scratch directory that stands for `/`.
struct Host {
    tmp: TestDir,
    /// The hook's state directory, where the fake keeps it.
    state_dir: PathBuf,
    ran: Arc<Mutex<Vec<Ran>>>,
}

impl Host {
    /// A host enrolled for its first install: an agent state root that is empty, and no hook
    /// state directory, which the fake keeps at `/var/lib/js-agent-update` below the root.
    fn new(name: &str) -> Host {
        Host::with_state_dir(name, "var/lib/js-agent-update")
    }

    /// [`Host::new`] with the hook's state directory at `state_dir` below the root.
    fn with_state_dir(name: &str, state_dir: &str) -> Host {
        let tmp = TestDir::under(env!("CARGO_TARGET_TMPDIR"), name);
        let root = tmp.path();
        let world = World::new(&root.join("world"), &root.join("control"), Box::new(NoGate));
        world.state_root();
        fs::create_dir_all(
            root.join("world")
                .join(Path::new(OWNED_CORDON).parent().unwrap()),
        )
        .unwrap();
        fs::create_dir_all(root.join("var/lib")).unwrap();
        Host {
            state_dir: root.join(state_dir),
            tmp,
            ran: Arc::default(),
        }
    }

    fn root(&self) -> &Path {
        self.tmp.path()
    }

    /// `/var/lib/hostkeeper/packages/js-agent/<VERSION>`: the package's files and the manifest
    /// with its placeholders filled in, every entry closed to group and others as the unpacker
    /// leaves it.
    fn unpack(&self) -> PathBuf {
        let dir = self
            .root()
            .join("var/lib/hostkeeper/packages/js-agent")
            .join(VERSION);
        for sub in ["bin", "units", "files"] {
            fs::create_dir_all(dir.join(sub)).unwrap();
        }
        let manifest = fs::read_to_string(package().join("manifest.toml.in"))
            .unwrap()
            .replace("@VERSION@", VERSION)
            .replace("@PLATFORM@", "linux-x86_64")
            .replace("@MIN_HOSTKEEPER@", MIN_HOSTKEEPER);
        fs::write(dir.join("manifest.toml"), manifest).unwrap();
        for sub in ["units", "files"] {
            for entry in fs::read_dir(package().join(sub)).unwrap() {
                let entry = entry.unwrap();
                fs::copy(entry.path(), dir.join(sub).join(entry.file_name())).unwrap();
            }
        }
        // Never run: the stand-in runs the fake in its place.
        fs::write(dir.join("bin/js-agent"), "the release's agent\n").unwrap();
        close(self.root());
        fs::set_permissions(dir.join("bin/js-agent"), fs::Permissions::from_mode(0o755)).unwrap();
        dir
    }

    /// The order the daemon gives its step runner for a first install of `dir`, from the
    /// manifest as hostkeeper's loader reads it (hostkeeper-daemon `verbs::update::update_order`
    /// and `declared_params`).
    fn order(&self, dir: &Path) -> UpdateOrder {
        let expected = Expected {
            name: ServiceName::new("js-agent").unwrap(),
            version: Version::new(VERSION).unwrap(),
            platform: Platform::LinuxX86_64,
        };
        let manifest = Manifest::load(
            dir,
            &expected,
            &HostkeeperVersion::parse(HOSTKEEPER).unwrap(),
        )
        .expect("hostkeeper loads the manifest");
        let hook = &manifest.hook;
        let timeouts = &hook.timeout_secs;
        UpdateOrder {
            service: "js-agent".to_owned(),
            target_version: VERSION.to_owned(),
            previous_version: None,
            artifact_digest: TARBALL.to_owned(),
            staged: Staged::seal(dir).unwrap(),
            previous: None,
            hook_binary: PathBuf::from(&hook.command[0]),
            bounds: Bounds {
                prepare_secs: timeouts.prepare.map_or(0, |secs| secs.get()),
                cutover_secs: timeouts.cutover.get(),
                verify_secs: timeouts.verify.get(),
                rollback_secs: timeouts.rollback.get(),
                cleanup_secs: timeouts.cleanup.get(),
                verify_deadline_secs: hook.verify_deadline_secs,
                promotion_ceiling_secs: hook.promotion_ceiling_secs.map_or(u64::MAX, |s| s.get()),
                cleanup_retries: hook.cleanup_retries,
            },
            declared_params: Params::parse_declared(&manifest.params.to_string()).unwrap(),
            min_updater_protocol: None,
            order_try: Some(1),
            builtin_steps: false,
        }
    }

    /// Runs the order through hostkeeper's step runner, with the updater's state directory of
    /// js-agent (`/var/lib/updater/state/js-agent`).
    fn run(&self, order: &UpdateOrder) -> hostkeeper_steps::Finished {
        let updater = self.root().join("var/lib/updater/state/js-agent");
        fs::create_dir_all(&updater).unwrap();
        fs::set_permissions(&updater, fs::Permissions::from_mode(0o700)).unwrap();
        let launcher = SystemdLauncher::new(StandIn {
            root: self.root().to_owned(),
            state_dir: self.state_dir.clone(),
            ran: Arc::clone(&self.ran),
        });
        let runner = StepRunner::new(
            Arc::new(launcher),
            Arc::new(StillClock(AtomicU64::new(1000))),
            RecordStore::new(&updater),
        );
        let finished = runner.run(order).expect("the runner ends with a verdict");
        assert!(
            !updater.join("params.toml").exists(),
            "hostkeeper removes the params file at the verdict"
        );
        finished
    }

    fn ran(&self) -> Vec<Ran> {
        self.ran
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// `text` with the scratch directory written as `<root>`.
    fn rooted(&self, text: &str) -> String {
        text.replace(&self.root().display().to_string(), "<root>")
    }
}

/// Takes the write bits of group and others off every directory below `root`, as the unpacker
/// leaves a release and whatever this test process's umask made of them.
fn close(root: &Path) {
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        let meta = fs::symlink_metadata(&path).unwrap();
        let mode = meta.permissions().mode() & 0o7777 & !0o022;
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        if meta.is_dir() {
            close(&path);
        }
    }
    let mode = fs::metadata(root).unwrap().permissions().mode() & 0o7777 & !0o022;
    fs::set_permissions(root, fs::Permissions::from_mode(mode)).unwrap();
}

/// The params file hostkeeper writes for the first install of the package: the manifest's
/// `[params]` as they are, and its reserved keys for a first install (no previous release or
/// version).
const FIRST_INSTALL_PARAMS: &str = r#"cleanup_timeout_secs = 120
cordon_lease_secs = 300
cordon_max_secs = 17355
cutover_timeout_secs = 4320
defer_retry_secs = 300
drain_wait_secs = 3600
flags_file = "/etc/js-agent/flags"
prepare_timeout_secs = 3900
release_link = "/var/lib/hostkeeper/packages/js-agent/current"
rollback_timeout_secs = 8220
runtime = "crun"
state_dir = "/var/lib/js-agent-update"
state_root = "/var/lib/js-agent/state"
token_file = "/etc/js-agent/token"
unit = "js-agent-crun.service"
"updater.artifact_digest" = "sha256:4a5b6c7d8e9f00112233445566778899aabbccddeeff00112233445566778899"
"updater.promotion_ceiling_secs" = 9223372036854775807
"updater.staged_release" = "<root>/var/lib/hostkeeper/packages/js-agent/0.1.208"
"updater.target_version" = "0.1.208"
verify_deadline_secs = 9000
verify_timeout_secs = 615
"#;

/// A first install: hostkeeper 0.1.194 orders js-agent on a host where nothing has made
/// the hook's state directory. The params file it writes is read by the hook as it is, `prepare`
/// makes the state directory (root's alone, mode 0700) and the update runs to the end:
/// `prepare`, `cutover`, `verify` and `cleanup`, each in the bare form hostkeeper runs a step
/// in, the staged release given to `prepare` alone. Catches a hook that refuses a state
/// directory that is not there yet (`state_invalid` from `prepare` and from the `rollback` after
/// it, and a halted host), and a params file the hook cannot read.
#[test]
fn a_first_install_through_hostkeepers_runner_makes_the_state_directory_and_succeeds() {
    let host = Host::new("hk-runner-first-install");
    let dir = host.unpack();
    let order = host.order(&dir);

    let finished = host.run(&order);

    assert_eq!(
        (finished.verdict, finished.reason),
        (
            Verdict::Succeeded {
                promotion_pending: false
            },
            None
        ),
        "{:#?}",
        host.ran()
    );
    let ran = host.ran();
    let lines: Vec<Vec<String>> = ran
        .iter()
        .map(|ran| {
            ran.line
                .iter()
                .map(|arg| host.rooted(&arg.to_string_lossy()))
                .collect()
        })
        .collect();
    let release = "<root>/var/lib/hostkeeper/packages/js-agent/0.1.208";
    let params = "<root>/var/lib/updater/state/js-agent/params.toml";
    let binary = format!("{release}/bin/js-agent");
    let step = |words: &[&str]| -> Vec<String> {
        std::iter::once(binary.as_str())
            .chain(words.iter().copied())
            .chain(["--params", params])
            .map(str::to_owned)
            .collect()
    };
    assert_eq!(
        lines,
        [
            step(&["prepare", release]),
            step(&["cutover"]),
            step(&["verify"]),
            step(&["cleanup"]),
        ]
    );
    for ran in &ran {
        assert_eq!(host.rooted(&ran.params), FIRST_INSTALL_PARAMS);
        assert_eq!(ran.code, Some(0), "{ran:#?}");
        assert_eq!(
            ran.stderr, "",
            "a step that succeeds says nothing on standard error"
        );
    }
    let made = fs::symlink_metadata(&host.state_dir).expect("the state directory");
    assert!(made.is_dir());
    assert_eq!(made.permissions().mode() & 0o7777, 0o700);
    assert!(
        !host.state_dir.join("record.json").exists(),
        "cleanup deletes the record"
    );
}

/// A state directory whose parent is not there either cannot be made: the hook makes only the
/// last directory of the path. `prepare` fails `state_invalid` and so does the `rollback` after
/// it, and the host halts; each step says why on standard error, which hostkeeper keeps as the
/// hook's log, in the same words as the detail of its last line. Catches a refusal that reaches
/// the journal as a bare reason token, and a hook that makes the directories above its own.
#[test]
fn a_state_directory_that_cannot_be_made_halts_and_each_step_says_why_on_standard_error() {
    let host = Host::with_state_dir("hk-runner-no-parent", "gone/js-agent-update");
    let dir = host.unpack();
    let order = host.order(&dir);

    let finished = host.run(&order);

    assert_eq!(
        (finished.verdict, finished.reason.as_deref()),
        (Verdict::Halt, Some("state_invalid")),
        "{:#?}",
        host.ran()
    );
    let detail = "<root>/gone: No such file or directory (os error 2)";
    let said: Vec<(Option<i32>, String, serde_json::Value)> = host
        .ran()
        .iter()
        .map(|ran| {
            let line = ran.last_line.as_deref().expect("a last line");
            (
                ran.code,
                host.rooted(&ran.stderr),
                serde_json::from_str(&host.rooted(line)).unwrap(),
            )
        })
        .collect();
    let report = serde_json::json!({
        "class": "release",
        "reversible": true,
        "promotion_pending": false,
        "reason": "state_invalid",
        "detail": detail,
    });
    assert_eq!(
        said,
        [
            (
                Some(1),
                format!("js-agent update-hook: prepare failed: state_invalid: {detail}\n"),
                report.clone(),
            ),
            (
                Some(1),
                format!("js-agent update-hook: rollback failed: state_invalid: {detail}\n"),
                report,
            ),
        ]
    );
    assert!(
        !host.root().join("gone").exists(),
        "nothing above the state directory is made"
    );
}
