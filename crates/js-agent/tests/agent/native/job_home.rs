//! Where a native job may write and what it takes for its home: the run directory and
//! nothing else, with `HOME`, `CFFIXED_USER_HOME` and `TMPDIR` below it, so the pool user's
//! real home stays empty (a folder a job makes in it, such as `~/Library/Preferences`, is
//! one macOS refuses the root helper to remove, which breaks the slot).

use std::path::Path;

use js_agent::lifecycle::Runtime;

use super::{Rig, native, parse};

/// The profile's one rule that grants writes, with no mount grants in the run.
const WRITE_GRANT: &str =
    "(allow file-write* (subpath (param \"RUN\")) (literal \"/dev/null\") (subpath \"/dev/fd\"))";

fn text(path: &Path) -> String {
    path.display().to_string()
}

/// Asserts that the profile of a run grants writes to the run directory `<home>/exec` by
/// the exact rule text and to no other place: `RUN` is `<home>/exec`, no parameter of the
/// profile is the account's home, and no other rule allows a write.
///
/// Catches: a profile that grants the whole home (`RUN` = the home), with which a job makes
/// `~/Library/Preferences`, which the root helper cannot remove.
#[test]
fn the_profile_grants_writes_below_the_run_directory_and_not_to_the_home() {
    let mut rig = Rig::new("job-home-profile");
    rig.start("r1", "/usr/bin/id", &[]);
    let home = rig.fake.home_of("7.0");
    let parsed = parse(&rig.fake.run_of("7.0").argv);
    assert_eq!(
        parsed.params[0],
        ("RUN".to_owned(), text(&home.join("exec")))
    );
    assert!(parsed.profile.contains(WRITE_GRANT), "{}", parsed.profile);
    let writes: Vec<&str> = parsed
        .profile
        .lines()
        .filter(|line| line.starts_with("(allow file-write") || line.starts_with("(allow file*"))
        .collect();
    assert_eq!(writes, [WRITE_GRANT]);
    for (name, value) in &parsed.params {
        assert_ne!(Path::new(value), home, "{name} is the account's home");
    }
}

/// Asserts that a job's `HOME` is `<home>/exec/.home`, set by `env` in front of the program
/// inside the profile (the helper sets `HOME` to the account's home whatever it is asked,
/// so the request cannot carry it), that `CFFIXED_USER_HOME` is the same folder and
/// `TMPDIR` is `<home>/exec/.tmp`, that both are directories of the run, and that nothing
/// of the account's home outside `exec/` was made.
///
/// Catches: a job whose preferences and caches go to the account's home (`HOME` left as the
/// helper sets it), or whose temporary files do.
#[test]
fn a_jobs_home_and_temporary_directory_are_in_the_run_directory() {
    let mut rig = Rig::new("job-home-env");
    rig.start("r1", "/usr/bin/id", &["-u"]);
    let home = rig.fake.home_of("7.0");
    let exec = home.join("exec");
    let run = rig.fake.run_of("7.0");
    let parsed = parse(&run.argv);
    assert_eq!(parsed.home, text(&exec.join(".home")));
    assert_eq!(parsed.tail, ["/usr/bin/id", "-u"]);
    let env = |name: &str| {
        run.env
            .iter()
            .find(|(set, _)| set == name)
            .map(|(_, value)| value.clone())
    };
    assert_eq!(env("CFFIXED_USER_HOME"), Some(text(&exec.join(".home"))));
    assert_eq!(env("TMPDIR"), Some(text(&exec.join(".tmp"))));
    for dir in [".home", ".tmp"] {
        assert!(exec.join(dir).is_dir(), "{dir}");
    }
    let mut made: Vec<String> = std::fs::read_dir(&home)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    made.sort();
    assert_eq!(
        made,
        ["exec"],
        "the account's home holds only the run directory"
    );
}

/// Asserts that the program of a run and its arguments are carried unchanged behind the
/// `env` that sets `HOME`: an argument that looks like a `NAME=value` assignment stays an
/// argument of the program.
///
/// Catches: a wrapper that eats or reorders the program's arguments, or reads one as an
/// assignment.
#[test]
fn the_home_setting_leaves_the_programs_arguments_as_they_were() {
    let mut rig = Rig::new("job-home-args");
    let spec = rig.spec("r1", native("/bin/echo", &["HOME=/elsewhere", "-n", "x"]));
    rig.rt.create_and_start(&spec).unwrap();
    let parsed = parse(&rig.fake.run_of("7.0").argv);
    assert_eq!(parsed.tail, ["/bin/echo", "HOME=/elsewhere", "-n", "x"]);
    assert_eq!(
        parsed.home,
        text(&rig.fake.home_of("7.0").join("exec/.home"))
    );
}
