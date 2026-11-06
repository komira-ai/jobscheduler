//! The comparison of the unit a release carries with the one the previous release carried: which
//! keys of a systemd unit differ, a plist compared as bytes, and the cases where there is nothing
//! to compare with.

use std::fs;
use std::path::Path;

use js_agent::update_hook::{INSTALLED_MISSING, Manager, PlanError, UnitRestart, plan};
use js_testkit::dir::TestDir;

const UNIT: &str = "js-agent-crun.service";

/// Two releases in a directory of the test's, each with the unit file `text` if it has one.
struct Releases {
    tmp: TestDir,
}

impl Releases {
    fn new(name: &str) -> Releases {
        Releases {
            tmp: TestDir::under(env!("CARGO_TARGET_TMPDIR"), name),
        }
    }

    fn staged(&self) -> std::path::PathBuf {
        self.tmp.path().join("staged")
    }

    fn previous(&self) -> std::path::PathBuf {
        self.tmp.path().join("previous")
    }

    /// Puts `text` in the unit file of `release`.
    fn put(&self, release: &Path, manager: Manager, text: &str) {
        let file = manager.path_in(release, UNIT);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, text).unwrap();
    }

    /// The systemd plan from the previous unit `old` to the staged unit `new`.
    fn systemd(&self, old: &str, new: &str) -> Option<Vec<String>> {
        self.put(&self.previous(), Manager::Systemd, old);
        self.put(&self.staged(), Manager::Systemd, new);
        plan(
            Manager::Systemd,
            UNIT,
            &self.staged(),
            Some(&self.previous()),
        )
        .unwrap()
        .map(|restart| restart.changed)
    }
}

fn changed(keys: &[&str]) -> Option<Vec<String>> {
    Some(keys.iter().map(|key| (*key).to_owned()).collect())
}

/// The unit file of a manager is `units/<unit>` for systemd and `units/<unit>.plist` for
/// launchd, in the release directory.
///
/// Catches a launchd job looked for under the systemd name, or the other way.
#[test]
fn the_unit_file_of_a_release_is_in_its_units_directory_under_the_managers_name() {
    assert_eq!(Manager::Systemd.file_name("agent.service"), "agent.service");
    assert_eq!(Manager::Launchd.file_name("js-agent"), "js-agent.plist");
    assert_eq!(
        Manager::Systemd.path_in(Path::new("/r/1.4.0"), "agent.service"),
        Path::new("/r/1.4.0/units/agent.service")
    );
    assert_eq!(
        Manager::Launchd.path_in(Path::new("/r/1.4.0"), "js-agent"),
        Path::new("/r/1.4.0/units/js-agent.plist")
    );
}

/// The same unit, spelt with other spacing, comments and blank lines, is the same unit.
///
/// Catches a comparison of bytes, which would restart an agent whose unit only gained a
/// comment.
#[test]
fn spacing_comments_and_blank_lines_are_no_difference() {
    let releases = Releases::new("hook-unit-same");
    let old =
        "[Unit]\nDescription=agent\n\n[Service]\nExecStart=/bin/agent --pool farm\nDelegate=yes\n";
    let new = "# the agent\n[Unit]\n  Description = agent  \n; second kind of comment\n\n\n[Service]\nExecStart= /bin/agent --pool farm\n# why\nDelegate =yes\n";
    assert_eq!(releases.systemd(old, new), None);
    assert_eq!(releases.systemd(old, old), None);
}

/// A key whose value changed, a key that is new and a key that is gone are each named
/// `Section.Key`, once, in order, however many of its values changed.
///
/// Catches a plan that names the file and not the keys, a key named without its section, and
/// an added or removed key that is missed.
#[test]
fn a_changed_added_or_removed_key_is_named_with_its_section() {
    let releases = Releases::new("hook-unit-keys");
    let old = "[Unit]\nDescription=agent\n[Service]\nExecStart=/bin/agent\nKillMode=process\n";
    let new = "[Unit]\nDescription=agent\n[Service]\nExecStart=/bin/agent --new\nDelegate=yes\nDelegateSubgroup=supervisor\n";
    assert_eq!(
        releases.systemd(old, new),
        changed(&[
            "Service.Delegate",
            "Service.DelegateSubgroup",
            "Service.ExecStart",
            "Service.KillMode"
        ])
    );
    // The same key in another section is another key.
    let moved = "[Install]\nWantedBy=multi-user.target\n";
    let there = "[Unit]\nWantedBy=multi-user.target\n";
    assert_eq!(
        releases.systemd(moved, there),
        changed(&["Install.WantedBy", "Unit.WantedBy"])
    );
}

