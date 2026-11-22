use clap::Parser as _;

use super::*;
use crate::testing::{FakeHost, scratch, trace};

/// A group the test process may give files to, other than `staff` (20): the first of
/// its groups that is not 20, by name and id.
fn my_group() -> (String, u32) {
    let output = |flag: &str| {
        let out = std::process::Command::new("id").arg(flag).output().unwrap();
        String::from_utf8(out.stdout).unwrap()
    };
    let names = output("-Gn");
    let ids = output("-G");
    names
        .split_whitespace()
        .zip(ids.split_whitespace())
        .map(|(name, id)| (name.to_owned(), id.parse().unwrap()))
        .find(|&(_, id)| id != STAFF_GID)
        .expect("a group other than staff")
}

/// The name of the group with id `gid`, from `/etc/group`.
fn group_named(gid: u32) -> Option<String> {
    let text = std::fs::read_to_string("/etc/group").ok()?;
    text.lines().find_map(|line| {
        let fields: Vec<&str> = line.split(':').collect();
        (fields.len() > 2 && fields[2] == gid.to_string()).then(|| fields[0].to_owned())
    })
}

/// Mode 0700: private whatever the test's umask.
fn private(path: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

fn args(dir: &Path, group: &str) -> Args {
    Args::parse_from([
        "kbf-mac-session",
        "--socket",
        &dir.join("run/socket").to_string_lossy(),
        "--socket-group",
        group,
        "--state-dir",
        &dir.join("state").to_string_lossy(),
        "--uid-range",
        "700-709",
        "--pool-size",
        "10",
        "--homes",
        &dir.to_string_lossy(),
        "--daemon-requirement",
        "cdhash H\"00\"",
    ])
}

fn prepared(args: &Args) -> Result<Ready, String> {
    trace();
    prepare(
        args,
        Box::new(FakeHost::default()),
        crate::sweep::SweepPlan::macos_schedules(),
        crate::sweep::SweepPlan::macos(),
        crate::scan::ScanFolder::macos(),
    )
}

#[test]
fn the_flags_default_to_the_design() {
    let args = Args::parse_from(["kbf-mac-session", "--daemon-requirement", "r"]);
    assert_eq!(
        args.socket,
        PathBuf::from("/var/run/kbf-mac-session/socket")
    );
    assert_eq!(args.socket_group, "_kbf");
    assert_eq!(args.uid_range.to_string(), "600-699");
    assert_eq!(args.pool_size, crate::lease::PoolSize::new(100).unwrap());
    assert_eq!(args.lease_gid, STAFF_GID);
    assert_eq!(args.homes, PathBuf::from("/Users"));
    assert_eq!(args.images_dir, PathBuf::from("/var/db/kbf-vm/images"));
    assert_eq!(args.staging_dir, PathBuf::from("/var/db/kbf-vm/staging"));
    assert_eq!(args.vmm, PathBuf::from("/usr/local/kbf/current/kbf-vmm"));
    assert_eq!(
        args.guest_binary,
        PathBuf::from("/usr/local/kbf/current/kbf-guest")
    );
    assert_eq!(args.vm_user, None);
    assert_eq!(args.kcpassword, PathBuf::from("/etc/kcpassword"));
    assert_eq!(
        args.loginwindow,
        "/Library/Preferences/com.apple.loginwindow"
    );
    assert!(Args::try_parse_from(["kbf-mac-session"]).is_err());
    assert!(
        Args::try_parse_from([
            "kbf-mac-session",
            "--daemon-requirement",
            "r",
            "--uid-range",
            "1-2"
        ])
        .is_err()
    );
    // No key file can make administrators: the flag that named one is gone, so a start
    // that still passes it fails before anything is served.
    let error = Args::try_parse_from([
        "kbf-mac-session",
        "--daemon-requirement",
        "r",
        "--grant-keys",
        "/etc/kbf/grant-keys",
    ])
    .err()
    .unwrap();
    assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument);
}

