//! The package's postinstall over a volume made in a directory and a `launchctl` that records
//! what it is asked: the release link is made once and never moved, a plist someone edited is
//! kept, a daemon is loaded only when it is not loaded, and nothing is ever stopped.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use js_mac_provision::daemon::{AGENT_LABEL, SESSION_LABEL};
use js_mac_provision::postinstall::{Error, Launchctl, SystemLaunchctl, run};
use js_testkit::dir::TestDir;

const VERSION: &str = "1.2.3";

#[derive(Default)]
struct Fake {
    loaded: BTreeSet<String>,
    refuse: bool,
    calls: RefCell<Vec<String>>,
}

impl Launchctl for Fake {
    fn is_loaded(&self, label: &str) -> io::Result<bool> {
        self.calls.borrow_mut().push(format!("is_loaded {label}"));
        Ok(self.loaded.contains(label))
    }

    fn bootstrap(&self, plist: &Path) -> io::Result<()> {
        self.calls
            .borrow_mut()
            .push(format!("bootstrap {}", plist.display()));
        if self.refuse {
            return Err(io::Error::other("Bootstrap failed: 5"));
        }
        Ok(())
    }
}

/// A volume with the release and the plists the package's payload holds.
fn volume(name: &str) -> (TestDir, PathBuf) {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), name);
    let root = dir.path().to_path_buf();
    std::fs::create_dir_all(root.join(format!("usr/local/js-agent/releases/{VERSION}"))).unwrap();
    std::fs::create_dir_all(root.join("usr/local/js-agent/link")).unwrap();
    std::fs::create_dir_all(root.join("usr/local/js-agent/share")).unwrap();
    std::fs::create_dir_all(root.join("Library/LaunchDaemons")).unwrap();
    for label in [AGENT_LABEL, SESSION_LABEL] {
        std::fs::write(
            root.join(format!("usr/local/js-agent/share/{label}.plist")),
            format!("template of {label}\n"),
        )
        .unwrap();
    }
    (dir, root)
}

fn installed(root: &Path, label: &str) -> PathBuf {
    root.join(format!("Library/LaunchDaemons/{label}.plist"))
}

fn link(root: &Path) -> PathBuf {
    root.join("usr/local/js-agent/link/current")
}

#[test]
fn a_first_install_makes_the_link_installs_both_plists_and_loads_the_helper_then_the_agent() {
    let (_dir, root) = volume("pi-first");
    let fake = Fake::default();
    let lines = run(&root, VERSION, Some(&fake)).unwrap();
    assert_eq!(
        lines,
        [
            "linked /usr/local/js-agent/link/current to /usr/local/js-agent/releases/1.2.3",
            "installed /Library/LaunchDaemons/com.komira.js-mac-session.plist",
            "loaded com.komira.js-mac-session",
            "installed /Library/LaunchDaemons/com.komira.js-agent.plist",
            "loaded com.komira.js-agent",
        ]
    );
    assert_eq!(
        std::fs::read_link(link(&root)).unwrap(),
        Path::new("/usr/local/js-agent/releases/1.2.3")
    );
    for label in [AGENT_LABEL, SESSION_LABEL] {
        let plist = installed(&root, label);
        assert_eq!(
            std::fs::read_to_string(&plist).unwrap(),
            format!("template of {label}\n")
        );
        let mode = std::fs::metadata(&plist).unwrap().permissions().mode();
        assert_eq!(mode & 0o7777, 0o644);
    }
    assert_eq!(
        *fake.calls.borrow(),
        [
            "is_loaded com.komira.js-mac-session".to_owned(),
            format!("bootstrap {}", installed(&root, SESSION_LABEL).display()),
            "is_loaded com.komira.js-agent".to_owned(),
            format!("bootstrap {}", installed(&root, AGENT_LABEL).display()),
        ]
    );
}

#[test]
fn a_second_run_changes_nothing_and_loads_nothing_that_is_loaded() {
    let (_dir, root) = volume("pi-second");
    run(&root, VERSION, Some(&Fake::default())).unwrap();
    let fake = Fake {
        loaded: [AGENT_LABEL, SESSION_LABEL]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        ..Fake::default()
    };
    let lines = run(&root, VERSION, Some(&fake)).unwrap();
    assert_eq!(
        lines,
        [
            "kept the release link /usr/local/js-agent/link/current",
            "kept /Library/LaunchDaemons/com.komira.js-mac-session.plist, which exists",
            "com.komira.js-mac-session is loaded already",
            "kept /Library/LaunchDaemons/com.komira.js-agent.plist, which exists",
            "com.komira.js-agent is loaded already",
        ]
    );
    let calls = fake.calls.borrow();
    assert!(
        calls.iter().all(|call| call.starts_with("is_loaded ")),
        "{calls:?}"
    );
}

#[test]
fn a_link_the_update_hook_moved_is_left_where_it_points() {
    let (_dir, root) = volume("pi-link-moved");
    std::os::unix::fs::symlink("/usr/local/js-agent/releases/1.1.0", link(&root)).unwrap();
    run(&root, VERSION, Some(&Fake::default())).unwrap();
    assert_eq!(
        std::fs::read_link(link(&root)).unwrap(),
        Path::new("/usr/local/js-agent/releases/1.1.0")
    );
}

#[test]
fn a_link_to_a_release_that_is_gone_is_still_a_link_and_is_left() {
    let (_dir, root) = volume("pi-link-dangling");
    std::os::unix::fs::symlink("/usr/local/js-agent/releases/0.9.0", link(&root)).unwrap();
    let lines = run(&root, VERSION, Some(&Fake::default())).unwrap();
    assert_eq!(
        lines[0],
        "kept the release link /usr/local/js-agent/link/current"
    );
    assert_eq!(
        std::fs::read_link(link(&root)).unwrap(),
        Path::new("/usr/local/js-agent/releases/0.9.0")
    );
}