/// The values of a key given more than once are compared in order, and a key given again is not
/// a key given once.
///
/// Catches values compared as a set, which would take a reordered `ExecStartPre` for no change,
/// and a doubled key taken for a single one.
#[test]
fn the_values_of_a_key_are_compared_in_order() {
    let releases = Releases::new("hook-unit-order");
    let old = "[Service]\nExecStartPre=/bin/a\nExecStartPre=/bin/b\n";
    let swapped = "[Service]\nExecStartPre=/bin/b\nExecStartPre=/bin/a\n";
    let doubled = "[Service]\nExecStartPre=/bin/a\nExecStartPre=/bin/b\nExecStartPre=/bin/b\n";
    let reset = "[Service]\nExecStartPre=\nExecStartPre=/bin/a\nExecStartPre=/bin/b\n";
    assert_eq!(
        releases.systemd(old, swapped),
        changed(&["Service.ExecStartPre"])
    );
    assert_eq!(
        releases.systemd(old, doubled),
        changed(&["Service.ExecStartPre"])
    );
    assert_eq!(
        releases.systemd(old, reset),
        changed(&["Service.ExecStartPre"])
    );
    assert_eq!(releases.systemd(old, old), None);
}

/// A line ending in a backslash goes on in the next, a comment inside it is skipped, and the
/// joined line is the assignment.
///
/// Catches a continued `ExecStart` read as two lines, which would name a key that is not there
/// and miss one that changed.
#[test]
fn a_line_ending_in_a_backslash_is_joined_to_the_next() {
    let releases = Releases::new("hook-unit-continued");
    let one = "[Service]\nExecStart=/bin/agent --pool farm --drain 3600\n";
    let two = "[Service]\nExecStart=/bin/agent \\\n  --pool farm \\\n# a comment inside\n  --drain 3600\n";
    assert_eq!(releases.systemd(one, two), None);
    let other = "[Service]\nExecStart=/bin/agent \\\n  --pool farm \\\n  --drain 60\n";
    assert_eq!(
        releases.systemd(one, other),
        changed(&["Service.ExecStart"])
    );
}

/// A line before the first section, or without an `=`, is ignored as systemd ignores it, and a
/// section header with its bracket open is no section.
///
/// Catches a parser that takes stray text for an assignment or a header, which would name a key
/// no unit has.
#[test]
fn lines_systemd_ignores_are_ignored() {
    let releases = Releases::new("hook-unit-stray");
    let old = "[Service]\nExecStart=/bin/agent\n";
    let new =
        "Orphan=1\n[Service\nnot an assignment\n[Service]\nExecStart=/bin/agent\nplain words\n";
    assert_eq!(releases.systemd(old, new), None);
}

/// A staged unit that is not UTF-8 is compared as text with the bad bytes replaced; two such
/// units that differ only there are the same.
///
/// Catches a unit that cannot be read as text failing the step.
#[test]
fn a_unit_that_is_not_utf8_is_still_compared() {
    let releases = Releases::new("hook-unit-bytes");
    releases.put(
        &releases.previous(),
        Manager::Systemd,
        "[Service]\nExecStart=/bin/agent\n",
    );
    let file = Manager::Systemd.path_in(&releases.staged(), UNIT);
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(
        &file,
        b"[Service]\nExecStart=/bin/agent\nDescription=\xff\n",
    )
    .unwrap();
    let got = plan(
        Manager::Systemd,
        UNIT,
        &releases.staged(),
        Some(&releases.previous()),
    )
    .unwrap();
    assert_eq!(
        got,
        Some(UnitRestart {
            changed: vec!["Service.Description".to_owned()]
        })
    );
}

