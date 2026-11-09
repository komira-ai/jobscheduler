//! The fixtures of the placement tests: the configuration read from the flags, the agent side
//! assembled from it as the server assembles it, and hosts registered with the registry. No
//! listener is bound and no connection is made.

use std::ffi::OsString;
use std::path::Path;
use std::sync::Arc;

use js_agent_wire::Hello;
use js_registry::Credential;
use js_testkit::dir::TestDir;
use js_types::{ManualClock, Timestamp};
use serde_json::json;

use crate::agents::{Agents, assemble};
use crate::clock::SharedClock;
use crate::config::{Cli, Config};

/// The join token of the pool.
pub(super) const JOIN: &str = "join-token-A-0123456789-abcdefghijklmnop";

/// A pool of up to two machines with a node report: the scheduler describes a host by
/// its `arch`.
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
/// guard is dropped. Its path carries the process id and a counter beside `name`, so two
/// runs of one test at once (two test binaries, or a test run twice) never share it.
pub(super) fn scratch(name: &str) -> TestDir {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/placement_wiring");
    TestDir::under(root, name)
}

pub(super) fn write(dir: &Path, name: &str, text: &str, mode: u32) -> OsString {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    std::fs::write(&path, text).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
    path.into_os_string()
}

/// The configuration of a server with agent registration over the agent pool `pool` and
/// `extra` flags.
pub(super) fn config_of(dir: &Path, pool: &str, extra: &[&str]) -> Config {
    config_booted(dir, pool, extra, "initialise")
}

/// [`config_of`], booting as `boot` says (`initialise` or `resume`).
pub(super) fn config_booted(dir: &Path, pool: &str, extra: &[&str], boot: &str) -> Config {
    config_result(dir, pool, extra, boot).unwrap()
}

/// [`config_booted`], or the refusal of the configuration.
pub(super) fn config_result(
    dir: &Path,
    pool: &str,
    extra: &[&str],
    boot: &str,
) -> Result<Config, crate::StartupError> {
    let flags = [
        ("--listen", "127.0.0.1:0".into()),
        ("--state-file", dir.join("leases.toml").into_os_string()),
        ("--boot", boot.into()),
        (
            "--pool-config",
            write(dir, "pool.toml", js_pool_static::EXAMPLE_CONFIG, 0o644),
        ),
        ("--token-file", write(dir, "token", &"t".repeat(40), 0o600)),
        (
            "--agent-token-file",
            write(dir, "agent-tokens", &format!("{JOIN}\n"), 0o600),
        ),
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
    Config::from_cli(Cli::try_parse_from(args).unwrap())
}

pub(super) fn hello(host: &str) -> Hello {
    let body = json!({
        "host_id": host,
        "pool": "burst",
        "agent_version": "0.1.0",
        "shape": { "cpu_millis": 8000, "memory_bytes": 17179869184u64, "gpus": 0 },
        "capabilities": { "arch": "x86_64" },
        "labels": {},
        "phase": "serving",
    });
    serde_json::from_value(body).unwrap()
}

/// The agent side of a server and the directory of its files. The fields drop in order, so
/// the directory goes after what writes into it.
struct Served {
    _agents: Arc<Agents>,
    _dir: TestDir,
}

/// The agent side of a server over [`POOL`] with `extra` flags, assembled as the server
/// assembles it, with hosts `h1` and `h2` registered.
fn serving(name: &str, extra: &[&str]) -> Served {
    serving_of(name, POOL, extra)
}

/// [`serving`] over the agent pool `pool`.
fn serving_of(name: &str, pool: &str, extra: &[&str]) -> Served {
    let dir = scratch(name);
    let config = config_of(dir.path(), pool, extra);
    let clock = SharedClock::new(ManualClock::new(Timestamp::from_millis(1_000_000)));
    let (_source, agents) =
        assemble(config.hosts, config.usage, &clock, &config.state_file, None).unwrap();
    let agents = agents.unwrap();
    let credential = Credential::of_token(JOIN);
    for host in ["h1", "h2"] {
        agents.registry.hello(&hello(host), &credential).unwrap();
    }
    Served {
        _agents: agents,
        _dir: dir,
    }
}

/// Asserts that two fixtures made under one name at once have two directories, and that
/// dropping one leaves the other's files. It catches a scratch path made of the name alone,
/// which two runs of one test would share and remove under each other.
#[test]
fn fixtures_of_one_name_have_their_own_directories() {
    let first = scratch("same");
    let second = scratch("same");
    assert_ne!(first.path(), second.path());
    let file = write(first.path(), "kept", "x", 0o600);
    drop(second);
    assert!(Path::new(&file).is_file());
    let one = serving("same_served", &[]);
    let two = serving("same_served", &[]);
    assert_ne!(one._dir.path(), two._dir.path());
    let state = one._dir.path().join("leases.toml");
    drop(two);
    assert!(
        one._dir.path().join("agent-tokens").is_file(),
        "{}",
        state.display()
    );
}
