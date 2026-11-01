//! The refusals and failure paths of `packaging/js-agent/crun_host_setup.sh` beyond the main path
//! (`crun_host_setup.rs`): what it checks before it unmounts the scratch filesystem, what it
//! trusts of the unit files and the user database, what it does when it is interrupted, and the
//! host checks of its preflight. Same fake root and stand-ins (`crun_host/mod.rs`).
#![cfg(target_os = "linux")]

mod crun_host;

use std::os::unix::fs::{PermissionsExt as _, symlink};

use crun_host::*;

const TARGETS: &str = "-n -o TARGET -S /dev/loop3";

fn unit_path() -> String {
    format!("etc/systemd/system/{MOUNT}")
}

fn show_state(unit: &str, state: &str) -> String {
    rule(
        "systemctl",
        &format!("show -p ActiveState --value -- {unit}"),
        0,
        &format!("{state}\\n"),
        &[],
    )
}

// --- before the scratch filesystem is unmounted -------------------------------------------------

/// The units whose stop follows the mount's (what requires, binds, or is part of it), asked of the
/// stand-in the way the script asks.
fn stoppers(unit: &str, names: &str) -> String {
    rule(
        "systemctl",
        &format!("show -p RequiredBy,RequisiteOf,BoundBy,ConsistsOf --value -- {unit}"),
        0,
        &esc(&format!("{names}\n\n\n\n")),
        &[],
    )
}

/// A unit that is active and requires the mount, directly or through other units, goes down with
/// it and does not come back: refused, naming every one found. A custom target is no exception;
/// a dependent that is not active, and `local-fs.target`, which a mount legitimately touches, do
/// not matter.
#[test]
fn an_active_unit_that_depends_on_the_mount_is_refused() {
    let mut rig = Rig::new("dependent-active");
    rig.rule(stoppers(MOUNT, "other.service"));
    rig.rule(show_state("other.service", "active"));
    rig.expect_refused("other.service");

    let mut rig = Rig::new("dependent-custom-target");
    rig.rule(stoppers(MOUNT, "farm.target local-fs.target"));
    rig.rule(show_state("farm.target", "active"));
    rig.expect_refused("farm.target");

    let mut rig = Rig::new("dependent-indirect");
    rig.rule(stoppers(MOUNT, "mid.service"));
    rig.rule(stoppers("mid.service", "far.service"));
    rig.rule(show_state("mid.service", "inactive"));
    rig.rule(show_state("far.service", "active"));
    rig.expect_refused("far.service");

    let mut rig = Rig::new("dependent-inactive");
    rig.rule(stoppers(MOUNT, "other.service local-fs.target"));
    rig.rule(show_state("other.service", "inactive"));
    let out = rig.apply();
    assert_eq!(out.code, 0, "{}", out.text);
}

/// A bind mount of the same filesystem keeps it alive when the mount unit stops, so prjquota would
/// not take; a mount below the scratch directory is unmounted with it and not mounted again.
#[test]
fn a_bind_mount_or_a_mount_below_the_scratch_directory_is_refused() {
    let mut rig = Rig::new("bind");
    rig.rule(rule(
        "findmnt",
        TARGETS,
        0,
        &format!("{SCRATCH}\\n/srv/elsewhere\\n"),
        &[],
    ));
    rig.expect_refused("/srv/elsewhere");

    let mut rig = Rig::new("child");
    rig.rule(rule(
        "findmnt",
        &format!("-n -R -o TARGET {SCRATCH}"),
        0,
        &format!("{SCRATCH}\\n{SCRATCH}/oci\\n"),
        &[],
    ));
    rig.expect_refused(&format!("{SCRATCH}/oci"));

    let mut rig = Rig::new("no-source");
    rig.rule(rule(
        "findmnt",
        &format!("-n -o SOURCE --mountpoint {SCRATCH}"),
        0,
        "",
        &[],
    ));
    rig.expect_refused("source");
}

/// Only a clean "nothing holds it" lets the filesystem be unmounted: a `fuser` that fails, or
/// prints a complaint, is not "not held".
#[test]
fn a_fuser_that_fails_is_not_taken_for_a_mount_nothing_holds() {
    let mut rig = Rig::new("fuser-fails");
    rig.rule(rule("fuser", &format!("-m -- {SCRATCH}"), 2, "", &[]));
    rig.expect_refused("cannot tell");

    let mut rig = Rig::new("fuser-complains");
    rig.rule(rule(
        "fuser",
        &format!("-m -- {SCRATCH}"),
        1,
        "fuser: cannot stat\\n",
        &[],
    ));
    rig.expect_refused("cannot tell");
}

