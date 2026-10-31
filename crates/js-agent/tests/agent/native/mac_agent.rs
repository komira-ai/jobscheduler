//! The start-up checks of a native agent against the real `sandbox-exec` of the Mac the
//! tests run on.

use std::path::Path;

use js_agent::native::{self, NativeFlags, NativeSettings};

use super::fake_helper::FakeHelper;
use crate::support::Scratch;

/// Asserts that the Mac's own `sandbox-exec` accepts the profile the runtime builds for the
/// start-up self-test, and that one that does not exist fails it. It catches a profile the
/// operating system rejects, which would fail every job after the hello said so.
#[test]
fn the_real_sandbox_exec_passes_the_self_test() {
    let dir = Scratch::new("native-mac-selftest");
    let fake = FakeHelper::start(&dir);
    let flags = NativeFlags {
        socket: Some(fake.socket()),
        homes: Some(fake.homes()),
        network: None,
        tool_roots: &[],
        deny_read: &[],
    };
    let settings = NativeSettings::from_flags(&flags, &[dir.path("").as_path()])
        .unwrap()
        .unwrap();
    assert_eq!(
        native::self_test(settings.config(), settings.sandbox_exec()),
        Ok(())
    );
    assert!(native::self_test(settings.config(), Path::new("/usr/bin/false")).is_err());
}
