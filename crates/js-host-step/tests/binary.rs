//! Whether the running program may carry a secret: root's alone.

use std::os::unix::fs::MetadataExt;

use js_host_step::fake::FakeHost;
use js_host_step::{Host, RealHost, check_binary_meta};

/// Catches a binary that others could replace passing as trusted: only a file owned by root
/// and writable by neither its group nor others is accepted; the message says which of the two
/// is wrong.
#[test]
fn only_a_file_root_owns_alone_is_trusted() {
    for mode in [0o755, 0o700, 0o555, 0o4755, 0o000] {
        assert!(check_binary_meta(0, mode).is_ok(), "{mode:o}");
    }
    for mode in [0o775, 0o757, 0o777, 0o772, 0o720] {
        let err = check_binary_meta(0, mode).unwrap_err().to_string();
        assert!(
            err.starts_with("the running binary is writable by its group or others"),
            "{err}"
        );
    }
    for (uid, mode) in [(1000, 0o755), (1000, 0o777), (1, 0o700)] {
        let err = check_binary_meta(uid, mode).unwrap_err().to_string();
        assert_eq!(
            err,
            format!("the running binary is owned by uid {uid}, not root")
        );
    }
}

/// Catches a fake that cannot play both parts: it trusts the binary until a test says
/// otherwise, and then judges the owner and mode it was given.
#[test]
fn the_fake_host_judges_the_binary_it_was_given() {
    let host = FakeHost::new(0);
    assert!(host.check_binary().is_ok());
    host.set_binary(1000, 0o755);
    assert!(host.check_binary().is_err());
    host.set_binary(0, 0o775);
    assert!(host.check_binary().is_err());
    host.set_binary(0, 0o755);
    assert!(host.check_binary().is_ok());
}

/// Catches the real host judging some other file than the running program: its verdict is
/// the verdict on this test binary's own owner and mode.
#[test]
fn the_real_host_judges_the_running_binary() {
    let meta = std::fs::metadata(std::env::current_exe().unwrap()).unwrap();
    let want = check_binary_meta(meta.uid(), meta.mode());
    let got = RealHost::new().check_binary();
    assert_eq!(got.is_ok(), want.is_ok());
    assert_eq!(
        got.map_err(|e| e.to_string()),
        want.map_err(|e| e.to_string())
    );
}
