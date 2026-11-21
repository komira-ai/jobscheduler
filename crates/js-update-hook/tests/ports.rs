//! The machine's side of the ports: the command lines of the unit manager, the atomic swap of
//! the release link, the clock and the gate.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, symlink};
use std::path::{Path, PathBuf};
use std::time::Duration;

use js_testkit::dir::TestDir;
use js_update_hook::{
    Clock, CommandRunner, Gate, Link, NoGate, PortError, SYSTEMCTL, SystemClock, SystemLink,
    SystemUnits, UnitState, Units, points_at, read_link, real_path, resolved, same_dir, swap_link,
};

const UNIT: &str = "js-agent-crun.service";
const SHOW: &str = "/usr/bin/systemctl show -p ActiveState,SubState -- js-agent-crun.service";

/// Answers by the full command line and records every line it is asked to run.
#[derive(Default)]
struct FakeCommands {
    answers: BTreeMap<String, Result<String, String>>,
    ran: RefCell<Vec<String>>,
}

impl FakeCommands {
    fn with(mut self, line: &str, answer: Result<&str, &str>) -> Self {
        let answer = answer.map(str::to_owned).map_err(str::to_owned);
        self.answers.insert(line.to_owned(), answer);
        self
    }
}

impl CommandRunner for FakeCommands {
    fn run(&self, program: &str, args: &[&str]) -> Result<String, String> {
        let line = format!("{program} {}", args.join(" "));
        self.ran.borrow_mut().push(line.clone());
        self.answers
            .get(&line)
            .cloned()
            .unwrap_or_else(|| Err(format!("{program}: not found")))
    }

    fn run_in(
        &self,
        program: &str,
        args: &[&str],
        _env: &[(&str, &str)],
    ) -> Result<String, String> {
        self.run(program, args)
    }
}

fn dir(name: &str) -> TestDir {
    TestDir::under(env!("CARGO_TARGET_TMPDIR"), name)
}

fn state(active: &str, sub: &str) -> UnitState {
    UnitState {
        active: active.to_owned(),
        sub: sub.to_owned(),
    }
}

/// A unit counts as crashed when it is failed or waiting to restart, and not otherwise.
#[test]
fn a_unit_has_crashed_when_failed_or_waiting_to_auto_restart() {
    assert!(state("failed", "failed").crashed());
    assert!(state("activating", "auto-restart").crashed());
    assert!(state("failed", "auto-restart").crashed());
    assert!(!state("active", "running").crashed());
    assert!(!state("inactive", "dead").crashed());
}

/// The systemctl path is the stock absolute one.
#[test]
fn the_unit_manager_client_has_its_stock_absolute_path() {
    assert_eq!(SYSTEMCTL, "/usr/bin/systemctl");
}

/// An I/O error becomes an Io port error carrying the error's text.
#[test]
fn an_io_error_converts_to_an_io_port_error_with_its_text() {
    let err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "no way in");
    let want = err.to_string();
    assert_eq!(PortError::from(err), PortError::Io(want.clone()));
    assert_eq!(PortError::Io(want.clone()).to_string(), want);
    assert_eq!(PortError::Command("c".to_owned()).to_string(), "c");
    assert_eq!(PortError::Parse("p".to_owned()).to_string(), "p");
}

/// Show asks for the two properties by name and reads them in either order.
#[test]
fn show_asks_for_the_two_properties_and_reads_them_in_either_order() {
    for text in [
        "ActiveState=deactivating\nSubState=stop-sigterm\n",
        "SubState=stop-sigterm\nActiveState=deactivating\n",
    ] {
        let runner = FakeCommands::default().with(SHOW, Ok(text));
        let got = SystemUnits::new(&runner).show(UNIT).unwrap();
        assert_eq!(got, state("deactivating", "stop-sigterm"));
        assert_eq!(*runner.ran.borrow(), [SHOW]);
    }
}

