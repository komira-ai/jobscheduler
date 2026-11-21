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

/// `js-agent crun-start-refused` is answered by `main_with` before the signal handlers are
/// installed: after a stop whose result is not `resources` it reads nothing, says nothing and
/// exits 0, as it does after every plain stop of the unit. Catches the verb falling through to
/// the agent's own flags (exit 2, `--result` unknown) or installing the handlers.
#[test]
fn main_with_answers_the_start_refused_verb_before_it_installs_the_signal_handlers() {
    let installed = std::cell::Cell::new(false);
    let install = || {
        installed.set(true);
        Err(std::io::Error::other("no signal handlers for the verb"))
    };
    let args: Vec<std::ffi::OsString> = ["js-agent", "crun-start-refused", "--result", "success"]
        .map(Into::into)
        .to_vec();
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    let status = js_agent::main_with(&install, args, &mut stdout, &mut stderr);
    assert!(!installed.get(), "the verb installed the signal handlers");
    assert_eq!(
        format!("{status:?}"),
        format!("{:?}", std::process::ExitCode::SUCCESS)
    );
    assert_eq!((stdout, stderr), (Vec::new(), Vec::new()));
}
