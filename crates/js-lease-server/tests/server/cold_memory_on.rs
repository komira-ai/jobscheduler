//! `--cold-memory-bytes-on`, the cold memory of the jobs of an operating system, read with the
//! rest of the agent registration flags; what it books is tested in process, in
//! `src/sched/cold_tests.rs`.

use std::ffi::OsString;

use js_lease_server::{Cli, Config};

use crate::agents_support::JOIN;
use crate::support::{Scratch, base_args};

/// A pool of Macs, whose capabilities name an `arch` and an `os`.
const POOL: &str = r#"
name = "macs"
class = "burst-host"
max_machines = 2

[shape]
cpu_millis = 28000
memory_bytes = 103079215104

[capabilities]
arch = "arm64"
os = "macos"
"#;

/// The flags of a server with agent registration over [`POOL`], and `extra` after them.
fn args(scratch: &Scratch, extra: &[&str]) -> Vec<OsString> {
    let mut args = base_args(scratch);
    let tokens = scratch.write("agent-tokens", &format!("{JOIN}\n"), 0o600);
    scratch.write("agent-pool.toml", POOL, 0o644);
    args.extend(["--agent-token-file".into(), tokens.into_os_string()]);
    args.extend(extra.iter().map(OsString::from));
    args
}

/// The configuration of `args`, or what refused it.
fn parsed(args: Vec<OsString>) -> Result<Config, String> {
    let cli = Cli::try_parse_from(args).map_err(|err| err.to_string())?;
    Config::from_cli(cli).map_err(|err| err.to_string())
}

/// Asserts that a server started with one cold memory for each of two systems starts, and
/// that one given twice for the same system is refused at start naming the flag and the
/// system. Catches a later entry silently taking the place of an earlier one.
#[test]
fn an_os_given_twice_is_refused_and_two_systems_are_taken() {
    let scratch = Scratch::new("cold_memory_on_flags");
    let two = [
        "--cold-memory-bytes-on",
        "macos=4294967296",
        "--cold-memory-bytes-on",
        "linux=8589934592",
    ];
    assert!(parsed(args(&scratch, &two)).is_ok());
    let twice = [
        "--cold-memory-bytes-on",
        "macos=4294967296",
        "--cold-memory-bytes-on",
        "macos=8589934592",
    ];
    assert_eq!(
        parsed(args(&scratch, &twice)).err(),
        Some("--cold-memory-bytes-on: macos is given twice".to_owned())
    );
}