/// Lines of other properties, or of a longer name with the same start, are not the property.
#[test]
fn show_ignores_other_lines_and_names_that_only_start_alike() {
    let text =
        "Id=x.service\nActiveStateChange=7\nActiveState=active\nSubStateX\nSubState=running\n";
    let runner = FakeCommands::default().with(SHOW, Ok(text));
    let got = SystemUnits::new(&runner).show(UNIT).unwrap();
    assert_eq!(got, state("active", "running"));
}

/// Show names the property that is missing, and passes a failed program on as a command error.
#[test]
fn show_names_what_is_missing_and_what_failed() {
    for (text, name) in [
        ("ActiveState=active\n", "SubState"),
        ("SubState=dead\n", "ActiveState"),
        ("", "ActiveState"),
    ] {
        let runner = FakeCommands::default().with(SHOW, Ok(text));
        let why = SystemUnits::new(&runner).show(UNIT).unwrap_err();
        assert_eq!(
            why,
            PortError::Parse(format!("systemctl show printed no {name}"))
        );
    }
    let runner = FakeCommands::default().with(SHOW, Err("systemctl: exit 1"));
    let why = SystemUnits::new(&runner).show(UNIT).unwrap_err();
    assert_eq!(why, PortError::Command("systemctl: exit 1".to_owned()));
}

/// Show refuses an empty value and a property printed twice, whichever property it is.
#[test]
fn show_refuses_an_empty_value_and_a_property_printed_twice() {
    for (text, why) in [
        (
            "ActiveState=\nSubState=dead\n",
            "systemctl show printed an empty ActiveState",
        ),
        (
            "ActiveState=active\nSubState=\n",
            "systemctl show printed an empty SubState",
        ),
        (
            "ActiveState=active\nSubState=running\nSubState=dead\n",
            "systemctl show printed SubState twice",
        ),
        (
            "ActiveState=active\nActiveState=\nSubState=dead\n",
            "systemctl show printed ActiveState twice",
        ),
    ] {
        let runner = FakeCommands::default().with(SHOW, Ok(text));
        let got = SystemUnits::new(&runner).show(UNIT).unwrap_err();
        assert_eq!(got, PortError::Parse(why.to_owned()), "{text:?}");
    }
}

/// Stop is asked without waiting and start by name, each as one systemctl line.
#[test]
fn stop_is_asked_without_waiting_and_start_by_name() {
    let stop = format!("/usr/bin/systemctl stop --no-block -- {UNIT}");
    let start = format!("/usr/bin/systemctl start -- {UNIT}");
    let runner = FakeCommands::default()
        .with(&stop, Ok(""))
        .with(&start, Ok(""));
    let units = SystemUnits::new(&runner);
    units.stop_no_block(UNIT).unwrap();
    units.start(UNIT).unwrap();
    assert_eq!(*runner.ran.borrow(), [stop, start]);
}

/// A failed stop or start is a command error with the runner's text unchanged.
#[test]
fn a_failed_stop_or_start_is_a_command_error_with_the_runners_text() {
    let runner = FakeCommands::default();
    let units = SystemUnits::new(&runner);
    for why in [units.stop_no_block(UNIT), units.start(UNIT)] {
        assert_eq!(
            why,
            Err(PortError::Command(
                "/usr/bin/systemctl: not found".to_owned()
            ))
        );
    }
}

/// Is-active is true only for the word active, and a failure to run reads as not active.
#[test]
fn is_active_is_true_only_for_active_and_a_failure_reads_as_not_active() {
    let line = format!("/usr/bin/systemctl is-active -- {UNIT}");
    for (answer, want) in [
        (Ok("active\n"), true),
        (Ok("activating\n"), false),
        (Err("systemctl: exit 3"), false),
    ] {
        let runner = FakeCommands::default().with(&line, answer);
        assert_eq!(SystemUnits::new(&runner).is_active(UNIT), Ok(want));
        assert_eq!(runner.ran.borrow().as_slice(), std::slice::from_ref(&line));
    }
}

