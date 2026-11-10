//! `--estimate-feed` beside files that will not open, in process: the configuration read
//! from the flags and the agent side assembled from it as the server assembles it. No listener is
//! bound and no connection is made.

use std::ffi::OsString;
use std::path::Path;

use js_testkit::dir::TestDir;
use js_types::{ManualClock, Timestamp};

use crate::agents::assemble;
use crate::clock::SharedClock;
use crate::config::{Cli, Config};

/// The join token of the pool.
const JOIN: &str = "join-token-A-0123456789-abcdefghijklmnop";

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
    let flags = [
        ("--listen", "127.0.0.1:0".into()),
        ("--state-file", dir.join("state").into_os_string()),
        ("--boot", "initialise".into()),
        ("--token-file", write(dir, "token", &"t".repeat(40), 0o600)),
        (
            "--agent-token-file",
            write(dir, "agent-tokens", &format!("{JOIN}\n"), 0o600),
        ),
        (
            "--agent-pool-config",
            write(dir, "agent-pool.toml", POOL, 0o644),
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
/// place) is refused naming `--usage-db`, and one whose file of join tokens cannot be read
/// (a directory in its place) is refused. The flag, which does nothing, does not turn a
/// broken usage file into a start.
#[test]
fn with_the_flag_a_start_over_files_that_will_not_open_is_refused() {
    let usage = refused("refused_usage", &["--estimate-feed"], |dir| {
        std::fs::create_dir(dir.join("usage.db")).unwrap();
    });
    assert!(usage.starts_with("--usage-db: "), "{usage}");
    let joins = refused("refused_joins", &["--estimate-feed"], |dir| {
        std::fs::create_dir(dir.join("state.joins")).unwrap();
    });
    assert!(!joins.is_empty());
    assert!(!joins.starts_with("--usage-db"), "{joins}");
}
