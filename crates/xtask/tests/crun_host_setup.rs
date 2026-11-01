//! `packaging/js-agent/crun_host_setup.sh`, the setup of a Linux host that runs `js-agent --runtime
//! crun`, run against a fake root with a `PATH` of recording stand-ins (`host-shim`) for every
//! program that reads or changes the host. Nothing here runs a real `systemctl`, `mount`,
//! `useradd` or `apt-get`: the harness refuses to start if one could be reached, or if it runs as
//! root, and a stand-in nobody configured fails the call (exit 97) and is logged.
//! (Linux only: the script is.)
#![cfg(target_os = "linux")]

use std::process::Command;

mod crun_host;

use crun_host::*;

/// The stand-ins really are the programs the script finds, and the harness knows its own mutating
/// calls: a defence against a test that passes because nothing was looked at.
#[test]
fn the_harness_reaches_only_stand_ins_and_names_the_calls_that_change_the_host() {
    let rig = Rig::new("harness");
    let dirs = [rig.shim.as_path(), rig.tools.as_path()];
    for name in GUARDED {
        assert_eq!(resolve(name, &dirs), Some(rig.shim.join(name)), "{name}");
    }
    for call in [
        "apt-get install x",
        "systemctl daemon-reload",
        "systemctl stop a",
        "systemctl start a",
        "usermod x",
        "mount a",
        "umount a",
    ] {
        assert!(mutating(call), "{call}");
    }
    for call in [
        "systemctl show -p ActiveState",
        "systemctl --version",
        "findmnt -n",
        "getent passwd",
        "fuser -m",
    ] {
        assert!(!mutating(call), "{call}");
    }
    // A stand-in nobody configured fails closed and says so.
    std::fs::write(rig.shim.join("answers"), "").expect("answers");
    let out = Command::new(rig.shim.join("systemctl"))
        .arg("poweroff")
        .env_clear()
        .env("HOST_SHIM_DIR", &rig.shim)
        .envs(profile_env())
        .output()
        .expect("run");
    assert_eq!(out.status.code(), Some(97));
    assert_eq!(rig.calls(), ["UNCONFIGURED systemctl poweroff"]);
    // So does one run with no directory at all.
    let out = Command::new(rig.shim.join("systemctl"))
        .env_clear()
        .envs(profile_env())
        .output()
        .expect("run");
    assert_eq!(out.status.code(), Some(97));
    // The escapes of an answer: a tab, a backslash, a backslash before any other character, and a
    // backslash that ends the text.
    std::fs::write(
        rig.shim.join("answers"),
        rule("uname", "-s", 0, r"a\tb\\c\zd\", &[]) + "\n",
    )
    .expect("answers");
    let out = Command::new(rig.shim.join("uname"))
        .arg("-s")
        .env_clear()
        .env("HOST_SHIM_DIR", &rig.shim)
        .envs(profile_env())
        .output()
        .expect("run");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "a\tb\\czd\\");
}

/// The default is a dry run: it prints the plan, including the scratch filesystem's quota, reads
/// the host, and changes neither the root nor the host.
#[test]
fn a_dry_run_prints_the_plan_and_changes_nothing() {
    let rig = Rig::new("dry-run");
    let before = rig.snapshot();
    let mut args = vec![
        "--server".to_owned(),
        SERVER.to_owned(),
        "--pool".into(),
        "farm".into(),
    ];
    args.extend(rig.file_args());
    let out = rig.run(&args.iter().map(String::as_str).collect::<Vec<_>>());
    assert_eq!(out.code, 0, "{}", out.text);
    for want in [
        "would: apt-get install crun uidmap",
        "would: useradd",
        "would: usermod --add-subuids 200000-265535 jsjob",
        "prjquota",
        "dry run: nothing changed",
    ] {
        assert!(out.text.contains(want), "{want}: {}", out.text);
    }
    assert_untouched(&rig, &before);
    assert!(!rig.calls().is_empty(), "the host was not read");
}