/// A unit named like an option is passed after the end of options in every call.
#[test]
fn a_unit_named_like_an_option_is_passed_after_the_end_of_options() {
    let runner = FakeCommands::default();
    let units = SystemUnits::new(&runner);
    let _ = (
        units.show("-x"),
        units.stop_no_block("-x"),
        units.start("-x"),
    );
    let _ = units.is_active("-x");
    assert_eq!(
        *runner.ran.borrow(),
        [
            "/usr/bin/systemctl show -p ActiveState,SubState -- -x",
            "/usr/bin/systemctl stop --no-block -- -x",
            "/usr/bin/systemctl start -- -x",
            "/usr/bin/systemctl is-active -- -x",
        ]
    );
}

fn target_of(link: &Path) -> PathBuf {
    fs::read_link(link).unwrap()
}

fn names_in(dir: &Path) -> Vec<std::ffi::OsString> {
    let mut names: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    names.sort();
    names
}

/// Reading a link: absent is None, a link is its target, a regular file is an error.
#[test]
fn read_link_tells_absent_from_a_link_from_something_else() {
    let tmp = dir("ports-read-link");
    let link = tmp.path().join("current");
    assert_eq!(read_link(&link).unwrap(), None);
    symlink("releases/1", &link).unwrap();
    assert_eq!(read_link(&link).unwrap(), Some(PathBuf::from("releases/1")));
    let file = tmp.path().join("plain");
    fs::write(&file, "x").unwrap();
    assert!(read_link(&file).is_err());
}

/// The link port reports absent, present and not-a-link the same way, as port errors.
#[test]
fn the_link_port_reads_the_target_and_calls_a_regular_file_an_io_error() {
    let tmp = dir("ports-link-target");
    let link = tmp.path().join("current");
    assert_eq!(SystemLink.target(&link), Ok(None));
    symlink("releases/2", &link).unwrap();
    assert_eq!(SystemLink.target(&link), Ok(Some("releases/2".into())));
    let file = tmp.path().join("plain");
    fs::write(&file, "x").unwrap();
    assert!(matches!(SystemLink.target(&file), Err(PortError::Io(_))));
}

/// The shape of a Mac: `<tmp>/private/var/lib/pk/svc/{1.4.0,1.3.0}` are real directories,
/// `<tmp>/var` is a link to `private/var`, and `<tmp>/var/lib/pk/svc/current` holds the
/// relative target `1.4.0`. Returns the scratch directory.
fn mac_tree(name: &str) -> TestDir {
    let tmp = dir(name);
    let svc = tmp.path().join("private/var/lib/pk/svc");
    fs::create_dir_all(svc.join("1.4.0")).unwrap();
    fs::create_dir_all(svc.join("1.3.0")).unwrap();
    symlink("private/var", tmp.path().join("var")).unwrap();
    symlink("1.4.0", tmp.path().join("var/lib/pk/svc/current")).unwrap();
    tmp
}

/// A link reached through a symlinked directory that holds a relative target is on the release
/// directory whether the release is named by its real path or through the same symlink; it is
/// not on another existing release.
///
/// Catches a comparison of the two paths by name, which says no to the real path of a release
/// the link sits beside under another spelling (every first install on a Mac), and one so loose
/// that it says yes to a sibling release.
#[test]
fn a_link_is_on_a_release_by_real_path_whatever_symlinked_directory_it_is_reached_through() {
    let tmp = mac_tree("ports-points-at-mac");
    let link = tmp.path().join("var/lib/pk/svc/current");
    let real = tmp.path().join("private/var/lib/pk/svc");
    let spelt = tmp.path().join("var/lib/pk/svc");
    assert_eq!(points_at(&SystemLink, &link, &real.join("1.4.0")), Ok(true));
    assert_eq!(
        points_at(&SystemLink, &link, &spelt.join("1.4.0")),
        Ok(true)
    );
    // The link named by its real path too.
    let real_link = real.join("current");
    assert_eq!(
        points_at(&SystemLink, &real_link, &spelt.join("1.4.0")),
        Ok(true)
    );
    assert_eq!(
        points_at(&SystemLink, &link, &real.join("1.3.0")),
        Ok(false)
    );
    assert_eq!(
        points_at(&SystemLink, &link, &spelt.join("1.3.0")),
        Ok(false)
    );
}

