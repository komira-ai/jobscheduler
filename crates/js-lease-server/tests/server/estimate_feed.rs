//! `--estimate-feed`: whether each lease's estimated end (its start plus the P90 of the
//! profiles of its keyed runs) is fed to the drain cost of a whole-node request. Off by
//! default, which is the server as it was. These tests read the flag and the key; what the
//! feed does to a reservation is tested in process, without a listener, in
//! `src/agents_estimate_tests.rs`.

use std::ffi::OsString;

use js_lease_server::{Cli, Config};

use crate::agents_support::{JOIN, POOL, agent_args, join};
use crate::config_support::{PoolText, file, load, refusal, server};
use crate::support::{Harness, Scratch, base_args, config, ids, lease_body, replace};

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
    let cli = Cli::try_parse_from(args).map_err(|err| err.to_string())?;
    Config::from_cli(cli).map_err(|err| err.to_string())
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
        let text = file(&table, &[&PoolText::joined("burst")]);
        let config = load(&scratch, &text).unwrap();
        assert_eq!(config.estimate_feed(), expected, "{name}");
    }
    let scratch = Scratch::new("estimate_feed_key_usage_off");
    let extra = "estimate_feed = true\nusage_off = true";
    let table = server(&scratch, &format!("{listeners}\n{extra}"));
    let text = file(&table, &[&PoolText::joined("burst")]);
    assert_eq!(
        refusal(&scratch, &text),
        "server.estimate_feed: not allowed with server.usage_off"
    );
}

/// Asserts the start-up path of a server with the flag, over the files a real start uses:
/// with agent registration and its usage file, a lease granted on a registered host is
/// put back held on that host by a restart that also has the flag (the lease file's
/// booking seated with its start and bound), and the host takes the lease again once it
/// says hello; the same holds with a usage sink that keeps no curves. It catches a start
/// that fails or drops a restored lease when the feed is on.
#[tokio::test]
async fn a_server_with_the_flag_puts_back_a_held_lease_after_a_restart() {
    for curves in [true, false] {
        let name = format!("estimate_feed_restart_{curves}");
        let scratch = Scratch::new(&name);
        let mut flags = agent_args(&scratch, false);
        flags.push("--estimate-feed".into());
        let first = start(scratch, flags.clone(), curves).await;
        join(&first, "burst-1").await;
        let granted = first.call("POST", "/v1/leases", Some(&lease_body())).await;
        assert_eq!(granted.json()["host"], "burst-1", "{name}");
        let Harness {
            running, scratch, ..
        } = first;
        running.shutdown().await.unwrap();

        let h = start(scratch, replace(flags, "--boot", "resume"), curves).await;
        let leases = h.call("GET", "/v1/leases", None).await.json();
        assert_eq!(leases["leases"][0]["id"], "1-1", "{name}");
        assert_eq!(leases["leases"][0]["state"], "held", "{name}");
        assert_eq!(leases["leases"][0]["host"], "burst-1", "{name}");
        join(&h, "burst-1").await;
        h.ticks.step().await;
        assert_eq!(ids(&h.call("GET", "/v1/leases", None).await), ["1-1"]);
    }
}

/// A server over `scratch` with `flags`: over its own usage file when `curves`, else over a
/// usage sink that keeps no curves.
async fn start(scratch: Scratch, flags: Vec<OsString>, curves: bool) -> Harness {
    let config = config(flags).unwrap();
    let config = match curves {
        true => config,
        false => config.with_usage_sink(std::sync::Arc::new(js_usage::NoUsage)),
    };
    Harness::over(scratch, config).await
}
