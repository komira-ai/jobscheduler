//! The blocking calls against the fake provider of `js_testkit::mount_provider`, on a real Unix
//! socket. No test waits on a clock to pass: a held call never answers, so its bound ends it.

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::time::Duration;

use js_testkit::dir::TestDir;
use js_testkit::mount_provider::{FakeProvider, Kind, mount_path};

use super::{Provider, beneath};
use crate::provider::CallBounds;

/// Far above any answer of the fake; no test passes by it.
const WAIT: Duration = Duration::from_secs(120);

/// A fake provider in a directory of its own, closed to group and others.
fn start(name: &str) -> (TestDir, FakeProvider) {
    let dir = TestDir::under(std::env::temp_dir(), name);
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
    let fake = FakeProvider::start(dir.path()).unwrap();
    fs::set_permissions(fake.socket(), fs::Permissions::from_mode(0o600)).unwrap();
    (dir, fake)
}

fn provider(fake: &FakeProvider, bounds: CallBounds) -> Provider {
    Provider {
        agent: "mac-1".to_owned(),
        socket: fake.socket().to_path_buf(),
        user: rustix::process::geteuid().as_raw(),
        bounds,
    }
}

/// Bounds with a short retry pause, so a test that retries does not wait for the default one.
fn quick() -> CallBounds {
    CallBounds {
        retry: Duration::from_millis(5),
        ..CallBounds::default()
    }
}

#[test]
fn a_tree_is_prepared_read_only_then_mounted_at_the_path_the_provider_names() {
    let (_dir, fake) = start("blocking-mount");
    let path = provider(&fake, quick())
        .mount("job-1", 0, "cas/l/abc", WAIT)
        .expect("mounted");
    assert_eq!(path, mount_path("mac-1", "js.job-1", 0));
    let prepared = fake.prepared();
    assert_eq!(
        prepared.get(&("mac-1".to_owned(), "js.job-1".to_owned(), 0)),
        Some(&"cas/l/abc".to_owned())
    );
}

#[test]
fn a_mount_the_provider_cannot_serve_yet_is_asked_again() {
    let (_dir, fake) = start("blocking-retry");
    fake.fail_mount(2);
    let path = provider(&fake, quick())
        .mount("job-2", 3, "cas/l/def", WAIT)
        .expect("mounted on the third ask");
    assert_eq!(path, mount_path("mac-1", "js.job-2", 3));
}

#[test]
fn a_refused_tree_ends_the_mount_at_once() {
    let (_dir, fake) = start("blocking-refuse");
    fake.refuse("cas/l/bad");
    let why = provider(&fake, quick())
        .mount("job-3", 0, "cas/l/bad", WAIT)
        .expect_err("refused")
        .to_string();
    assert!(why.starts_with("refused: "), "{why}");
}

#[test]
fn a_provider_that_never_answers_ends_the_mount_when_the_wait_is_over() {
    let (_dir, fake) = start("blocking-held");
    fake.hold(Kind::Mount);
    let bounds = CallBounds {
        mount: Duration::from_millis(20),
        ..quick()
    };
    let why = provider(&fake, bounds)
        .mount("job-4", 0, "cas/l/abc", Duration::ZERO)
        .expect_err("not answered")
        .to_string();
    assert!(
        why.starts_with("not answered in time: Unavailable"),
        "{why}"
    );
    fake.unhold(Kind::Mount);
}

#[test]
fn a_missing_socket_is_unavailable_until_the_wait_is_over() {
    let dir = TestDir::under(std::env::temp_dir(), "blocking-none");
    let none = Provider {
        agent: "mac-1".to_owned(),
        socket: dir.path().join("absent.sock"),
        user: rustix::process::geteuid().as_raw(),
        bounds: quick(),
    };
    let why = none
        .mount("job-5", 0, "cas/l/abc", Duration::ZERO)
        .expect_err("no provider")
        .to_string();
    assert!(
        why.starts_with("not answered in time: Unavailable"),
        "{why}"
    );
    let why = none
        .release("job-5", 0, Duration::ZERO)
        .expect_err("no provider")
        .to_string();
    assert!(
        why.starts_with("not answered in time: Unavailable"),
        "{why}"
    );
}

#[test]
fn a_release_is_asked_until_answered() {
    let (_dir, fake) = start("blocking-release");
    fake.fail_release(1);
    provider(&fake, quick())
        .release("job-6", 0, WAIT)
        .expect("released on the second ask");
}

#[test]
fn a_runtime_error_reads_as_one() {
    let why = super::TreeError::from(std::io::Error::other("no threads")).to_string();
    assert_eq!(why, "a runtime for the provider's calls: no threads");
}

#[test]
fn a_mounted_path_must_be_absolute_and_strictly_beneath_the_root() {
    let root = Path::new("/private/var/tierstore/fs");
    assert!(beneath(
        Path::new("/private/var/tierstore/fs/mount-provider/a/b/0"),
        root
    ));
    for bad in [
        "/private/var/tierstore/fs",
        "/private/var/tierstore/fsx/a",
        "/private/var/other",
        "private/var/tierstore/fs/a",
    ] {
        assert!(!beneath(Path::new(bad), root), "{bad}");
    }
}