/// A link that is absent, that leads nowhere, or a release directory that is not there, are not
/// "on" anything: false, not an error.
///
/// Catches a missing path reported as a failure of the step, or two missing paths taken as the
/// same place.
#[test]
fn a_link_that_is_missing_or_dangling_or_a_release_that_is_missing_is_on_nothing() {
    let tmp = mac_tree("ports-points-at-missing");
    let svc = tmp.path().join("var/lib/pk/svc");
    let staged = tmp.path().join("private/var/lib/pk/svc/1.4.0");
    assert_eq!(
        points_at(&SystemLink, &svc.join("absent"), &staged),
        Ok(false)
    );
    symlink("9.9.9", svc.join("dangling")).unwrap();
    assert_eq!(
        points_at(&SystemLink, &svc.join("dangling"), &staged),
        Ok(false)
    );
    // Both lead nowhere: still not the same directory.
    assert_eq!(
        points_at(&SystemLink, &svc.join("dangling"), &svc.join("9.9.9")),
        Ok(false)
    );
    assert_eq!(
        points_at(&SystemLink, &svc.join("current"), &svc.join("2.0.0")),
        Ok(false)
    );
}

/// A link path that is a regular file cannot be read as a link, and a path that cannot be
/// resolved for another reason than being absent (a regular file on the way) is an error, from
/// either side of the comparison; neither is "false".
///
/// Catches an unreadable link or a failing lookup taken for "not on the release", which would
/// let a step go on with a link it never read.
#[test]
fn a_link_that_cannot_be_read_or_a_path_that_cannot_be_resolved_is_an_io_error() {
    let tmp = mac_tree("ports-points-at-errors");
    let svc = tmp.path().join("var/lib/pk/svc");
    let staged = svc.join("1.4.0");
    let file = svc.join("plain");
    fs::write(&file, "x").unwrap();
    assert!(matches!(
        points_at(&SystemLink, &file, &staged),
        Err(PortError::Io(_))
    ));
    // The link's target leads through a regular file.
    symlink("plain/x", svc.join("through-file")).unwrap();
    assert!(matches!(
        points_at(&SystemLink, &svc.join("through-file"), &staged),
        Err(PortError::Io(_))
    ));
    // The release path leads through a regular file.
    assert!(matches!(
        points_at(&SystemLink, &svc.join("current"), &file.join("x")),
        Err(PortError::Io(_))
    ));
}

/// Two names are one directory when both lead somewhere and to the same real path; a name that
/// leads nowhere on either side is not one, and a name that cannot be resolved is an error.
///
/// Catches two absent names called equal, a lookup that stops at the first name and never looks
/// at the second, and a failing lookup read as "different".
#[test]
fn two_paths_are_one_directory_by_their_real_paths_and_absent_ones_are_not() {
    let tmp = mac_tree("ports-same-dir");
    let real = tmp.path().join("private/var/lib/pk/svc");
    let spelt = tmp.path().join("var/lib/pk/svc");
    assert_eq!(
        same_dir(&SystemLink, &real.join("1.4.0"), &spelt.join("1.4.0")),
        Ok(true)
    );
    assert_eq!(
        same_dir(&SystemLink, &real.join("1.4.0"), &spelt.join("current")),
        Ok(true)
    );
    assert_eq!(
        same_dir(&SystemLink, &real.join("1.4.0"), &spelt.join("1.3.0")),
        Ok(false)
    );
    assert_eq!(
        same_dir(&SystemLink, &real.join("absent"), &real.join("1.4.0")),
        Ok(false)
    );
    assert_eq!(
        same_dir(&SystemLink, &real.join("1.4.0"), &real.join("absent")),
        Ok(false)
    );
    assert_eq!(
        same_dir(&SystemLink, &real.join("absent"), &spelt.join("absent")),
        Ok(false)
    );
    let file = real.join("plain");
    fs::write(&file, "x").unwrap();
    assert!(matches!(
        same_dir(&SystemLink, &file.join("x"), &real.join("1.4.0")),
        Err(PortError::Io(_))
    ));
    assert!(matches!(
        same_dir(&SystemLink, &real.join("1.4.0"), &file.join("x")),
        Err(PortError::Io(_))
    ));
}