/// An apply on a host with nothing of the setup makes every piece, in an order that works: the
/// packages before the user, the unit's edit and a reload before the unmount and mount, the mount
/// check after it.
#[test]
fn an_apply_on_a_fresh_root_sets_the_host_up() {
    let rig = Rig::new("apply-fresh");
    let out = rig.apply();
    assert_eq!(out.code, 0, "{}", out.text);
    assert_eq!(rig.text("etc/subuid"), "jsjob:200000:65536\n");
    assert_eq!(rig.text("etc/subgid"), "jsjob:200000:65536\n");
    assert!(rig.text("etc/passwd").contains("jsjob:"));
    for (dir, want) in [
        ("etc/js-agent", 0o700),
        ("var/lib/js-agent", 0o711),
        ("var/lib/js-agent/state", 0o700),
        ("var/lib/js-agent/scratch", 0o711),
        ("var/lib/js-agent/scratch/runs", 0o711),
    ] {
        assert_eq!(mode(&rig, dir), want, "{dir}");
    }
    assert_eq!(
        rig.text("etc/systemd/system/js-agent-crun.service"),
        rendered_unit()
    );
    assert_eq!(
        rig.text("etc/tmpfiles.d/js-agent-crun.conf"),
        read(TMPFILES)
    );
    assert_eq!(
        rig.text(&format!("etc/systemd/system/{MOUNT}")),
        mount_unit("loop,prjquota"),
        "the existing option is kept and prjquota added"
    );
    let calls = rig.calls();
    assert!(
        !calls.iter().any(|c| c.starts_with("UNCONFIGURED")),
        "{calls:?}"
    );
    assert!(
        !calls
            .iter()
            .any(|c| c.starts_with("mount ") || c.starts_with("umount ")),
        "{calls:?}"
    );
    let stop = index(&calls, &format!("systemctl stop {MOUNT}"));
    let start = index(&calls, &format!("systemctl start {MOUNT}"));
    let reload = index(&calls, "systemctl daemon-reload");
    let check = calls
        .iter()
        .rposition(|c| c == &format!("findmnt -n -o OPTIONS --mountpoint {SCRATCH}"))
        .expect("the mount options were read");
    assert!(reload < stop && stop < start && start < check, "{calls:?}");
    assert!(
        index(
            &calls,
            "apt-get install -y --no-install-recommends crun uidmap"
        ) < index(&calls, "usermod --add-subuids 200000-265535 jsjob")
    );
    let manifest = rig.text("var/lib/js-crun-host-setup/manifest");
    for line in [
        "pkg\tcrun",
        "pkg\tuidmap",
        "user\tjsjob\t998",
        "subuid\tjsjob\t200000\t65536",
        "mount-options\t",
    ] {
        assert!(manifest.contains(line), "{line}: {manifest}");
    }
    assert!(out.text.contains("runtimes: docker crun"), "{}", out.text);
}

/// A second run on a host that is set up changes nothing and says so (exit 3), whether it is a
/// dry run or an apply: the root is the same bytes and no call changes the host.
#[test]
fn a_rerun_on_a_host_that_is_set_up_changes_nothing() {
    let rig = Rig::new("rerun");
    let first = rig.apply();
    assert_eq!(first.code, 0, "{}", first.text);
    let before = rig.snapshot();
    let mark = rig.calls().len();
    let second = rig.apply();
    assert_eq!(second.code, 3, "{}", second.text);
    assert!(second.text.contains("nothing to do"), "{}", second.text);
    let all = rig.calls();
    assert!(!all[mark..].is_empty(), "the second run read nothing");
    assert!(
        mutations(&all[mark..]).is_empty(),
        "{:?}",
        mutations(&all[mark..])
    );
    assert!(rig.snapshot() == before, "the root changed");
    let mut args = vec![
        "--server".to_owned(),
        SERVER.to_owned(),
        "--pool".into(),
        "farm".into(),
    ];
    args.extend(rig.file_args());
    let dry = rig.run(&args.iter().map(String::as_str).collect::<Vec<_>>());
    assert_eq!(dry.code, 3, "{}", dry.text);
    assert!(rig.snapshot() == before, "the dry run changed the root");
    assert!(mutations(&rig.calls()[mark..]).is_empty());
}

/// Where the mount unit already has prjquota and the mount shows it, the unit is not edited and
/// the filesystem is not unmounted: the rest of the setup goes on.
#[test]
fn prjquota_already_present_is_left_alone() {
    let rig = Rig::new("quota-present");
    rig.write(
        &format!("etc/systemd/system/{MOUNT}"),
        &mount_unit("loop,prjquota"),
    );
    rig.write("mount-options", "rw,relatime,attr2,inode64,prjquota\n");
    let out = rig.apply();
    assert_eq!(out.code, 0, "{}", out.text);
    assert_eq!(
        rig.text(&format!("etc/systemd/system/{MOUNT}")),
        mount_unit("loop,prjquota")
    );
    let calls = rig.calls();
    assert!(
        !calls.contains(&format!("systemctl stop {MOUNT}")),
        "{calls:?}"
    );
    assert!(
        !calls.contains(&format!("systemctl start {MOUNT}")),
        "{calls:?}"
    );
    assert!(
        !calls.contains(&"fuser -m -- /var/lib/js-agent/scratch".to_owned()),
        "{calls:?}"
    );
    assert!(out.text.contains("prjquota"), "{}", out.text);
}

