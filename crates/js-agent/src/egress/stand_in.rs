//! Stand-ins for the `ip`, `nft` and `nsenter` programs, for unit tests that check egress
//! flags and never run a program: an executable, empty file in a test directory of its own,
//! so a test depends on no program of the host.

use std::os::unix::fs::PermissionsExt as _;

use js_testkit::dir::TestDir;

/// An executable stand-in program, removed with its directory when dropped.
pub(crate) struct StandIn {
    dir: TestDir,
}

impl StandIn {
    /// A stand-in in a directory named for `label`.
    pub(crate) fn new(label: &str) -> Self {
        let dir = TestDir::under(std::env::temp_dir(), &format!("egress-stand-in-{label}"));
        let path = dir.path().join("program");
        std::fs::write(&path, b"").expect("a stand-in program");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("an executable stand-in");
        Self { dir }
    }

    /// The stand-in's absolute path.
    pub(crate) fn path(&self) -> String {
        self.dir
            .path()
            .join("program")
            .to_string_lossy()
            .into_owned()
    }
}
