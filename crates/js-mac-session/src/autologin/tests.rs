use std::os::unix::fs::PermissionsExt as _;

use super::*;
use crate::testing::{FakeHost, act, scratch};

/// Catches: a file of the state directory written through a link planted at its path (root
/// would write where the link points), left readable by others, or a stale temporary file
/// that stops the write.
#[test]
fn a_file_is_written_whole_over_a_planted_link_and_is_private() {
    let dir = scratch("write-whole");
    let path = dir.join("record");
    let target = dir.join("target");
    std::fs::write(&target, "keep").unwrap();
    std::os::unix::fs::symlink(&target, &path).unwrap();
    std::fs::write(dir.join("record.new"), "stale").unwrap();
    write_whole(&path, b"text").unwrap();
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep");
    let meta = std::fs::symlink_metadata(&path).unwrap();
    assert!(meta.file_type().is_file());
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    assert_eq!(std::fs::read(&path).unwrap(), b"text");
    assert!(!dir.join("record.new").exists());
}

fn answer(host: &FakeHost, text: &str, success: bool, stdout: &str) {
    host.state().answers.insert(
        text.to_owned(),
        CommandOutput {
            success,
            stdout: stdout.to_owned(),
            stderr: "said no".to_owned(),
        },
    );
}

/// Catches: a failed `defaults delete` taken as cleared while the key is still set, or
/// a missing key (which `defaults delete` reports as a failure) taken as an error.
#[test]
fn clearing_auto_login_checks_a_failed_delete_with_a_read() {
    let dir = scratch("clear");
    let paths = LoginPaths::under(&dir);
    let domain = paths.loginwindow.clone();
    let host = FakeHost::default();
    std::fs::write(&paths.kcpassword, "x").unwrap();
    assert_eq!(clear_auto_login(&host, &paths), Ok(true));
    assert!(!paths.kcpassword.exists());
    // Not set, nothing left: a failed delete, and a read that finds nothing.
    answer(
        &host,
        &format!("{DEFAULTS} delete {domain} autoLoginUser"),
        false,
        "",
    );
    answer(
        &host,
        &format!("{DEFAULTS} read {domain} autoLoginUser"),
        false,
        "",
    );
    assert_eq!(clear_auto_login(&host, &paths), Ok(false));
    // Still set after a failed delete.
    answer(
        &host,
        &format!("{DEFAULTS} read {domain} autoLoginUser"),
        true,
        "kbf-lease-1-1\n",
    );
    let error = clear_auto_login(&host, &paths).unwrap_err();
    assert!(error.contains("said no"), "{error}");
    host.state().fail.insert("command");
    let error = clear_auto_login(&host, &paths).unwrap_err();
    assert!(error.contains("running /usr/bin/defaults"), "{error}");
}

/// Asserts that a failed `defaults read` reads as an unset key in the words of each macOS the
/// pool runs on (the texts are what `defaults read` printed on macOS 26.4, 26.6 and
/// 27.0.1), and that a failure in neither form does not. Catches a check that knows only
/// macOS 26's wording, which on macOS 27 would make the leak scan fail its auto-login item on
/// every Mac with no auto-login set.
#[test]
fn an_unset_auto_login_user_reads_as_unset_on_macos_26_and_27() {
    let unset = [
        "2026-11-20 21:29:13.385 defaults[84941:2779365] \nThe domain/default pair of \
         (/Library/Preferences/com.apple.loginwindow, autoLoginUser) does not exist\n",
        "Error: Could not find key 'autoLoginUser' in domain 'com.apple.loginwindow'.\n",
    ];
    for stderr in unset {
        assert!(key_absent(stderr), "{stderr}");
    }
    let other = "Error: Could not read domain 'com.apple.loginwindow': not permitted.\n";
    assert!(!key_absent(other));
}

/// Catches the real runner losing or swapping stdout and stderr, taking a failed exit
/// for success, or passing its environment on (a `HOME` or `PATH` of the helper's
/// would reach `defaults` or `launchctl`).
#[test]
fn run_command_reports_output_and_clears_the_environment() {
    let output = run_command(
        &act(),
        &[
            "echo", "out", ";", "echo", "--stderr", "err", ";", "env", "HOME", "unset", ";",
            "exit", "3",
        ],
    )
    .unwrap();
    assert_eq!(
        output,
        CommandOutput {
            success: false,
            stdout: "out\nunset\n".to_owned(),
            stderr: "err\n".to_owned(),
        }
    );
    assert!(run_command(&act(), &["exit", "0"]).unwrap().success);
    assert!(run_command("/nonexistent/program", &[]).is_err());
}

/// Catches: a failure to write or remove a file of the state directory taken as success
/// (each path here is a directory, which no file operation may treat as the file), and a
/// relative path's directory not synced.
#[test]
fn file_errors_are_reported() {
    let dir = scratch("file-errors");
    let taken = dir.join("taken");
    std::fs::create_dir_all(taken.join("inside")).unwrap();
    assert!(remove(&taken).is_err(), "a directory removed as a file");
    std::fs::create_dir_all(dir.join("record.new/inside")).unwrap();
    assert!(write_whole(&dir.join("record"), b"").is_err());
    let paths = LoginPaths {
        kcpassword: taken.clone(),
        ..LoginPaths::under(&dir)
    };
    let error = clear_auto_login(&FakeHost::default(), &paths).unwrap_err();
    assert!(error.contains("removing"), "{error}");
    // A bare file name: its directory is the current one.
    assert!(!remove(Path::new("kbf-mac-session-no-such-file")).unwrap());
    assert!(sync_parent(Path::new("kbf-mac-session-no-such-file")).is_ok());
}