/// Where the unit has prjquota but the running mount does not (an interrupted earlier run), the
/// unit is not edited again and the mount is made again.
#[test]
fn a_unit_with_prjquota_and_a_mount_without_it_is_mounted_again() {
    let rig = Rig::new("quota-unit-only");
    rig.write(
        &format!("etc/systemd/system/{MOUNT}"),
        &mount_unit("loop,prjquota"),
    );
    let out = rig.apply();
    assert_eq!(out.code, 0, "{}", out.text);
    assert_eq!(
        rig.text(&format!("etc/systemd/system/{MOUNT}")),
        mount_unit("loop,prjquota")
    );
    let calls = rig.calls();
    assert!(
        index(&calls, &format!("systemctl stop {MOUNT}"))
            < index(&calls, &format!("systemctl start {MOUNT}"))
    );
}

/// Something holds the scratch mount open: the run is refused before any change, in a dry run
/// and an apply.
#[test]
fn a_mount_that_something_holds_open_is_refused() {
    let mut rig = Rig::new("held");
    rig.rule(rule(
        "fuser",
        &format!("-m -- {SCRATCH}"),
        0,
        " 4242\\n",
        &[],
    ));
    let before = rig.snapshot();
    let out = rig.apply();
    assert_eq!(out.code, 2, "{}", out.text);
    assert!(
        out.text.contains("REFUSE") && out.text.contains("holds"),
        "{}",
        out.text
    );
    assert_untouched(&rig, &before);
    let dry = rig.run(&[]);
    assert_eq!(dry.code, 2, "{}", dry.text);
}

/// The agent runs (either unit, in any state but inactive or failed): refused before any change.
#[test]
fn a_running_agent_is_refused() {
    for (name, unit, state) in [
        ("running-engine-unit", "js-agent.service", "active"),
        ("running-crun-unit", "js-agent-crun.service", "activating"),
    ] {
        let mut rig = Rig::new(name);
        rig.rule(rule(
            "systemctl",
            &format!("show -p ActiveState --value -- {unit}"),
            0,
            &format!("{state}\\n"),
            &[],
        ));
        let before = rig.snapshot();
        let out = rig.apply();
        assert_eq!(out.code, 2, "{}", out.text);
        assert!(
            out.text.contains(unit) && out.text.contains(state),
            "{}",
            out.text
        );
        assert_untouched(&rig, &before);
    }
}

/// The mount comes back without prjquota: the run fails (exit 1), says why, and does not report
/// the host as set up.
#[test]
fn a_remount_that_does_not_show_prjquota_fails() {
    let mut rig = Rig::new("quota-not-active");
    rig.rule(rule(
        "systemctl",
        &format!("start {MOUNT}"),
        0,
        "",
        &[format!(
            "write={}=rw,noquota\\n",
            rig.path("mount-options").display()
        )],
    ));
    let out = rig.apply();
    assert_eq!(out.code, 1, "{}", out.text);
    assert!(out.text.contains("prjquota is not active"), "{}", out.text);
    assert!(!out.text.contains("done."), "{}", out.text);
}

/// A mount unit with no `Options=` line cannot have its options kept, and a missing unit file
/// cannot be edited: both are refused before any change, and `--skip-scratch-quota` leaves the
/// scratch filesystem out.
#[test]
fn a_mount_unit_that_cannot_be_edited_is_refused() {
    let rig = Rig::new("quota-no-options");
    rig.write(
        &format!("etc/systemd/system/{MOUNT}"),
        &mount_unit("loop").replace("Options=loop\n", ""),
    );
    let before = rig.snapshot();
    let out = rig.apply();
    assert_eq!(out.code, 2, "{}", out.text);
    assert!(out.text.contains("Options="), "{}", out.text);
    assert_untouched(&rig, &before);

    let missing = Rig::new("quota-no-unit");
    std::fs::remove_file(missing.path(&format!("etc/systemd/system/{MOUNT}"))).expect("remove");
    let before = missing.snapshot();
    let out = missing.apply();
    assert_eq!(out.code, 2, "{}", out.text);
    assert_untouched(&missing, &before);

    let mut args = missing.apply_args();
    args.push("--skip-scratch-quota".into());
    let out = missing.run(&args.iter().map(String::as_str).collect::<Vec<_>>());
    assert_eq!(out.code, 0, "{}", out.text);
    assert!(!missing.calls().iter().any(|c| c.contains(".mount")));
}