// --- the mount unit ---------------------------------------------------------------------------

/// A drop-in directory can override `Options=` and is not read by the edit; a line continued with
/// a backslash is not one the edit can extend: both refused, as are several `Options=` lines and a
/// filesystem that is not XFS.
#[test]
fn a_mount_unit_the_edit_cannot_be_trusted_with_is_refused() {
    let rig = Rig::new("drop-in");
    std::fs::create_dir(rig.path(&format!("{}.d", unit_path()))).expect("directory");
    rig.expect_refused("drop-in");

    let rig = Rig::new("continued");
    rig.write(
        &unit_path(),
        &mount_unit("loop,\\").replace("loop,\\\n", "loop,\\\nnoatime\n"),
    );
    rig.expect_refused("continued");

    let rig = Rig::new("two-options");
    rig.write(
        &unit_path(),
        &mount_unit("loop").replace("Options=loop\n", "Options=loop\nOptions=noatime\n"),
    );
    rig.expect_refused("2 Options=");

    let rig = Rig::new("not-xfs");
    rig.write(
        &unit_path(),
        &mount_unit("loop").replace("Type=xfs", "Type=ext4"),
    );
    rig.expect_refused("Type=ext4");
}

/// The options systemd will use once it has read the edited unit are checked, not assumed: where
/// the effective `Options=` still lacks prjquota (a file the edit missed), the run stops before
/// the filesystem is unmounted.
#[test]
fn an_effective_options_line_without_prjquota_stops_the_run_before_the_unmount() {
    let mut rig = Rig::new("effective");
    rig.rule(rule(
        "systemctl",
        &format!("show -p Options --value -- {MOUNT}"),
        0,
        "loop\\n",
        &[],
    ));
    let out = rig.apply();
    assert_eq!(out.code, 1, "{}", out.text);
    assert!(out.text.contains("effective"), "{}", out.text);
    let calls = rig.calls();
    assert!(
        !calls.contains(&format!("systemctl stop {MOUNT}")),
        "{calls:?}"
    );
    assert!(
        !rig.path("var/lib/js-agent").exists(),
        "the steps after it ran"
    );
}

// --- what the script may act on ---------------------------------------------------------------

/// A fake root that runs as root would still reach the real `systemctl`, `useradd` and the rest of
/// the host: an apply with `--root` refuses when the process really is root.
#[test]
fn an_apply_with_a_root_option_refuses_to_run_as_root() {
    let mut rig = Rig::new("root-and-root");
    rig.rule(rule("id", "-ur", 0, "0\\n", &[]));
    rig.expect_refused("--root");
}

/// The pin is of the bytes that are installed: a unit file that is one thing when it is read and
/// another when it is hashed again is refused (here the second read would see the pinned file).
#[test]
fn the_installed_bytes_are_the_bytes_that_were_hashed() {
    let mut rig = Rig::new("toctou");
    let good = read(UNIT);
    let copy = rig.root.parent().expect("scratch").join("unit-copy");
    std::fs::write(&copy, format!("{good}# not the pinned file\n")).expect("copy");
    rig.rule(rule(
        "sha256sum",
        &format!("-- {}", copy.display()),
        0,
        &format!("!{}", real("sha256sum").display()),
        &[format!("write={}={}", copy.display(), esc(&good))],
    ));
    let mut args = rig.apply_args();
    let at = args.iter().position(|a| a == "--unit").expect("flag") + 1;
    args[at] = copy.display().to_string();
    let before = rig.snapshot();
    let out = rig.run(&args.iter().map(String::as_str).collect::<Vec<_>>());
    assert_eq!(out.code, 2, "{}", out.text);
    assert!(out.text.contains("sha256"), "{}", out.text);
    assert_untouched(&rig, &before);
    assert!(
        !rig.path("etc/systemd/system/js-agent-crun.service")
            .exists()
    );
}

