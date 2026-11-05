//! `--estimate-feed`: whether each lease's estimated end (its start plus the P90 of the
//! profiles of its keyed runs) is fed to the drain cost of a whole-node request. Off by
//! default, which is the server as it was. These tests read the flag and the key; what the
//! feed does to a reservation is tested in process, without a listener, in
//! `src/agents_estimate_tests.rs`.

use std::ffi::OsString;

use js_lease_server::{Cli, Config};

use crate::agents_support::{JOIN, POOL};
use crate::config_support::{PoolText, file, load, refusal, server};
use crate::support::{FakeHosts, Scratch, base_args, config, take_fake_hosts};

/// The flags of a server with agent registration over the agents' test pool, and `extra`
/// after them.
fn args(scratch: &Scratch, extra: &[&str]) -> Vec<OsString> {
    let mut args = base_args(scratch);
    let tokens = scratch.write("agent-tokens", &format!("{JOIN}\n"), 0o600);
    let pool = scratch.write("agent-pool.toml", POOL, 0o644);
    let flags = [
        ("--agent-token-file", tokens.into_os_string()),
        ("--agent-pool-config", pool.into_os_string()),
    ];
    for (flag, value) in flags {
        args.push(flag.into());
        args.push(value);
    }
    args.extend(extra.iter().map(OsString::from));
    args
}

fn parsed(args: Vec<OsString>) -> Result<Config, String> {
    let (args, hosts) = take_fake_hosts(args);
    let cli = Cli::try_parse_from(args).map_err(|err| err.to_string())?;
    match hosts {
        Some(path) => Config::simulated(cli, Box::new(FakeHosts::new(path))),
        None => Config::from_cli(cli),
    }
    .map_err(|err| err.to_string())
}

/// Asserts the flag's parse: absent it is off, given it is on; without
/// `--agent-pool-config` it is refused naming the flag, as the other flags of agent
/// registration are; beside `--usage-off` it is a usage error naming both. It catches a
/// feed on by default, a flag that is not read, and a flag accepted where it does nothing.
#[test]
fn the_flag_is_off_by_default_and_needs_an_agent_pool_and_usage() {
    let scratch = Scratch::new("estimate_feed_flag");
    assert!(!config(args(&scratch, &[])).unwrap().estimate_feed());
    assert!(
        config(args(&scratch, &["--estimate-feed"]))
            .unwrap()
            .estimate_feed()
    );
    assert!(!config(base_args(&scratch)).unwrap().estimate_feed());
    let mut bare = base_args(&scratch);
    bare.push("--estimate-feed".into());
    let said = parsed(bare).err().unwrap();
    assert_eq!(said, "--estimate-feed: requires --agent-pool-config");
    let said = parsed(args(&scratch, &["--estimate-feed", "--usage-off"]))
        .err()
        .unwrap();
    assert!(
        said.contains("'--estimate-feed' cannot be used with '--usage-off'"),
        "{said}"
    );
}

/// Asserts the file's key: `server.estimate_feed` is off when left out or `false`, and on
/// when `true`, for a pool whose hosts register agents; `true` beside `usage_off = true` is
/// refused naming the key, and `false` beside it is accepted. A static pool and a bad value
/// are refused in `config_one_file`. It catches a key that is not read and one accepted
/// where it does nothing.
#[test]
fn the_key_is_read_from_the_file_and_refused_beside_usage_off() {
    let listeners = "listen = \"127.0.0.1:0\"\nagent_listen = \"127.0.0.2:0\"";
    for (name, extra, expected) in [
        ("estimate_feed_key_absent", "", false),
        ("estimate_feed_key_false", "estimate_feed = false", false),
        ("estimate_feed_key_true", "estimate_feed = true", true),
        (
            "estimate_feed_key_false_off",
            "estimate_feed = false\nusage_off = true",
            false,
        ),
    ] {
        let scratch = Scratch::new(name);
        let table = server(&scratch, &format!("{listeners}\n{extra}"));
        let text = file(&table, &[&PoolText::registering("burst")]);
        let config = load(&scratch, &text).unwrap();
        assert_eq!(config.estimate_feed(), expected, "{name}");
    }
    let scratch = Scratch::new("estimate_feed_key_usage_off");
    let extra = "estimate_feed = true\nusage_off = true";
    let table = server(&scratch, &format!("{listeners}\n{extra}"));
    let text = file(&table, &[&PoolText::registering("burst")]);
    assert_eq!(
        refusal(&scratch, &text),
        "server.estimate_feed: not allowed with server.usage_off"
    );
}
