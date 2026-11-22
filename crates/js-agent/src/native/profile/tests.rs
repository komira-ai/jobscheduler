//! The command line of a native run, built on any platform: where the profile lets a job
//! write and what the job's home is.

use std::path::PathBuf;

use js_mac_pool::seatbelt::{AbsPath, ReadModel};
use js_mac_session::full_disk_access::FullDiskAccess;

use super::*;
use crate::native::Ceiling;

/// The configuration of a run whose users' homes are below `/srv/homes`.
fn config() -> NativeConfig {
    NativeConfig {
        homes: PathBuf::from("/srv/homes"),
        network: Ceiling::Loopback,
        tool_roots: vec![PathBuf::from("/usr/bin")],
        read_denies: vec![AbsPath::parse("/srv/js-state").unwrap()],
        mount_roots: Vec::new(),
        read_model: ReadModel::Open,
        journal: PathBuf::from("/srv/state/native-leases"),
        max_file_bytes: 1 << 30,
        max_processes: 512,
        egress: None,
        full_disk_access: FullDiskAccess::Granted,
    }
}

fn body(program: &str, args: &[&str]) -> NativeSpec {
    NativeSpec {
        program: program.to_owned(),
        args: args.iter().map(|arg| (*arg).to_owned()).collect(),
        env: BTreeMap::new(),
        cwd: String::new(),
        inputs: Vec::new(),
        outputs: Vec::new(),
        network: NetworkLevel::Off,
        ports: Vec::new(),
    }
}

/// Asserts that the profile grants writes to the run directory `<home>/exec` by the exact
/// rule text, names no parameter that is the account's home, and that the program follows
/// `/usr/bin/env HOME=<home>/exec/.home` with its arguments as they were.
///
/// Catches: a profile that grants the whole home, with which a job makes
/// `~/Library/Preferences` that root cannot remove, and a job whose `HOME` is the account's
/// home (the helper's value) because nothing set it.
#[test]
fn a_run_may_write_below_its_run_directory_alone_and_gets_a_home_there() {
    let home = Path::new("/srv/homes/kbf-lease-pool-4");
    let argv = profile_argv(&config(), &body("/bin/echo", &["HOME=/x", "-n"]), home).unwrap();
    let rule = "(allow file-write* (subpath (param \"RUN\")) (literal \"/dev/null\") (subpath \"/dev/fd\"))";
    let at = argv.iter().position(|word| word == "-p").unwrap();
    let profile = &argv[at + 1];
    let writes: Vec<&str> = profile
        .lines()
        .filter(|line| line.starts_with("(allow file-write") || line.starts_with("(allow file*"))
        .collect();
    assert_eq!(writes, [rule]);
    let params: Vec<&String> = argv[..at]
        .iter()
        .filter(|word| word.contains('='))
        .collect();
    assert!(params.contains(&&"RUN=/srv/homes/kbf-lease-pool-4/exec".to_owned()));
    assert!(
        !params
            .iter()
            .any(|word| word.ends_with("=/srv/homes/kbf-lease-pool-4")),
        "{params:?}"
    );
    assert_eq!(
        argv[at + 2..],
        [
            "/usr/bin/env",
            "HOME=/srv/homes/kbf-lease-pool-4/exec/.home",
            "/bin/echo",
            "HOME=/x",
            "-n"
        ]
    );
}

/// Asserts that a program that is not an absolute path fails the build of the command line,
/// naming it, as before the `env` that sets `HOME` stood in front of it.
///
/// Catches: a relative program that `env` would search `PATH` for.
#[test]
fn a_relative_program_is_refused() {
    let home = Path::new("/srv/homes/kbf-lease-pool-4");
    let why = profile_argv(&config(), &body("echo", &[]), home).unwrap_err();
    assert!(why.0.contains("echo"), "{}", why.0);
}