/// The real path of a name is the canonical path with every link on the way followed; a name
/// that is absent, or a link that leads nowhere, is `None`; a regular file on the way is an
/// error.
///
/// Catches a real path that keeps the link's own spelling, an absent path reported as an error,
/// and a not-a-directory error swallowed as absent.
#[test]
fn the_real_path_follows_every_link_and_tells_absent_from_unresolvable() {
    let tmp = mac_tree("ports-real-path");
    let root = fs::canonicalize(tmp.path()).unwrap();
    assert_eq!(
        real_path(&tmp.path().join("var/lib/pk/svc/current")).unwrap(),
        Some(root.join("private/var/lib/pk/svc/1.4.0"))
    );
    assert_eq!(
        SystemLink.real(&tmp.path().join("var")),
        Ok(Some(root.join("private/var")))
    );
    assert_eq!(real_path(&tmp.path().join("nothing")).unwrap(), None);
    symlink("nowhere", tmp.path().join("dangling")).unwrap();
    assert_eq!(real_path(&tmp.path().join("dangling")).unwrap(), None);
    let file = tmp.path().join("plain");
    fs::write(&file, "x").unwrap();
    assert!(real_path(&file.join("x")).is_err());
    assert!(matches!(
        SystemLink.real(&file.join("x")),
        Err(PortError::Io(_))
    ));
}

/// Where a link holding a target points, by name: an absolute target as it is, a relative one
/// from the link's directory, with `.` and `..` taken away; nothing is looked up.
///
/// Catches a relative target read from the wrong directory, a `..` that is kept as a name or
/// that climbs one level too few or too many, and an absolute target joined to the link's
/// directory.
#[test]
fn a_link_target_is_resolved_by_name_from_the_links_directory() {
    let link = Path::new("/a/b/current");
    for (target, want) in [
        ("1.4.0", "/a/b/1.4.0"),
        ("./1.4.0", "/a/b/1.4.0"),
        ("x/./y", "/a/b/x/y"),
        ("../1.4.0", "/a/1.4.0"),
        ("../../c/d", "/c/d"),
        ("x/../y", "/a/b/y"),
        ("/z/1.4.0", "/z/1.4.0"),
        ("1.4.0/", "/a/b/1.4.0"),
    ] {
        assert_eq!(
            resolved(link, Path::new(target)),
            Path::new(want),
            "{target}"
        );
    }
    // A link with no parent resolves from itself.
    assert_eq!(resolved(Path::new("/"), Path::new("x")), Path::new("/x"));
}

/// Relink makes the link, repoints it, and leaves no .relink name behind.
#[test]
fn relink_makes_the_link_and_repoints_it_without_leaving_the_new_name_behind() {
    let tmp = dir("ports-relink");
    let link = tmp.path().join("current");
    SystemLink.relink(&link, Path::new("releases/1.0")).unwrap();
    assert_eq!(target_of(&link), Path::new("releases/1.0"));
    SystemLink.relink(&link, Path::new("releases/1.1")).unwrap();
    assert_eq!(target_of(&link), Path::new("releases/1.1"));
    assert_eq!(names_in(tmp.path()), ["current"]);
}

