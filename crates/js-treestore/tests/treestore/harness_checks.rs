//! The privileged tests' harness helpers, checked where CI can run them.

use std::path::Path;

use crate::priv_harness::{inheritable_write_ace, user_command, user_name};

#[test]
fn the_name_of_root_is_root_and_a_number_nobody_has_has_no_name() {
    assert_eq!(user_name(0).as_deref(), Some("root"));
    assert_eq!(user_name(4_000_000_011), None);
}

#[test]
fn the_acl_entry_names_the_user_and_never_a_number() {
    let entry = inheritable_write_ace(0);
    assert_eq!(
        entry,
        "user:root allow write,file_inherit,directory_inherit"
    );
    assert!(!entry.contains('#'), "{entry}");
}

#[test]
#[should_panic(expected = "no user with uid 4000000011")]
fn an_acl_entry_for_a_uid_with_no_name_is_refused() {
    let _ = inheritable_write_ace(4_000_000_011);
}

#[test]
fn a_command_run_as_another_user_starts_in_the_root_directory() {
    let me = rustix::process::geteuid().as_raw();
    let group = rustix::process::getegid().as_raw();
    let mut command = user_command("/bin/pwd", &[], me, group);
    assert_eq!(command.get_current_dir(), Some(Path::new("/")));
    let out = command.output().unwrap();
    assert!(out.status.success());
    assert_eq!(out.stdout, b"/\n");
}

#[test]
fn a_command_passes_its_arguments_and_reads_no_input() {
    let me = rustix::process::geteuid().as_raw();
    let group = rustix::process::getegid().as_raw();
    let mut command = user_command("/bin/echo", &[Path::new("-n"), Path::new("a")], me, group);
    let out = command.output().unwrap();
    assert_eq!(out.stdout, b"a");
}
