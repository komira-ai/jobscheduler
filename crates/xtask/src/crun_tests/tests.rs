use std::os::unix::ffi::OsStrExt;

use super::*;

/// Catches the two mapped the wrong way round, or one architecture for every machine
/// (busybox for the wrong one pulls, and the tests run another image than the one
/// named), and a machine with no mapping given a default architecture: macOS's spelling
/// of arm64, a 32-bit ARM board and an empty answer are all refused.
#[test]
fn the_machine_names_its_architecture_and_an_unknown_one_is_refused() {
    assert_eq!(arch_for("x86_64"), Ok("amd64"));
    assert_eq!(arch_for("aarch64"), Ok("arm64"));
    for machine in ["arm64", "armv7l", ""] {
        assert_eq!(
            arch_for(machine),
            Err(Error::Failed(format!(
                "no busybox architecture for {machine}"
            )))
        );
    }
}

/// Catches the two pins swapped, one digest for every machine, a pin that is not a
/// sha256 digest (the image would not resolve), and an architecture with no pin given
/// some digest.
#[test]
fn each_architecture_has_its_own_pinned_manifest() {
    assert_eq!(manifest_for("amd64"), Ok(BUSYBOX_AMD64));
    assert_eq!(manifest_for("arm64"), Ok(BUSYBOX_ARM64));
    assert_ne!(BUSYBOX_AMD64, BUSYBOX_ARM64);
    for pin in [BUSYBOX_AMD64, BUSYBOX_ARM64] {
        let hex = pin.strip_prefix("sha256:").expect("a sha256 digest");
        assert_eq!(hex.len(), 64, "{pin}");
        assert!(
            hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
            "{pin}"
        );
    }
    for arch in ["arm", "s390x", ""] {
        assert_eq!(
            manifest_for(arch),
            Err(Error::Failed(format!(
                "no pinned {BUSYBOX_NAME} manifest for linux/{arch}"
            )))
        );
    }
}

