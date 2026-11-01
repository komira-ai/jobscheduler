//! The real host, under a scratch directory standing in for `/`.

use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

use js_host_step::{Cmd, Host, Kind, RealHost};
use js_testkit::dir::TestDir;

fn scratch(name: &str) -> (TestDir, RealHost) {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), name);
    let host = RealHost::under(dir.path());
    (dir, host)
}

fn me(dir: &TestDir) -> (u32, u32) {
    let m = std::fs::metadata(dir.path()).unwrap();
    (m.uid(), m.gid())
}

/// Catches a write that leaves the mode to the umask or leaves a temporary file behind: the
/// mode is exact, the old content is replaced whole, and only the target remains.
#[test]
fn write_sets_the_exact_mode_and_replaces_whole() {
    let (dir, host) = scratch("write");
    let owner = me(&dir);
    host.mkdir("/etc/x", 0o750, owner).unwrap();
    host.write("/etc/x/f", 0o640, owner, b"first").unwrap();
    host.write("/etc/x/f", 0o600, owner, b"second").unwrap();
    // A temporary file an interrupted run left is cleared.
    std::fs::write(dir.path().join("etc/x/f.js-host.tmp"), b"junk").unwrap();
    host.write("/etc/x/f", 0o644, owner, b"third").unwrap();
    let meta = host.stat("/etc/x/f").unwrap().unwrap();
    assert_eq!((meta.kind, meta.mode, meta.size), (Kind::File, 0o644, 5));
    assert_eq!(host.read("/etc/x/f").unwrap().unwrap(), b"third");
    let names: Vec<_> = std::fs::read_dir(dir.path().join("etc/x"))
        .unwrap()
        .collect();
    assert_eq!(names.len(), 1);
    let dir_meta = host.stat("/etc/x").unwrap().unwrap();
    assert_eq!((dir_meta.kind, dir_meta.mode), (Kind::Dir, 0o750));
}

/// Catches a mkdir that leaves an existing directory's mode alone (a rerun must fix it) and
/// a stat that follows a link or reports a missing path as an error.
#[test]
fn mkdir_fixes_an_existing_mode_and_stat_sees_links_and_gaps() {
    let (dir, host) = scratch("mkdir");
    let owner = me(&dir);
    host.mkdir("/d", 0o777, owner).unwrap();
    host.mkdir("/d", 0o700, owner).unwrap();
    assert_eq!(
        std::fs::metadata(dir.path().join("d"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        0o700
    );
    symlink("d", dir.path().join("l")).unwrap();
    assert_eq!(host.stat("/l").unwrap().unwrap().kind, Kind::Other);
    assert_eq!(host.stat("/nothing").unwrap(), None);
    assert_eq!(host.read("/nothing").unwrap(), None);
    host.remove("/nothing").unwrap();
    host.write("/d/f", 0o600, owner, b"x").unwrap();
    host.remove("/d/f").unwrap();
    assert_eq!(host.stat("/d/f").unwrap(), None);
}

/// Catches errors that are swallowed: a write into a missing directory fails, a read of a
/// directory fails, and a path the system rejects fails rather than reading as missing.
#[test]
fn errors_other_than_not_found_are_returned() {
    let (dir, host) = scratch("errors");
    let owner = me(&dir);
    assert!(host.write("/missing/f", 0o600, owner, b"x").is_err());
    host.mkdir("/d", 0o700, owner).unwrap();
    assert!(host.read("/d").is_err());
    assert!(host.stat("/d/../..\0x").is_err());
}

/// Catches a runner that goes through a shell, passes the environment on, or reports a
/// failing exit as success; and a missing program that is not an error.
#[test]
fn run_executes_without_a_shell_or_environment() {
    let (_dir, host) = scratch("run");
    let said = host
        .run(&Cmd::new("/bin/echo").arg("a  b").arg("$HOME"))
        .unwrap();
    assert_eq!((said.code, said.stdout.as_str()), (0, "a  b $HOME\n"));
    let env = host.run(&Cmd::new("/usr/bin/env")).unwrap();
    assert_eq!(env.stdout, "");
    let failed = host.run(&Cmd::new("/bin/false")).unwrap();
    assert_eq!(failed.code, 1);
    assert!(host.run(&Cmd::new("/nonexistent/program")).is_err());
    // A signal ends the program: the code is -1.
    let killed = host
        .run(&Cmd::new("/bin/sh").arg("-c").arg("kill -9 $$"))
        .unwrap();
    assert_eq!(killed.code, -1);
    assert_eq!(host.euid(), std::fs::metadata("/proc/self").unwrap().uid());
    assert_eq!(host.egid(), std::fs::metadata("/proc/self").unwrap().gid());
    assert_eq!(
        RealHost::default().stat("/").unwrap().unwrap().kind,
        Kind::Dir
    );
}