/// Catches: image flags that do not reach the helper's settings (the verbs would act in
/// the default directory with the default programs whatever the Mac is set up with).
#[test]
fn the_image_flags_reach_the_settings() {
    let args = Args::parse_from([
        "kbf-mac-session",
        "--daemon-requirement",
        "r",
        "--images-dir",
        "/i",
        "--staging-dir",
        "/s",
        "--vmm",
        "/k/kbf-vmm",
        "--guest-binary",
        "/k/kbf-guest",
    ]);
    let image = args.image_settings();
    assert_eq!(image.dir, PathBuf::from("/i"));
    assert_eq!(image.staging, PathBuf::from("/s"));
    assert_eq!(image.vmm, PathBuf::from("/k/kbf-vmm"));
    assert_eq!(image.guest, PathBuf::from("/k/kbf-guest"));
}

/// Catches: a VM user flag that takes a lease user's name (the boot reset deletes lease
/// users), root or the daemon's account, or ignores the name it was given.
#[test]
fn the_vm_user_flag_takes_a_name_no_lease_user_can_have() {
    let parse = |name: &str| {
        Args::try_parse_from([
            "kbf-mac-session",
            "--daemon-requirement",
            "r",
            "--vm-user",
            name,
        ])
    };
    let args = parse("kbf-vm").unwrap();
    assert_eq!(args.vm_user.unwrap().name(), "kbf-vm");
    for bad in ["kbf-lease-1-1", "root", "_kbf", "", "Kbf vm"] {
        assert!(parse(bad).is_err(), "{bad:?} was accepted");
    }
}

/// Catches: a start-up that skips the state directory's privacy or the socket's
/// group.
#[test]
fn prepare_makes_private_state_and_binds_the_socket() {
    let dir = scratch("start-ok");
    let (group, gid) = my_group();
    let args = args(&dir, &group);
    let ready = prepared(&args).unwrap();
    drop(ready.listener);
    let state = std::fs::metadata(dir.join("state")).unwrap();
    assert_eq!(
        std::os::unix::fs::PermissionsExt::mode(&state.permissions()) & 0o777,
        0o700
    );
    assert!(dir.join("state/ledger").is_file());
    let socket = std::fs::metadata(dir.join("run/socket")).unwrap();
    assert_eq!(std::os::unix::fs::MetadataExt::gid(&socket), gid);
    // Started again over the same state.
    prepared(&args).unwrap();
}

/// Catches: the socket's group allowed to be `staff` (S4.3: every macOS user is in
/// it) or the lease users' own group, which would let lease users reach the helper.
#[test]
fn the_socket_group_must_be_dedicated() {
    let dir = scratch("start-group");
    let (group, gid) = my_group();
    let mut same = args(&dir, &group);
    same.lease_gid = gid;
    let error = prepared(&same).err().unwrap();
    assert!(error.contains("must be the daemon's own"), "{error}");
    if let Some(staff) = group_named(STAFF_GID) {
        let error = prepared(&args(&dir, &staff)).err().unwrap();
        assert!(error.contains("must be the daemon's own"), "{error}");
    }
    let error = prepared(&args(&dir, "no-such-group-kbf")).err().unwrap();
    assert!(error.contains("no group named"), "{error}");
    let error = prepared(&args(&dir, "nul\0group")).err().unwrap();
    assert!(error.contains("nul"), "{error}");
}

