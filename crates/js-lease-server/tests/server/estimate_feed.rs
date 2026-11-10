//! `--estimate-feed`: deprecated, and does nothing, because no lease is costed by an
//! estimate any more. The flag and the key are still accepted, with a warning given in
//! full, and keep their refusals (beside `--usage-off`, without an agent pool). These
//! tests read the flag and the key; the start over files that will not open is tested in
//! process, without a listener, in `src/agents_estimate_tests.rs`.

use std::ffi::OsString;

use js_lease_server::{Cli, Config};

use crate::agents_support::{JOIN, POOL};
use crate::config_support::{PoolText, file, load, refusal, server};
use crate::support::{Scratch, base_args, config};

/// The flags of a server with agent registration over the agents' test pool, and `extra`
/// after them.
fn args(scratch: &Scratch, extra: &[&str]) -> Vec<OsString> {
    let mut args = base_args(scratch);
    let tokens = scratch.write("agent-tokens", &format!("{JOIN}\n"), 0o600);
    scratch.write("agent-pool.toml", POOL, 0o644);
    let flags = [("--agent-token-file", tokens.into_os_string())];
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

/// The warning the flag leaves.
const FLAG_WARNING: &str = "--estimate-feed is deprecated and does nothing: no lease is costed by an estimate any more (drop the flag)";

/// The warning the key leaves.
const KEY_WARNING: &str = "server.estimate_feed is deprecated and does nothing: no lease is costed by an estimate any more (drop the key)";

/// Asserts the flag's parse: absent it leaves no warning; given it starts with the warning
/// in full, and the same server otherwise (the CPU slack of the same flags); without
/// `--agent-pool-config` it is refused naming the flag, as the other flags of agent
/// registration are; beside `--usage-off` it is a usage error naming both.
#[test]
fn the_flag_warns_and_changes_nothing_and_needs_an_agent_pool_and_usage() {
    let scratch = Scratch::new("estimate_feed_flag");
    let plain = config(args(&scratch, &[])).unwrap();
    assert!(plain.warnings().is_empty(), "{:?}", plain.warnings());
    let fed = config(args(&scratch, &["--estimate-feed"])).unwrap();
    assert_eq!(fed.warnings(), [FLAG_WARNING]);
    assert_eq!(fed.cpu_slack(), plain.cpu_slack());
    assert_eq!(fed.usage_db(), plain.usage_db());
    let mut bare = crate::support::without_agent_pool(&scratch);
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

/// Asserts the file's key: `server.estimate_feed` leaves no warning when left out or
/// `false`, and the warning in full, naming the key, when `true`, for a pool whose hosts
/// register agents; `true` beside `usage_off = true` is refused naming the key, and `false`
/// beside it is accepted. A bad value is refused in `config_one_file`.
#[test]
fn the_key_warns_when_on_and_is_refused_beside_usage_off() {
    let listeners = "listen = \"127.0.0.1:0\"\nagent_listen = \"127.0.0.2:0\"";
    for (name, extra, warns) in [
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
        let expected: &[&str] = if warns { &[KEY_WARNING] } else { &[] };
        assert_eq!(config.warnings(), expected, "{name}");
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
