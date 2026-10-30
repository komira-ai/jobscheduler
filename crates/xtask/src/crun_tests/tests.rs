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
        built: &built,
        uid: 1001,
        gid: 118,
        home,
        path: OsStr::new("/usr/bin:/bin"),
        manifest: BUSYBOX_AMD64,
    })
}

/// Catches any of the unit's settings dropped or changed: `Delegate=yes` (the tests
/// cannot set up their cgroup), `OOMScoreAdjust=-900` (the tests see another OOM score
/// than a job inherits), `RuntimeMaxSec=1200` (a hung test would hold the job until its
/// timeout), the slice, the runner's uid and gid (the tests would run as
/// root), the package directory, an environment variable, a unit that does not wait or
/// pass the exit status through, and the ignored tests left out.
#[test]
fn every_target_s_unit_is_delegated_with_the_agent_s_settings() {
    let home = OsStr::new("/srv/runner");
    for target in TARGETS {
        let want = [
            "systemd-run",
            "--quiet",
            "--wait",
            "--collect",
            "--pipe",
            &format!("--unit=js-crun-tests-{target}"),
            "--slice=js-agent.slice",
            "-p",
            "Delegate=yes",
            "-p",
            "OOMScoreAdjust=-900",
            "-p",
            "RuntimeMaxSec=1200",
            "--uid=1001",
            "--gid=118",
            "--working-directory=/w/crates/js-contain",
            "-E",
            "HOME=/srv/runner",
            "-E",
            "PATH=/usr/bin:/bin",
            "-E",
            &format!("JS_TEST_IMAGE=docker://docker.io/library/busybox@{BUSYBOX_AMD64}"),
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
