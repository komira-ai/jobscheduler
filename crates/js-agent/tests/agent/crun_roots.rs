//! The state root and the scratch root the crun runtime makes itself at start, in place of what
//! the unit's tmpfiles step did: the leaf only, with the modes the check wants, and nothing that
//! exists is changed. (Linux only: only Linux runs containers.)

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use js_agent::crun_host::{Request, make_roots};
use js_testkit::dir::TestDir;

fn request<'a>(state: &'a Path, scratch: &'a Path) -> Request<'a> {
    Request {
        crun: Path::new("/usr/bin/crun"),
        job_user: "jsjob",
        state_root: state,
        scratch_root: scratch,
    }
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o7777
}

/// Missing roots are made with mode 0700 (state) and 0711 (scratch), whatever the umask would
/// have given.
#[test]
fn missing_roots_are_made_with_their_modes() {
    let dir = TestDir::under(std::env::temp_dir(), "crun-roots-make");
    let (state, scratch) = (dir.path().join("state"), dir.path().join("scratch"));
    make_roots(&request(&state, &scratch));
    assert_eq!((mode(&state), mode(&scratch)), (0o700, 0o711));
}

/// A root that exists keeps its mode and its contents, so the check reports a wrong mode with
/// its fix; a root whose parent is missing is not made, so the check reports the miss.
#[test]
fn an_existing_root_is_not_changed_and_a_missing_parent_is_not_made() {
    let dir = TestDir::under(std::env::temp_dir(), "crun-roots-keep");
    let state = dir.path().join("state");
    fs::create_dir(&state).unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o750)).unwrap();
    fs::write(state.join("kept"), "x").unwrap();
    let scratch = dir.path().join("no-parent/scratch");
    make_roots(&request(&state, &scratch));
    assert_eq!(mode(&state), 0o750);
    assert!(state.join("kept").exists());
    assert!(!scratch.exists());
    assert!(!dir.path().join("no-parent").exists());
}