/// Catches: a start over a state directory that is missing, linked or holds a ledger
/// that does not parse, or a socket that cannot be bound, taken as a start.
#[test]
fn bad_state_or_socket_stop_the_start() {
    let dir = scratch("start-bad");
    let (group, _) = my_group();
    let good = args(&dir, &group);
    std::fs::write(dir.join("a-file"), "").unwrap();

    let mut orphan = good.clone();
    orphan.state_dir = dir.join("no/such/state");
    assert!(prepared(&orphan).err().unwrap().contains("no/such/state"));
    std::fs::create_dir(dir.join("real-state")).unwrap();
    std::os::unix::fs::symlink(dir.join("real-state"), dir.join("linked-state")).unwrap();
    let mut linked = good.clone();
    linked.state_dir = dir.join("linked-state");
    assert!(prepared(&linked).is_err());

    let mut corrupt = good.clone();
    corrupt.state_dir = dir.join("corrupt");
    std::fs::create_dir(dir.join("corrupt")).unwrap();
    std::fs::write(dir.join("corrupt/ledger"), "nonsense\n").unwrap();
    private(&dir.join("corrupt"));
    private(&dir.join("corrupt/ledger"));
    assert!(prepared(&corrupt).err().unwrap().contains("unknown record"));

    let mut unbindable = good;
    unbindable.socket = dir.join("a-file/socket");
    assert!(
        prepared(&unbindable)
            .err()
            .unwrap()
            .contains("a-file/socket")
    );
}

/// Catches: trusted files read with no check of who may write them (S4.3): a state
/// directory, ledger or file the helper reads as its own that another user owns, or that
/// its group or others may write, or such a file read through a link or that is not a
/// regular file. Any would let someone else reuse a lease id, hand out a user twice or
/// write the leak scan's baseline.
#[test]
fn trusted_files_must_be_the_helpers_own_and_private() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = scratch("start-trusted");
    let (group, _) = my_group();
    let good = args(&dir, &group);
    let mode = |path: &Path, mode: u32| {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    };

    // A file the helper reads as its own (the leak scan's baseline and window): never
    // through a link, a regular file, writable by no one else.
    let file = dir.join("trusted");
    std::fs::write(&file, "x\n").unwrap();
    mode(&file, 0o644);
    assert_eq!(read_trusted(&file).unwrap(), "x\n");
    mode(&file, 0o664);
    let error = read_trusted(&file).unwrap_err().to_string();
    assert_eq!(error, "writable by its group or others (mode 664)");
    mode(&file, 0o646);
    assert!(
        read_trusted(&file)
            .unwrap_err()
            .to_string()
            .contains("writable by")
    );
    mode(&file, 0o600);
    std::os::unix::fs::symlink(&file, dir.join("linked")).unwrap();
    assert!(read_trusted(&dir.join("linked")).is_err());
    let error = read_trusted(&dir).unwrap_err().to_string();
    assert_eq!(error, "not a regular file");

    // State: the directory and the ledger in it.
    prepared(&good).unwrap();
    let state = dir.join("state");
    mode(&state, 0o770);
    let error = prepared(&good).err().unwrap();
    assert!(error.contains("state: writable by"), "{error}");
    mode(&state, 0o700);
    let ledger = state.join("ledger");
    mode(&ledger, 0o622);
    let error = prepared(&good).err().unwrap();
    assert!(error.contains("ledger: writable by"), "{error}");
    mode(&ledger, 0o600);
    prepared(&good).unwrap();

    // Another user's: root's own files, seen by a test that is not root.
    if rustix::process::geteuid().as_raw() != 0 {
        let error = read_trusted(Path::new("/etc/passwd"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("owned by uid 0"), "{error}");
        let mut foreign_state = good;
        foreign_state.state_dir = PathBuf::from("/");
        let error = prepared(&foreign_state).err().unwrap();
        assert!(error.contains("owned by uid 0"), "{error}");
    }
}

#[cfg(not(target_os = "macos"))]
#[test]
fn main_serves_only_on_macos() {
    let dir = scratch("start-main");
    assert_eq!(main(&args(&dir, "g")), ExitCode::from(2));
}

/// Catches: a pool started that holds more users than the uid range has uids (two slots
/// would share a uid, or one would have none), before anything is made.
#[test]
fn a_pool_larger_than_the_uid_range_stops_the_start() {
    let dir = scratch("start-pool-fits");
    let mut args = args(&dir, &my_group().0);
    args.pool_size = crate::lease::PoolSize::new(11).unwrap();
    assert_eq!(
        prepared(&args).err(),
        Some("a pool of 11 users does not fit the uid range 700-709, which holds 10".to_owned())
    );
    assert!(!dir.join("state").exists());
}