/// An apply needs root, the server and pool for the unit, and a unit that is the pinned file; each
/// missing piece is a refusal before any change.
#[test]
fn an_apply_without_what_it_needs_is_refused_before_any_change() {
    let mut rig = Rig::new("refusals");
    let before = rig.snapshot();
    // Not root.
    rig.rule(rule("id", "-u", 0, "1000\\n", &[]));
    let out = rig.apply();
    assert_eq!(out.code, 2, "{}", out.text);
    assert!(out.text.contains("not root"), "{}", out.text);
    assert_untouched(&rig, &before);
    rig.front.clear();
    // No server and pool.
    let mut args: Vec<String> = vec!["--apply".into()];
    args.extend(rig.file_args());
    let out = rig.run(&args.iter().map(String::as_str).collect::<Vec<_>>());
    assert_eq!(out.code, 2, "{}", out.text);
    assert!(out.text.contains("--server and --pool"), "{}", out.text);
    assert_untouched(&rig, &before);
    // A unit that is not the pinned file.
    let mut args = rig.apply_args();
    let at = args
        .iter()
        .position(|a| a == "--unit-sha256")
        .expect("flag")
        + 1;
    args[at] = "0".repeat(64);
    let out = rig.run(&args.iter().map(String::as_str).collect::<Vec<_>>());
    assert_eq!(out.code, 2, "{}", out.text);
    assert!(out.text.contains("sha256"), "{}", out.text);
    assert_untouched(&rig, &before);
    // A flag the script does not know.
    let out = rig.run(&["--bogus"]);
    assert_eq!(out.code, 2, "{}", out.text);
    assert!(out.text.contains("--bogus"), "{}", out.text);
    assert_untouched(&rig, &before);
}

/// `--help` prints every flag the script takes and changes nothing; the tests above use them all.
#[test]
fn help_lists_the_flags() {
    let rig = Rig::new("help");
    let before = rig.snapshot();
    let out = rig.run(&["--help"]);
    assert_eq!(out.code, 0, "{}", out.text);
    for flag in [
        "--apply",
        "--root",
        "--job-user",
        "--subid-start",
        "--unit",
        "--unit-sha256",
        "--tmpfiles",
        "--tmpfiles-sha256",
        "--server",
        "--pool",
        "--scratch-mount-unit",
        "--skip-scratch-quota",
    ] {
        assert!(out.text.contains(flag), "{flag}: {}", out.text);
    }
    assert_untouched(&rig, &before);
    assert!(rig.calls().is_empty(), "--help ran a program");
}

/// A write that fails (here: a unit bigger than the 2 KiB a full disk would take) is a failed step:
/// exit 1, the file is not installed, no half-written copy is left, and the manifest does not
/// record a file that was not written.
#[test]
fn a_write_that_fails_installs_nothing_and_records_nothing() {
    let rig = Rig::new("write-fails");
    let args = rig.apply_args();
    let out = rig.run_with(&args.iter().map(String::as_str).collect::<Vec<_>>(), true);
    assert_eq!(out.code, 1, "{}", out.text);
    assert!(out.text.contains("FAILED"), "{}", out.text);
    assert!(
        !rig.path("etc/systemd/system/js-agent-crun.service")
            .exists(),
        "a unit was installed"
    );
    let left: Vec<String> = rig
        .snapshot()
        .into_keys()
        .filter(|p| p.contains(".new."))
        .collect();
    assert!(left.is_empty(), "{left:?}");
    let manifest = rig.text("var/lib/js-crun-host-setup/manifest");
    assert!(
        !manifest
            .lines()
            .any(|l| l.starts_with("file\t") && l.contains("js-agent-crun.service")),
        "{manifest}"
    );
    // The run says which command failed first.
    let last = manifest.lines().last().expect("a line");
    assert!(
        last.starts_with("incomplete\t1\tinstall ") && last.contains("js-agent-crun.service"),
        "{manifest}"
    );
    assert!(!rig.calls().iter().any(|c| c.starts_with("UNCONFIGURED")));
}

/// The mount does not start after it was stopped: the remaining steps do not run (the scratch
/// directories are not made on the root filesystem under the mount point), the run exits 1, the
/// manifest keeps the unit edit, and the output says how to start the mount again.
#[test]
fn a_mount_that_does_not_start_again_aborts_the_run_and_says_how_to_recover() {
    let mut rig = Rig::new("start-fails");
    rig.rule(rule("systemctl", &format!("start {MOUNT}"), 1, "", &[]));
    let out = rig.apply();
    assert_eq!(out.code, 1, "{}", out.text);
    assert!(
        out.text.contains(&format!("systemctl start {MOUNT}")),
        "{}",
        out.text
    );
    assert!(out.text.contains("unmounted"), "{}", out.text);
    assert!(
        !rig.path("var/lib/js-agent").exists(),
        "directories were made"
    );
    assert!(
        !rig.path("etc/systemd/system/js-agent-crun.service")
            .exists(),
        "the unit was installed"
    );
    let manifest = rig.text("var/lib/js-crun-host-setup/manifest");
    assert!(manifest.contains("mount-options\t"), "{manifest}");
    let calls = rig.calls();
    assert!(
        calls.contains(&format!("systemctl stop {MOUNT}")),
        "{calls:?}"
    );
}
