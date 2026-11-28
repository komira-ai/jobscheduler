//! `--estimate-feed` beside files that will not open, and the hold of a heartbeat's reply the
//! pool's interval sets, in process: the configuration read from the flags and the agent side
//! assembled from it as the server assembles it. No listener is bound and no connection is made.

use std::ffi::OsString;
use std::path::Path;

use js_testkit::dir::TestDir;
use js_types::{ManualClock, Timestamp};

use crate::agents::assemble;
use crate::clock::SharedClock;
use crate::config::{Cli, Config};

/// When the test's clock starts.
const T0: u64 = 1_000_000;

/// A pool of up to two machines whose class books a whole host.
const POOL: &str = r#"
name = "burst"
class = "burst-host"
max_machines = 2

[shape]
cpu_millis = 8000
memory_bytes = 17179869184

[capabilities]
arch = "x86_64"
"#;

/// An empty directory of the test's own under the target directory, removed when the
/// guard is dropped.
fn scratch(name: &str) -> TestDir {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/estimate_feed");
    TestDir::under(root, name)
}

fn write(dir: &Path, name: &str, text: &str, mode: u32) -> OsString {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    std::fs::write(&path, text).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
    path.into_os_string()
}

/// The configuration of a server with agent registration over [`POOL`] and `extra` flags.
/// Usage is on, in `usage.db` beside the state file.
fn config(dir: &Path, extra: &[&str]) -> Config {
    config_over(dir, extra, POOL)
}

/// [`config`] over the pool file `pool`.
fn config_over(dir: &Path, extra: &[&str], pool: &str) -> Config {
    let flags = [
        ("--listen", "127.0.0.1:0".into()),
        ("--state-file", dir.join("state").into_os_string()),
        ("--boot", "initialise".into()),
        ("--token-file", write(dir, "token", &"t".repeat(40), 0o600)),
        (
            "--agent-pool-config",
            write(dir, "agent-pool.toml", pool, 0o644),
        ),
    ];
    let mut args: Vec<OsString> = vec!["js-lease-server".into()];
    for (flag, value) in flags {
        args.push(flag.into());
        args.push(value);
    }
    args.extend(extra.iter().map(OsString::from));
    Config::from_cli(Cli::try_parse_from(args).unwrap()).unwrap()
}

/// The refusal `assemble` gives for the agent side over `dir` with `extra` flags, after
/// `spoil` has been done to the directory.
fn refused(name: &str, extra: &[&str], spoil: impl Fn(&Path)) -> String {
    let dir = scratch(name);
    let config = config(dir.path(), extra);
    spoil(dir.path());
    let clock = SharedClock::new(ManualClock::new(Timestamp::from_millis(T0)));
    let assembled = assemble(
        config.agents,
        config.usage,
        &clock,
        &config.state_file,
        None,
    );
    match assembled {
        Err(err) => err.to_string(),
        Ok(_) => panic!("{name}: assembled"),
    }
}

/// Asserts that with the flag a start whose usage file will not open (a directory in its
/// place) is refused naming `--usage-db`. The flag, which does nothing, does not turn a broken
/// usage file into a start.
#[test]
fn with_the_flag_a_start_over_a_usage_file_that_will_not_open_is_refused() {
    let usage = refused("refused_usage", &["--estimate-feed"], |dir| {
        std::fs::create_dir(dir.join("usage.db")).unwrap();
    });
    assert!(usage.starts_with("--usage-db: "), "{usage}");
}

/// Asserts that the agent side, assembled as the server assembles it, holds a quiet heartbeat's
/// reply for the pool's heartbeat interval when that is under five seconds (1 s here) and for
/// five seconds when the interval is longer (the default, 5 s, and 30 s). It catches a server
/// that holds replies longer than its hosts' interval, which would stretch the gaps its
/// silence counting assumes, or past the agent's request limit.
#[test]
fn the_hold_of_a_reply_is_the_pools_interval_up_to_five_seconds() {
    for (timing, held) in [
        ("heartbeat_interval = \"1s\"\ngrace = \"3s\"\n", 1),
        ("", 5),
        ("heartbeat_interval = \"30s\"\ngrace = \"90s\"\n", 5),
    ] {
        let dir = scratch(&format!("hold_{held}_{}", timing.len()));
        let config = config_over(dir.path(), &[], &format!("{timing}{POOL}"));
        let clock = SharedClock::new(ManualClock::new(Timestamp::from_millis(T0)));
        let assembled = assemble(
            config.agents,
            config.usage,
            &clock,
            &config.state_file,
            None,
        );
        let agents = assembled.unwrap();
        assert_eq!(agents.hold_bound, std::time::Duration::from_secs(held));
        assert_eq!(agents.starting_hold, crate::held_beat::STARTING_HOLD);
    }
}