/// An existing account is the job user only if it is a system account that cannot log in; a
/// group of the same name makes `useradd --user-group` fail after the plan, so it is refused in it.
#[test]
fn an_existing_user_or_group_that_is_not_the_job_user_is_refused() {
    let rig = Rig::new("user-shell");
    rig.write(
        "etc/passwd",
        "root:x:0:0:root:/root:/bin/bash\njsjob:x:998:998::/home/jsjob:/bin/bash\n",
    );
    rig.expect_refused("shell");

    let rig = Rig::new("user-uid");
    rig.write(
        "etc/passwd",
        "root:x:0:0:root:/root:/bin/bash\njsjob:x:1000:1000::/nonexistent:/usr/sbin/nologin\n",
    );
    rig.expect_refused("uid");

    let rig = Rig::new("group-only");
    rig.write("etc/group", "root:x:0:\njsjob:x:555:\n");
    rig.expect_refused("group");

    let rig = Rig::new("user-ok");
    rig.write(
        "etc/passwd",
        "root:x:0:0:root:/root:/bin/bash\njsjob:x:998:998::/nonexistent:/bin/false\n",
    );
    rig.write("etc/group", "root:x:0:\njsjob:x:998:\n");
    let out = rig.apply();
    assert_eq!(out.code, 0, "{}", out.text);
}

// --- the record --------------------------------------------------------------------------------

/// A run that ends in a failure writes that into the manifest (the exit status and the step it was
/// in), so what an apply did and where it stopped are in one file; a run that succeeds does not.
#[test]
fn a_failed_apply_ends_its_manifest_with_an_incomplete_line() {
    let mut rig = Rig::new("incomplete");
    rig.rule(rule("systemctl", &format!("start {MOUNT}"), 1, "", &[]));
    let out = rig.apply();
    assert_eq!(out.code, 1, "{}", out.text);
    let manifest = rig.text("var/lib/js-crun-host-setup/manifest");
    let last = manifest.lines().last().expect("a line");
    assert!(last.starts_with("incomplete\t1\t"), "{manifest}");

    let rig = Rig::new("complete");
    assert_eq!(rig.apply().code, 0);
    assert!(
        !rig.text("var/lib/js-crun-host-setup/manifest")
            .contains("incomplete")
    );
}

/// The state directory holds the record the script trusts: one that is not 0700 and the caller's
/// is refused before anything is written.
#[test]
fn a_state_directory_that_is_open_to_others_is_refused() {
    let rig = Rig::new("state-dir");
    let dir = rig.path("var/lib/js-crun-host-setup");
    std::fs::create_dir(&dir).expect("directory");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).expect("mode");
    rig.expect_refused("state directory");
}

// --- the refusals of the main path, one each ---------------------------------------------------

/// A directory that is a link, an existing unit that differs, and a subid block that overlaps
/// another range are each refused before any change.
#[test]
fn a_linked_directory_a_different_unit_and_an_overlapping_block_are_refused() {
    let rig = Rig::new("linked-dir");
    std::fs::create_dir_all(rig.path("elsewhere")).expect("directory");
    symlink(rig.path("elsewhere"), rig.path("var/lib/js-agent")).expect("link");
    rig.expect_refused("symbolic link");

    let rig = Rig::new("unit-differs");
    rig.write("etc/systemd/system/js-agent-crun.service", "[Service]\n");
    rig.expect_refused("differs");

    let rig = Rig::new("subid-overlap");
    rig.write("etc/subuid", "other:100000:65536\n");
    let mut args = rig.apply_args();
    args.extend(["--subid-start".to_owned(), "100010".to_owned()]);
    let before = rig.snapshot();
    let out = rig.run(&args.iter().map(String::as_str).collect::<Vec<_>>());
    assert_eq!(out.code, 2, "{}", out.text);
    assert!(out.text.contains("overlaps"), "{}", out.text);
    assert_untouched(&rig, &before);
}

// --- the host checks ---------------------------------------------------------------------------