#[test]
fn a_plist_the_operator_edited_is_not_overwritten() {
    let (_dir, root) = volume("pi-edited");
    let plist = installed(&root, AGENT_LABEL);
    std::fs::write(&plist, "the host's own\n").unwrap();
    run(&root, VERSION, Some(&Fake::default())).unwrap();
    assert_eq!(std::fs::read_to_string(&plist).unwrap(), "the host's own\n");
}

#[test]
fn on_a_volume_that_is_not_the_running_system_nothing_is_loaded() {
    let (_dir, root) = volume("pi-offline");
    let lines = run(&root, VERSION, None).unwrap();
    assert_eq!(
        lines.last().unwrap(),
        "the volume is not the running system: nothing was loaded"
    );
    assert!(installed(&root, AGENT_LABEL).is_file());
    assert!(link(&root).symlink_metadata().is_ok());
}

#[test]
fn a_release_that_is_not_on_the_volume_changes_nothing() {
    let (_dir, root) = volume("pi-no-release");
    std::fs::remove_dir(root.join(format!("usr/local/js-agent/releases/{VERSION}"))).unwrap();
    let err = run(&root, VERSION, Some(&Fake::default())).unwrap_err();
    assert!(matches!(err, Error::NoRelease(_)), "{err}");
    assert!(link(&root).symlink_metadata().is_err());
    assert!(!installed(&root, AGENT_LABEL).exists());
}

#[test]
fn a_missing_template_changes_nothing() {
    let (_dir, root) = volume("pi-no-template");
    std::fs::remove_file(root.join(format!("usr/local/js-agent/share/{SESSION_LABEL}.plist")))
        .unwrap();
    let fake = Fake::default();
    let err = run(&root, VERSION, Some(&fake)).unwrap_err();
    assert!(matches!(err, Error::NoTemplate(_)), "{err}");
    assert!(link(&root).symlink_metadata().is_err());
    assert!(!installed(&root, AGENT_LABEL).exists());
    assert!(fake.calls.borrow().is_empty());
}

#[test]
fn a_version_that_could_name_another_directory_is_refused() {
    let (_dir, root) = volume("pi-bad-version");
    for bad in ["", ".", "..", "../x", "1/2", ".1", "1 2", "1.2.3\n"] {
        let err = run(&root, bad, None).unwrap_err();
        assert!(matches!(err, Error::BadVersion(_)), "{bad:?}: {err}");
    }
}

#[test]
fn a_refused_bootstrap_stops_the_run_and_names_the_daemon() {
    let (_dir, root) = volume("pi-refused");
    let fake = Fake {
        refuse: true,
        ..Fake::default()
    };
    let err = run(&root, VERSION, Some(&fake)).unwrap_err();
    match &err {
        Error::Launchctl { label, .. } => assert_eq!(label, SESSION_LABEL),
        other => panic!("{other}"),
    }
    assert!(err.to_string().contains("Bootstrap failed: 5"), "{err}");
    // The agent was not tried: the helper it needs did not load.
    assert!(!fake.calls.borrow().iter().any(|c| c.contains(AGENT_LABEL)));
}

#[test]
fn the_real_runner_reads_the_exit_status_of_print_and_bootstrap() {
    let yes = SystemLaunchctl::new("true");
    assert!(yes.is_loaded("x").unwrap());
    yes.bootstrap(Path::new("/x.plist")).unwrap();
    let no = SystemLaunchctl::new("false");
    assert!(!no.is_loaded("x").unwrap());
    assert!(no.bootstrap(Path::new("/x.plist")).is_err());
    let missing = SystemLaunchctl::new("/nonexistent/launchctl");
    assert_eq!(
        missing.is_loaded("x").unwrap_err().kind(),
        io::ErrorKind::NotFound
    );
}

#[test]
fn a_release_link_directory_that_cannot_be_made_stops_the_run_with_its_path() {
    let (_dir, root) = volume("pi-link-dir-is-a-file");
    std::fs::remove_dir(root.join("usr/local/js-agent/link")).unwrap();
    std::fs::write(root.join("usr/local/js-agent/link"), "a file").unwrap();
    let err = run(&root, VERSION, Some(&Fake::default())).unwrap_err();
    assert!(matches!(err, Error::Io { .. }), "{err}");
    assert!(err.to_string().starts_with("link "), "{err}");
}

#[test]
fn a_launch_daemons_directory_that_is_not_there_stops_the_run_with_the_plists_path() {
    let (_dir, root) = volume("pi-no-launch-daemons");
    std::fs::remove_dir(root.join("Library/LaunchDaemons")).unwrap();
    let err = run(&root, VERSION, Some(&Fake::default())).unwrap_err();
    assert!(matches!(err, Error::Io { .. }), "{err}");
    assert!(
        err.to_string().contains("com.komira.js-mac-session.plist"),
        "{err}"
    );
}

#[test]
fn a_launchctl_that_cannot_answer_stops_the_run() {
    struct Mute;
    impl Launchctl for Mute {
        fn is_loaded(&self, _: &str) -> io::Result<bool> {
            Err(io::Error::other("no launchctl"))
        }
        fn bootstrap(&self, _: &Path) -> io::Result<()> {
            unreachable!("nothing is loaded when the state is unknown")
        }
    }
    let (_dir, root) = volume("pi-mute");
    let err = run(&root, VERSION, Some(&Mute)).unwrap_err();
    assert!(matches!(err, Error::Launchctl { .. }), "{err}");
}