/// A scratch directory for one unit test, empty.
fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("xtask-crun-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

/// Catches a file read from the wrong path, a value not trimmed, and a missing file
/// failing the verb (`lru_gen` and `zswap` are not on every kernel).
#[test]
fn the_host_report_reads_each_file_and_marks_a_missing_one() {
    let root = scratch("report");
    let files = [
        ("proc/sys/kernel/osrelease", "6.8.0-1017-generic\n"),
        ("proc/sys/vm/swappiness", "60\n"),
        ("sys/module/zswap/parameters/enabled", "N\n"),
        (
            "sys/kernel/mm/transparent_hugepage/enabled",
            "always [madvise] never\n",
        ),
        (
            "proc/swaps",
            "Filename\tType\tSize\tUsed\tPriority\n/mnt/swapfile\tfile\t4194300\t0\t-2\n",
        ),
    ];
    for (file, text) in files {
        let path = root.join(file);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
        std::fs::write(path, text).expect("file");
    }
    assert_eq!(
        host_report(&root),
        "kernel 6.8.0-1017-generic; vm.swappiness 60; zswap N; lru_gen n/a; THP always \
         [madvise] never\nswap areas:\nFilename\tType\tSize\tUsed\tPriority\n/mnt/swapfile\t\
         file\t4194300\t0\t-2"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// [`systemd_run_args`] for the target `name` with the runner as these tests set it.
fn args_of(name: &str, home: &OsStr) -> Vec<OsString> {
    let built = Built {
        executable: format!("/w/target/debug/deps/{name}-0f1e").into(),
        dir: "/w/crates/js-contain".into(),
    };
    systemd_run_args(&Unit {
        target: name,
        unit: &unit_name(name, 4242),
        built: &built,
        uid: 1001,
        gid: 118,
        home,
        path: OsStr::new("/usr/bin:/bin"),
        manifest: BUSYBOX_AMD64,
        xtask: Path::new("/w/target/debug/xtask"),
        count_file: Path::new("/w/target/crun-tests/u.count"),
    })
}

/// Catches any of the unit's settings dropped or changed: `Delegate=yes` (the tests
/// cannot set up their cgroup), `OOMScoreAdjust=-900` (the tests see another OOM score
/// than a job inherits), the slice, the runner's uid and gid (the tests would run as
/// root), the package directory, an environment variable, a unit that does not wait, one that
/// is collected on exit (a failed one could not be read), and the ignored tests left out.
#[test]
fn every_target_s_unit_is_delegated_with_the_agent_s_settings() {
    let home = OsStr::new("/srv/runner");
    for target in TARGETS {
        let want = [
            "systemd-run",
            "--quiet",
            "--wait",
            "--pipe",
            &format!("--unit=js-crun-tests-{target}-4242"),
            "--slice=js-agent.slice",
            "-p",
            "Delegate=yes",
            "-p",
            "OOMScoreAdjust=-900",
            "--uid=1001",
            "--gid=118",
            "--working-directory=/w/crates/js-contain",
            "-E",
            "HOME=/srv/runner",
            "-E",
            "PATH=/usr/bin:/bin",
            "-E",
            &format!("JS_TEST_IMAGE=docker://docker.io/library/busybox@{BUSYBOX_AMD64}"),
            "/w/target/debug/xtask",
            "count-run",
            "--count-file",
            "/w/target/crun-tests/u.count",
            "--",
            &format!("/w/target/debug/deps/{target}-0f1e"),
            "--include-ignored",
        ];
        let got: Vec<String> = args_of(target, home)
            .iter()
            .map(|a| a.to_str().expect("utf-8").to_owned())
            .collect();
        assert_eq!(got, want, "{target}");
    }
}

/// Catches a target dropped from the run or run in another order, and a unit given a
/// runtime environment a node does not have (no `XDG_RUNTIME_DIR`, no `containers.conf`).
#[test]
fn the_targets_are_the_real_crun_ones_and_get_no_user_session_environment() {
    assert_eq!(TARGETS, ["crun"]);
    for target in TARGETS {
        for arg in args_of(target, OsStr::new("/h")) {
            let text = arg.to_string_lossy();
            assert!(!text.starts_with("CONTAINERS_CONF"), "{text}");
            assert!(!text.starts_with("XDG_RUNTIME_DIR="), "{text}");
        }
    }
}

/// Catches the environment passed through a lossy UTF-8 conversion.
#[test]
fn a_home_that_is_not_utf8_reaches_the_unit_as_it_is() {
    let home = OsStr::from_bytes(b"/srv/r\xffn");
    let args = args_of(TARGETS[0], home);
    let mut want = OsString::from("HOME=");
    want.push(home);
    assert!(args.contains(&want), "{args:?}");
}

/// Catches a root target's unit that differs from the agent's in anything but who runs it
/// and what it reads: the same `Delegate=yes`, slice, OOM score, directory and arguments as
/// the rootless units, no `--uid` or `--gid` (the unit would run as the invoking user), and
/// an environment of `PATH` and the job user only (no `HOME` of the invoking user, no
/// image).
#[test]
fn a_root_target_s_unit_is_delegated_and_runs_as_root_for_the_job_user() {
    let built = Built {
        executable: "/w/target/debug/deps/crun_root-0f1e".into(),
        dir: "/w/crates/js-contain".into(),
    };
    let got: Vec<String> = root_systemd_run_args(&RootUnit {
        target: "crun_root",
        unit: &unit_name("crun_root", 4242),
        built: &built,
        job_user: "js-job",
        path: OsStr::new("/usr/bin:/bin"),
        xtask: Path::new("/w/target/debug/xtask"),
        count_file: Path::new("/w/target/crun-tests/u.count"),
    })
    .iter()
    .map(|a| a.to_str().expect("utf-8").to_owned())
    .collect();
    let want = [
        "systemd-run",
        "--quiet",
        "--wait",
        "--pipe",
        "--unit=js-crun-tests-crun_root-4242",
        "--slice=js-agent.slice",
        "-p",
        "Delegate=yes",
        "-p",
        "OOMScoreAdjust=-900",
        "--working-directory=/w/crates/js-contain",
        "-E",
        "PATH=/usr/bin:/bin",
        "-E",
        "JS_JOB_USER=js-job",
        "/w/target/debug/xtask",
        "count-run",
        "--count-file",
        "/w/target/crun-tests/u.count",
        "--",
        "/w/target/debug/deps/crun_root-0f1e",
        "--include-ignored",
    ];
    assert_eq!(got, want);
}

/// Catches the root targets run under another name than the test file's (cargo would build
/// nothing) and mixed into the rootless ones (they would run without a job user).
#[test]
fn the_root_targets_are_their_own_list() {
    assert_eq!(ROOT_TARGETS, ["crun_root"]);
    for target in ROOT_TARGETS {
        assert!(!TARGETS.contains(target), "{target}");
    }
}

use std::cell::RefCell;
use std::os::unix::process::ExitStatusExt;

/// A fake `sudo -n`: records every command (its first words) and answers `systemd-run` with
/// `run_status` and `systemctl show` with `show`.
struct FakeSudo {
    run_status: i32,
    show: Result<String, String>,
    calls: RefCell<Vec<String>>,
}

impl FakeSudo {
    fn new(run_status: i32, show: Result<&str, &str>) -> FakeSudo {
        FakeSudo {
            run_status,
            show: show.map(str::to_owned).map_err(str::to_owned),
            calls: RefCell::new(Vec::new()),
        }
    }

    fn note(&self, args: &[OsString]) {
        let words: Vec<String> = args
            .iter()
            .take(3)
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        self.calls.borrow_mut().push(words.join(" "));
    }
}

impl Sudo for FakeSudo {
    fn run(&self, args: &[OsString]) -> Result<ExitStatus, Error> {
        self.note(args);
        // A wait status: the exit code is in the second byte.
        Ok(ExitStatus::from_raw(self.run_status << 8))
    }

    fn read(&self, args: &[OsString]) -> Result<String, Error> {
        self.note(args);
        self.show.clone().map_err(Error::Failed)
    }
}

const EXITED_0: &str = "Result=success\nExecMainCode=1\nExecMainStatus=0\nLoadState=loaded\n";
const EXITED_101: &str = "Result=exit-code\nExecMainCode=1\nExecMainStatus=101\nLoadState=loaded\n";

/// A count file holding `text` (none: no file), in a scratch directory of its own.
fn counted(name: &str, text: Option<&str>) -> std::path::PathBuf {
    let file = scratch(name).join("u.count");
    if let Some(text) = text {
        std::fs::write(&file, text).expect("count file");
    }
    file
}

/// `run_unit` for a fake sudo, with the command the real verb would build and a unit that
/// ran 5 tests.
fn judged(sudo: &FakeSudo) -> Result<(), String> {
    judged_with(sudo, &counted("judged", Some("5\n"))).map(|_| ())
}

/// `run_unit` for a fake sudo and the count file `file`.
fn judged_with(sudo: &FakeSudo, file: &Path) -> Result<u64, String> {
    run_unit(
        sudo,
        "js-crun-tests-crun-7",
        &[OsString::from("systemd-run")],
        file,
    )
}

/// The bug of this module's second half: a unit that exited 0 having run no test. Catches a
/// zero count, a missing file and a garbled one passing, and a count that is read but not
/// returned; a unit that failed is reported for how it ended, not for its count.
#[test]
fn a_clean_unit_passes_only_with_a_positive_count_of_tests() {
    let ok = || FakeSudo::new(0, Ok(EXITED_0));
    assert_eq!(judged_with(&ok(), &counted("pos", Some("5\n"))), Ok(5));
    let zero = judged_with(&ok(), &counted("zero", Some("0\n"))).expect_err("no test ran");
    assert!(zero.contains("ran no test"), "{zero}");
    let missing = judged_with(&ok(), &counted("missing", None)).expect_err("no file");
    assert!(
        missing.contains("could not read the count of tests run"),
        "{missing}"
    );
    for garbage in ["", "many\n", "-1\n", "3 4\n", "2.5\n"] {
        let why = judged_with(&ok(), &counted("garbage", Some(garbage))).expect_err(garbage);
        assert!(
            why.contains("is not a count of tests"),
            "{garbage:?}: {why}"
        );
    }
    let failed = judged_with(
        &FakeSudo::new(0, Ok(EXITED_101)),
        &counted("failed", Some("5\n")),
    )
    .expect_err("the unit failed");
    assert!(failed.contains("ExecMainStatus=101"), "{failed}");
    assert!(!failed.contains("count"), "{failed}");
}

/// The bug: `systemd-run` exited 0 while the test binary in the unit exited 101. Catches a
/// verb that trusts `systemd-run`'s exit status: the unit's own result must fail it, saying
/// what the unit did.
#[test]
fn a_unit_whose_binary_failed_fails_though_systemd_run_exited_0() {
    let sudo = FakeSudo::new(0, Ok(EXITED_101));
    let why = judged(&sudo).expect_err("the unit failed");
    assert!(why.contains("Result=exit-code"), "{why}");
    assert!(why.contains("ExecMainStatus=101"), "{why}");
}

/// Catches a verb that fails a unit that exited 0, and one that does not clear the unit
/// after the read: the unit is read (not just run), then reset, in that order.
#[test]
fn a_unit_that_exited_0_passes_and_is_read_then_reset() {
    let sudo = FakeSudo::new(0, Ok(EXITED_0));
    assert_eq!(judged(&sudo), Ok(()));
    assert_eq!(
        *sudo.calls.borrow(),
        [
            "systemd-run",
            "systemctl show js-crun-tests-crun-7",
            "systemctl reset-failed js-crun-tests-crun-7"
        ]
    );
}

/// Catches a unit read that is trusted over `systemd-run` failing: both must say so, and a
/// failed `systemd-run` fails the target even when the unit reads clean.
#[test]
fn a_systemd_run_that_failed_fails_even_when_the_unit_reads_clean() {
    let sudo = FakeSudo::new(101, Ok(EXITED_0));
    let why = judged(&sudo).expect_err("systemd-run failed");
    assert!(why.starts_with("systemd-run: exit status: 101"), "{why}");
}

/// Catches a read that fails counted as a pass (nothing known about the unit is not a
/// success), and the reason lost.
#[test]
fn a_failed_read_of_the_unit_fails_the_target() {
    let sudo = FakeSudo::new(0, Err("systemctl show failed: exit status: 1"));
    let why = judged(&sudo).expect_err("no result");
    assert!(
        why.contains("could not read the result of unit js-crun-tests-crun-7"),
        "{why}"
    );
    assert!(why.contains("exit status: 1"), "{why}");
    // The unit is still reset.
    assert_eq!(sudo.calls.borrow().len(), 3);
}

/// Catches output that is not read strictly: a line that is not `Key=Value`, empty output,
/// a missing property and a value that is not a clean exit are each a failure, so garbage
/// can never read as success.
#[test]
fn garbled_or_incomplete_output_is_a_failure() {
    let cases = [
        "garbage\n",
        "",
        "Result=success\nExecMainCode=1\nExecMainStatus=0\n",
        "ExecMainCode=1\nExecMainStatus=0\nLoadState=loaded\n",
        "Result=success\nExecMainCode=1\nExecMainStatus=\nLoadState=loaded\n",
        "Result=success\nExecMainCode=1\nExecMainStatus=00\nLoadState=loaded\n",
        "Result=signal\nExecMainCode=2\nExecMainStatus=9\nLoadState=loaded\n",
        "Result=success\nExecMainCode=0\nExecMainStatus=0\nLoadState=loaded\n",
        "Result=exit-code\nExecMainCode=1\nExecMainStatus=0\nLoadState=loaded\n",
    ];
    for show in cases {
        let sudo = FakeSudo::new(0, Ok(show));
        assert!(judged(&sudo).is_err(), "{show:?}");
    }
}

/// Catches a unit that ended cleanly and was unloaded (a unit is collected when inactive
/// unless it failed) read as a failure: systemd reports its defaults with
/// `LoadState=not-found`. A failed unit is never unloaded, so `not-found` with a failing
/// result is still a failure.
#[test]
fn a_cleanly_ended_unit_that_is_already_unloaded_passes() {
    let gone = "Result=success\nExecMainCode=0\nExecMainStatus=0\nLoadState=not-found\n";
    assert_eq!(judged(&FakeSudo::new(0, Ok(gone))), Ok(()));
    let failed_gone = "Result=exit-code\nExecMainCode=1\nExecMainStatus=101\nLoadState=not-found\n";
    assert!(judged(&FakeSudo::new(0, Ok(failed_gone))).is_err());
}

/// Catches a `reset-failed` on a unit that was collected (not loaded), which prints
/// "not loaded" on every pass: no reset call when the unit reads `LoadState=not-found`, in
/// both entry points; a loaded unit is still reset.
#[test]
fn a_collected_unit_is_not_reset() {
    let gone = "Result=success\nExecMainCode=0\nExecMainStatus=0\nLoadState=not-found\n";
    let sudo = FakeSudo::new(0, Ok(gone));
    assert_eq!(judged(&sudo), Ok(()));
    assert_eq!(
        *sudo.calls.borrow(),
        ["systemd-run", "systemctl show js-crun-tests-crun-7"]
    );
    let sudo = FakeSudo::new(0, Ok(gone));
    assert!(check_unit(&sudo, "js-crun-runtime", &counted("gone", Some("2\n"))).is_ok());
    assert_eq!(*sudo.calls.borrow(), ["systemctl show js-crun-runtime"]);
}

/// Catches `unit-result` (for a step that ran its own `systemd-run`) passing on anything
/// but the unit's own clean exit, or not clearing the unit.
#[test]
fn checking_a_unit_a_step_started_reads_its_result() {
    let sudo = FakeSudo::new(0, Ok(EXITED_0));
    let file = counted("check", Some("7\n"));
    assert_eq!(
        check_unit(&sudo, "js-crun-runtime", &file),
        Ok("ok: unit js-crun-runtime exited 0 and ran 7 tests".to_owned())
    );
    assert_eq!(
        *sudo.calls.borrow(),
        [
            "systemctl show js-crun-runtime",
            "systemctl reset-failed js-crun-runtime"
        ]
    );
    let failed = check_unit(&FakeSudo::new(0, Ok(EXITED_101)), "js-crun-runtime", &file);
    assert!(
        matches!(&failed, Err(Error::Failed(why)) if why.contains("ExecMainStatus=101")),
        "{failed:?}"
    );
    assert!(check_unit(&FakeSudo::new(0, Err("no")), "js-crun-runtime", &file).is_err());
    let none = counted("check-none", Some("0\n"));
    assert!(check_unit(&FakeSudo::new(0, Ok(EXITED_0)), "js-crun-runtime", &none).is_err());
}

/// Catches two runs' units sharing a name (a read could be about another run's unit), and
/// two targets of one run sharing one.
#[test]
fn a_unit_is_named_for_its_target_and_its_run() {
    assert_eq!(unit_name("crun", 7), "js-crun-tests-crun-7");
    assert_ne!(unit_name("crun", 7), unit_name("crun", 8));
    assert_ne!(unit_name("crun", 7), unit_name("crun_root", 7));
}

/// Catches the read asking for fewer properties than the verdict needs.
#[test]
fn the_read_asks_for_every_property_the_verdict_needs() {
    let got: Vec<String> = show_args("u")
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        got,
        [
            "systemctl",
            "show",
            "u",
            "-p",
            "Result",
            "-p",
            "ExecMainCode",
            "-p",
            "ExecMainStatus",
            "-p",
            "LoadState"
        ]
    );
}