/// A kernel, systemd or cgroup hierarchy that is too old for the unit, and a `crun` that is not the
/// distribution's or is too old, are each refused before any change.
#[test]
fn a_host_that_is_too_old_or_has_the_wrong_crun_is_refused() {
    let mut rig = Rig::new("old-kernel");
    rig.rule(rule("uname", "-r", 0, "5.4.0-1-generic\\n", &[]));
    rig.expect_refused("older than 5.11");

    let mut rig = Rig::new("old-systemd");
    rig.rule(rule(
        "systemctl",
        "--version",
        0,
        "systemd 253 (253.1)\\n",
        &[],
    ));
    rig.expect_refused("older than 254");

    let rig = Rig::new("cgroup-1");
    std::fs::remove_file(rig.path("sys/fs/cgroup/cgroup.controllers")).expect("remove");
    rig.expect_refused("cgroup version 2");

    let mut rig = Rig::new("crun-not-owned");
    rig.install_crun();
    rig.rule(rule(
        "dpkg-query",
        "-S /usr/bin/crun",
        0,
        "other: /usr/bin/crun\\n",
        &[],
    ));
    rig.expect_refused("not owned by the crun package");

    let mut rig = Rig::new("crun-old");
    rig.install_crun();
    rig.rule(rule("crun", "--version", 0, "crun version 1.13.9\\n", &[]));
    rig.expect_refused("older than 1.14");
}

/// Where unprivileged user namespaces are restricted, the profile for `/usr/bin/crun` must be
/// loaded: absent, the run is refused; installed but not loaded, it is loaded and recorded.
#[test]
fn the_apparmor_profile_for_crun_is_loaded_or_the_run_is_refused() {
    let setup = |name: &str| {
        let rig = Rig::new(name);
        rig.write(
            "proc/sys/kernel/apparmor_restrict_unprivileged_userns",
            "1\n",
        );
        rig.write(
            "sys/kernel/security/apparmor/profiles",
            "snap.x (enforce)\n",
        );
        rig
    };
    setup("apparmor-absent").expect_refused("/etc/apparmor.d/crun");

    let rig = setup("apparmor-installed");
    rig.write(
        "etc/apparmor.d/crun",
        "profile crun /usr/bin/crun flags=(unconfined) {\n  userns,\n}\n",
    );
    let out = rig.apply();
    assert_eq!(out.code, 0, "{}", out.text);
    let want = format!(
        "apparmor_parser -r {}",
        rig.path("etc/apparmor.d/crun").display()
    );
    assert!(rig.calls().contains(&want), "{:?}", rig.calls());
    assert!(
        rig.text("var/lib/js-crun-host-setup/manifest")
            .contains("apparmor-loaded\t")
    );
}

/// The manifest's last line of a failed run names the step that failed, also when it is not a
/// command the script ran (here the check of the unit after the reload), and not the last command
/// that went well.
#[test]
fn the_incomplete_line_names_the_step_that_failed() {
    let mut rig = Rig::new("incomplete-step");
    rig.rule(rule("systemd-analyze", "verify *", 1, "", &[]));
    let out = rig.apply();
    assert_eq!(out.code, 1, "{}", out.text);
    let manifest = rig.text("var/lib/js-crun-host-setup/manifest");
    let last = manifest.lines().last().expect("a line");
    assert_eq!(last, "incomplete\t1\tstep_reload", "{manifest}");
}

/// A state directory or a manifest that is a link, an existing user with uid 0 and an effective
/// `Options=` that only looks like it has the option are each refused or stopped.
#[test]
fn links_a_root_account_and_a_lookalike_option_are_not_trusted() {
    let rig = Rig::new("state-dir-link");
    std::fs::create_dir_all(rig.path("elsewhere")).expect("directory");
    std::fs::set_permissions(
        rig.path("elsewhere"),
        std::fs::Permissions::from_mode(0o700),
    )
    .expect("mode");
    symlink(
        rig.path("elsewhere"),
        rig.path("var/lib/js-crun-host-setup"),
    )
    .expect("link");
    rig.expect_refused("state directory");

    let rig = Rig::new("manifest-link");
    let dir = rig.path("var/lib/js-crun-host-setup");
    std::fs::create_dir(&dir).expect("directory");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).expect("mode");
    symlink(rig.path("elsewhere"), dir.join("manifest")).expect("link");
    rig.expect_refused("manifest");

    let rig = Rig::new("user-uid-0");
    rig.write(
        "etc/passwd",
        "root:x:0:0:root:/root:/bin/bash\njsjob:x:0:0::/nonexistent:/usr/sbin/nologin\n",
    );
    rig.expect_refused("never runs as root");

    let mut rig = Rig::new("noprjquota");
    rig.rule(rule(
        "systemctl",
        &format!("show -p Options --value -- {MOUNT}"),
        0,
        "loop,noprjquota\\n",
        &[],
    ));
    let out = rig.apply();
    assert_eq!(out.code, 1, "{}", out.text);
    assert!(out.text.contains("effective"), "{}", out.text);
}
