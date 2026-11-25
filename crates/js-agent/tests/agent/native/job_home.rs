//! Where a native job may write and what it takes for its home: the run directory and
//! nothing else, with `HOME`, `CFFIXED_USER_HOME` and `TMPDIR` below it, so the account's
//! real home stays empty (a folder a job makes in it, such as `~/Library/Preferences`, is
//! one macOS refuses root to remove, which breaks the slot). The tests read the profile, the
//! environment and the working directory from what the runtime asks the supervisor.

use std::fs;
use std::path::Path;

use js_agent::lifecycle::{Exit, Runtime, Status};

use super::{Rig, native, param};

/// The profile's one rule that grants writes, with no mount grants in the run.
const WRITE_GRANT: &str =
    "(allow file-write* (subpath (param \"RUN\")) (literal \"/dev/null\") (subpath \"/dev/fd\"))";

fn text(path: &Path) -> String {
    path.display().to_string()
}

/// Asserts that the profile of a run grants writes to the run directory `<home>/exec` by
/// the exact rule text and to no other place: `RUN` is `<home>/exec`, the only parameter of
/// the profile that is the account's home is `OWN`, which only its read grants name, and no
/// other rule allows a write.
///
/// Catches: a profile that grants the whole home (`RUN` = the home), with which a job makes
/// `~/Library/Preferences`, which root cannot remove.
#[test]
fn the_profile_grants_writes_below_the_run_directory_and_not_to_the_home() {
    let mut rig = Rig::new("job-home-profile");
    let id = rig.start("r1", "/usr/bin/id", &[]);
    let launched = rig.launcher.last();
    let home = rig.home_of(launched.var("USER").unwrap());
    assert_eq!(
        param(&launched, "RUN"),
        Some(text(&home.join("exec")).as_str())
    );
    assert!(
        launched.profile.contains(WRITE_GRANT),
        "{}",
        launched.profile
    );
    let writes: Vec<&str> = launched
        .profile
        .lines()
        .filter(|line| line.starts_with("(allow file-write") || line.starts_with("(allow file*"))
        .collect();
    assert_eq!(writes, [WRITE_GRANT]);
    for (name, value) in &launched.params {
        if name != "OWN" {
            assert_ne!(Path::new(value), home, "{name} is the account's home");
        }
    }
    assert_eq!(param(&launched, "OWN"), Some(text(&home).as_str()));
    let own: Vec<&str> = launched
        .profile
        .lines()
        .filter(|line| line.contains("(param \"OWN\")"))
        .collect();
    assert_eq!(
        own,
        [
            "(allow file-read* (subpath (param \"OWN\")))",
            "(allow file-read-metadata (path-ancestors (param \"OWN\")))",
        ]
    );
    rig.settle(&id);
    rig.rt.remove(&id).unwrap();
}

/// Asserts that a job's `HOME` is `<home>/exec/.home`, that `CFFIXED_USER_HOME` is the same
/// folder and `TMPDIR` is `<home>/exec/.tmp` (all in the environment the supervisor starts
/// the job with), that the working directory is the run directory, that the home and the
/// temporary directory are directories of the run, and that nothing of the account's home
/// outside `exec/` was made.
///
/// Catches: a job whose preferences and caches go to the account's home, or whose temporary
/// files do.
#[test]
fn a_jobs_home_and_temporary_directory_are_in_the_run_directory() {
    let mut rig = Rig::new("job-home-env");
    let id = rig.start("r1", "/usr/bin/id", &["-u"]);
    let launched = rig.launcher.last();
    let home = rig.home_of(launched.var("USER").unwrap());
    let exec = home.join("exec");
    assert_eq!(
        launched.var("HOME"),
        Some(text(&exec.join(".home")).as_str())
    );
    assert_eq!(
        launched.var("CFFIXED_USER_HOME"),
        Some(text(&exec.join(".home")).as_str())
    );
    assert_eq!(
        launched.var("TMPDIR"),
        Some(text(&exec.join(".tmp")).as_str())
    );
    assert_eq!(launched.cwd, fs::canonicalize(&exec).unwrap());
    assert_eq!(launched.program, "/usr/bin/id");
    assert_eq!(launched.args, ["-u"]);
    for dir in [".home", ".tmp"] {
        assert!(exec.join(dir).is_dir(), "{dir}");
    }
    let mut made: Vec<String> = fs::read_dir(&home)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    made.sort();
    assert_eq!(
        made,
        ["exec"],
        "the account's home holds only the run directory"
    );
    rig.settle(&id);
    rig.rt.remove(&id).unwrap();
}

/// Asserts that the program of a run and its arguments reach the supervisor unchanged, with
/// nothing in front of the program: an argument that looks like a `NAME=value` assignment
/// stays an argument of the program, and the program, run with them, prints them as they
/// were.
///
/// Catches: a wrapper that eats or reorders the program's arguments, or reads one as an
/// assignment.
#[test]
fn the_programs_arguments_reach_the_supervisor_as_they_were() {
    let mut rig = Rig::new("job-home-args");
    let spec = rig.spec("r1", native("/bin/echo", &["HOME=/elsewhere", "-n", "x"]));
    let id = rig.rt.create_and_start(&spec).unwrap();
    let launched = rig.launcher.last();
    assert_eq!(launched.program, "/bin/echo");
    assert_eq!(launched.args, ["HOME=/elsewhere", "-n", "x"]);
    assert_eq!(
        launched.var("HOME"),
        Some(
            text(
                &rig.home_of(launched.var("USER").unwrap())
                    .join("exec/.home")
            )
            .as_str()
        )
    );
    assert_eq!(
        rig.settle(&id),
        Status::Exited(Exit {
            code: 0,
            oom_killed: false
        })
    );
    let tail = rig.rt.log_tail(&id).unwrap().unwrap();
    assert_eq!(tail.stdout, "HOME=/elsewhere -n x\n");
    rig.rt.remove(&id).unwrap();
}