/// A previous release that carries no unit file counts as a change, named so; a first install has
/// none to compare with and needs only its own unit.
///
/// Catches the first hostkeeper release of a host installed some other way being taken for an
/// unchanged unit, which would start the new layout from a cgroup of strays.
#[test]
fn a_previous_release_without_a_unit_is_a_change_and_a_first_install_is_not() {
    let releases = Releases::new("hook-unit-missing");
    releases.put(
        &releases.staged(),
        Manager::Systemd,
        "[Service]\nExecStart=/bin/agent\n",
    );
    fs::create_dir_all(releases.previous()).unwrap();
    assert_eq!(
        plan(
            Manager::Systemd,
            UNIT,
            &releases.staged(),
            Some(&releases.previous())
        )
        .unwrap(),
        Some(UnitRestart {
            changed: vec![INSTALLED_MISSING.to_owned()]
        })
    );
    assert_eq!(INSTALLED_MISSING, "(installed unit missing)");
    assert_eq!(
        plan(Manager::Systemd, UNIT, &releases.staged(), None).unwrap(),
        None
    );
}

/// A staged release without its unit is refused whether or not there is a previous release, and
/// an installed unit that is there and cannot be read is its own error.
///
/// Catches a release without its unit let through, and an unreadable installed unit taken for a
/// missing one.
#[test]
fn a_staged_release_must_carry_its_unit_and_an_unreadable_installed_unit_is_an_error() {
    let releases = Releases::new("hook-unit-errors");
    releases.put(&releases.previous(), Manager::Systemd, "[Service]\n");
    fs::create_dir_all(releases.staged()).unwrap();
    for previous in [Some(releases.previous()), None] {
        let error = plan(
            Manager::Systemd,
            UNIT,
            &releases.staged(),
            previous.as_deref(),
        )
        .unwrap_err();
        assert!(matches!(&error, PlanError::Staged { path, .. }
            if path == &Manager::Systemd.path_in(&releases.staged(), UNIT)));
        let said = error.to_string();
        assert!(
            said.starts_with("the staged release carries no readable unit "),
            "{said}"
        );
        assert!(said.contains(UNIT), "{said}");
    }
    // A directory where the installed unit file should be: there, and not a file to read.
    releases.put(&releases.staged(), Manager::Systemd, "[Service]\n");
    let installed = Manager::Systemd.path_in(&releases.previous(), UNIT);
    fs::remove_file(&installed).unwrap();
    fs::create_dir(&installed).unwrap();
    let error = plan(
        Manager::Systemd,
        UNIT,
        &releases.staged(),
        Some(&releases.previous()),
    )
    .unwrap_err();
    assert!(matches!(&error, PlanError::Installed { path, .. } if path == &installed));
    assert!(
        error
            .to_string()
            .starts_with("cannot read the installed unit ")
    );
}

/// A plist is compared as the bytes it is: the same bytes are no change, any other byte is one,
/// named by the plist's file.
///
/// Catches a plist parsed as a unit, which has no sections and so would never differ.
#[test]
fn a_plist_is_compared_as_bytes_and_named_by_its_file() {
    let releases = Releases::new("hook-unit-plist");
    let label = "js-agent";
    let same = "<plist><dict><key>Label</key><string>js-agent</string></dict></plist>\n";
    releases.put(&releases.previous(), Manager::Launchd, same);
    let file = Manager::Launchd.path_in(&releases.previous(), UNIT);
    fs::rename(&file, Manager::Launchd.path_in(&releases.previous(), label)).unwrap();
    releases.put(&releases.staged(), Manager::Launchd, same);
    fs::rename(
        Manager::Launchd.path_in(&releases.staged(), UNIT),
        Manager::Launchd.path_in(&releases.staged(), label),
    )
    .unwrap();
    assert_eq!(
        plan(
            Manager::Launchd,
            label,
            &releases.staged(),
            Some(&releases.previous())
        )
        .unwrap(),
        None
    );
    fs::write(
        Manager::Launchd.path_in(&releases.staged(), label),
        format!("{same} "),
    )
    .unwrap();
    assert_eq!(
        plan(
            Manager::Launchd,
            label,
            &releases.staged(),
            Some(&releases.previous())
        )
        .unwrap(),
        Some(UnitRestart {
            changed: vec!["js-agent.plist".to_owned()]
        })
    );
}
