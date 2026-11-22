//! The start-up checks of a native agent against the agent's own start mode on the Mac the
//! tests run on.

use std::path::Path;

use js_agent::native::{self, NativeFlags, NativeSettings, PfFlags};

use super::fake_helper::FakeHelper;
use crate::support::Scratch;

/// Asserts that the agent's own start mode applies the profile the runtime builds for the
/// start-up self-test on this Mac, and that a start program that refuses it fails the test.
/// It catches a profile the operating system rejects, which would fail every job after the
/// hello said so.
#[test]
fn the_agents_own_start_passes_the_self_test() {
    let dir = Scratch::new("native-mac-selftest");
    let fake = FakeHelper::start(&dir);
    let flags = NativeFlags {
        socket: Some(fake.socket()),
        homes: Some(fake.homes()),
        network: None,
        tool_roots: &[],
        deny_read: &[],
        max_file_mib: None,
        max_processes: None,
        pf: PfFlags::default(),
    };
    let settings =
        NativeSettings::from_flags(&flags, &[dir.path("").as_path()], Path::new("/srv/journal"))
            .unwrap()
            .unwrap();
    let agent = settings
        .clone()
        .with_start(Path::new(env!("CARGO_BIN_EXE_js-agent")));
    assert_eq!(native::self_test(agent.config()), Ok(()));
    let refusing = settings.with_start(Path::new("/usr/bin/false"));
    assert!(native::self_test(refusing.config()).is_err());
}
