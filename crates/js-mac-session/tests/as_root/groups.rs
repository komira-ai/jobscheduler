//! The root tests of the groups a started process holds: exactly the lease user's own.

use super::*;

/// Catches: a job that holds group 0 (`wheel`) or `admin` (80) it was not given, which
/// `setgroups(0, NULL)` leaves on macOS (the kernel reads a count below 1 as 1, with a
/// zero-filled list), so a job reads what only `wheel` may, such as a file `root:wheel`
/// mode 0440 (`/etc/sudoers` is one). The process's kernel list (`getgroups`, which is
/// what the file check uses, not `id -G`, which asks the directory) must be the lease
/// user's own, as the directory lists them.
#[test]
#[ignore = "needs root: run the test binary through `sudo` (see the header of tests/as_root.rs)"]
fn a_run_holds_the_lease_user_s_groups_and_cannot_read_what_only_wheel_may() {
    let (_serial, mac) = mac();
    let dir = scratch_dir("lease-groups", 0o755);
    let wheel_only = format!("{dir}/wheel-only");
    write_root(&wheel_only, "only wheel may read this\n");
    std::os::unix::fs::chown(&wheel_only, Some(0), Some(0)).expect("root:wheel");
    set_mode(&wheel_only, 0o440);
    let lease = mac.create("19.1", None);
    let name = lease.name();
    let client = path_str(&mac.client);
    let held = lease.run(&dir, &[client, "groups"]);
    let listed = held.said().lines().next().unwrap_or_default().to_owned();
    let mut got: Vec<u32> = listed
        .split_whitespace()
        .map(|g| g.parse().expect("a group id"))
        .collect();
    got.sort_unstable();
    got.dedup();
    let mut want: Vec<u32> = succeeds("/usr/bin/id", &["-G", &name])
        .split_whitespace()
        .map(|g| g.parse().expect("a group id"))
        .collect();
    want.sort_unstable();
    want.dedup();
    let said = held.all();
    assert!(
        !got.contains(&0) && !got.contains(&80),
        "the run holds wheel or admin: {said}"
    );
    assert_eq!(got, want, "the run's groups are not the user's: {said}");
    // One process reads the file: the last step, so a refused read ends it with exit 1,
    // and a read prints the file and exits 0.
    let read = lease.run(&dir, &mac.act(&["cat", &wheel_only]));
    assert_eq!(read.said().trim_end(), "exit 1", "{}", read.all());
    lease.end();
}