/// A link already pointing at the target is left untouched and no .relink is made.
#[test]
fn swapping_to_the_target_the_link_has_changes_nothing() {
    let tmp = dir("ports-swap-same");
    let link = tmp.path().join("current");
    swap_link(&link, Path::new("a")).unwrap();
    let before = fs::symlink_metadata(&link).unwrap();
    // A directory at the .relink name would make any attempt to build a new link fail.
    fs::create_dir(tmp.path().join("current.relink")).unwrap();
    swap_link(&link, Path::new("a")).unwrap();
    let after = fs::symlink_metadata(&link).unwrap();
    assert_eq!(
        (before.ino(), before.ctime_nsec()),
        (after.ino(), after.ctime_nsec())
    );
    assert_eq!(names_in(tmp.path()), ["current", "current.relink"]);
}

/// A leftover .relink file from an interrupted swap is removed first and the swap completes.
#[test]
fn a_swap_replaces_a_leftover_of_an_interrupted_swap() {
    let tmp = dir("ports-swap-leftover");
    let link = tmp.path().join("current");
    swap_link(&link, Path::new("old")).unwrap();
    symlink("stale", tmp.path().join("current.relink")).unwrap();
    swap_link(&link, Path::new("new")).unwrap();
    assert_eq!(target_of(&link), Path::new("new"));
    assert_eq!(names_in(tmp.path()), ["current"]);
}

/// A leftover .relink that is a non-empty directory fails the swap and the link stays as it was.
#[test]
fn a_swap_that_cannot_clear_its_leftover_leaves_the_link_as_it_was() {
    let tmp = dir("ports-swap-blocked");
    let link = tmp.path().join("current");
    swap_link(&link, Path::new("old")).unwrap();
    let leftover = tmp.path().join("current.relink");
    fs::create_dir(&leftover).unwrap();
    fs::write(leftover.join("keep"), "x").unwrap();
    assert!(swap_link(&link, Path::new("new")).is_err());
    assert_eq!(target_of(&link), Path::new("old"));
    let err = SystemLink.relink(&link, Path::new("new")).unwrap_err();
    assert!(matches!(err, PortError::Io(_)), "{err:?}");
    assert_eq!(target_of(&link), Path::new("old"));
    assert_eq!(fs::read_to_string(leftover.join("keep")).unwrap(), "x");
}

/// A swap never replaces something that is not a link, file or directory alike.
#[test]
fn a_swap_never_replaces_something_that_is_not_a_link() {
    let tmp = dir("ports-swap-nonlink");
    let file = tmp.path().join("current");
    fs::write(&file, "a regular file").unwrap();
    assert!(swap_link(&file, Path::new("new")).is_err());
    assert_eq!(fs::read_to_string(&file).unwrap(), "a regular file");
    let folder = tmp.path().join("releases");
    fs::create_dir(&folder).unwrap();
    assert!(matches!(
        SystemLink.relink(&folder, Path::new("new")),
        Err(PortError::Io(_))
    ));
    assert!(fs::symlink_metadata(&folder).unwrap().is_dir());
    assert!(!tmp.path().join("releases.relink").exists());
}

/// A link whose parent directory does not exist cannot be made.
#[test]
fn a_swap_into_a_missing_directory_is_an_error() {
    let tmp = dir("ports-swap-no-parent");
    let link = tmp.path().join("missing/dir/link");
    assert!(swap_link(&link, Path::new("t")).is_err());
    assert!(matches!(
        SystemLink.relink(&link, Path::new("t")),
        Err(PortError::Io(_))
    ));
    assert!(names_in(tmp.path()).is_empty());
}

/// The machine clock reports a non-zero time and pausing for nothing returns.
#[test]
fn the_machine_clock_tells_a_nonzero_time_and_pausing_for_nothing_returns() {
    let clock = SystemClock;
    assert_ne!(clock.now_ms(), 0);
    clock.pause(Duration::ZERO);
}

/// The machine gate does nothing at a kill point.
#[test]
fn the_machine_gate_does_nothing() {
    NoGate.point();
}
