//! `--placement` and `server.placement`, deprecated: `js-sched` places every lease on a
//! host that registered itself, so the flag and the key choose nothing. `js-sched` is
//! accepted with a warning, `registry` (the removed first-free placement) is refused at
//! start, and a pool the scheduler cannot take as a node (no `arch`) is refused whatever
//! is given. These tests read the flag and the key; where the leases land is tested in
//! process, without a listener, in `src/sched/wiring_tests.rs`, `src/sched/engine_tests.rs`
//! and `src/sched/engine_cases_tests.rs` (which also covers the routes).

use std::ffi::OsString;

use js_lease_server::{Cli, Config};

use crate::agents_support::JOIN;
use crate::config_support::{PoolText, file, load, quote, quoted, refusal, server};
use crate::support::{Scratch, base_args, config};

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

/// The warning `--placement js-sched` leaves.
const FLAG_WARNING: &str =
    "--placement is deprecated and does nothing: js-sched is the only placement (drop the flag)";

/// The warning `server.placement = "js-sched"` leaves.
const KEY_WARNING: &str = "server.placement is deprecated and does nothing: js-sched is the only placement (drop the key)";

/// The flags of a server with agent registration over `pool`, and `extra` after them.
fn args_over(scratch: &Scratch, pool: &str, extra: &[&str]) -> Vec<OsString> {
    let mut args = base_args(scratch);
    let tokens = scratch.write("agent-tokens", &format!("{JOIN}\n"), 0o600);
    let pool = scratch.write("agent-pool.toml", pool, 0o644);
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

/// The flags of a server with agent registration over [`POOL`], and `extra` after them.
fn args(scratch: &Scratch, extra: &[&str]) -> Vec<OsString> {
    args_over(scratch, POOL, extra)
}

/// An elastic pool whose hosts register agents; its floor names no capability.
fn elastic_pool(scratch: &Scratch) -> PoolText {
    let joins = scratch.write("joins", &format!("{JOIN}\n"), 0o600);
    PoolText::elastic(scratch)
        .set("name", &quote("burst"))
        .set("class", &quote("gpu-host"))
        .set("exec", &quote("none"))
        .set("join_token_file", &quoted(&joins))
}

fn parsed(args: Vec<OsString>) -> Result<Config, String> {
    let cli = Cli::try_parse_from(args).map_err(|err| err.to_string())?;
    Config::from_cli(cli).map_err(|err| err.to_string())
}

/// The file of a server over a joined pool whose capabilities are `caps`, with `extra`
/// in `[server]`.
fn joined_file(scratch: &Scratch, caps: &str, extra: &str) -> String {
    let pool = PoolText::joined("burst").set("capabilities", caps);
    let listeners = "listen = \"127.0.0.1:0\"\nagent_listen = \"127.0.0.2:0\"";
    let table = server(scratch, &format!("{listeners}\n{extra}"));
    file(&table, &[&pool])
}

/// Asserts that with neither the flag nor the key the server starts over a pool with an
/// `arch` and warns of nothing about placement: `js-sched` is the default, and there is
/// no other.
#[test]
fn placement_defaults_to_js_sched() {
    let scratch = Scratch::new("placement_default");
    let flagless = config(args(&scratch, &[])).unwrap();
    assert!(flagless.warnings().is_empty(), "{:?}", flagless.warnings());
    let text = joined_file(&scratch, "{ arch = \"x86_64\" }", "");
    let keyless = load(&scratch, &text).unwrap();
    assert!(keyless.warnings().is_empty(), "{:?}", keyless.warnings());
}

/// Asserts that `--placement js-sched` and `server.placement = "js-sched"` are accepted
/// and change nothing but one warning, given in full, naming the flag or the key to drop:
/// the CPU slack is that of the same server without them, and a
/// `--cpu-slack` (once refused unless `js-sched` was named) is read with or without it.
#[test]
fn placement_js_sched_is_accepted_and_does_nothing_more() {
    let scratch = Scratch::new("placement_js_sched");
    let plain = config(args(&scratch, &[])).unwrap();
    let named = config(args(&scratch, &["--placement", "js-sched"])).unwrap();
    assert_eq!(named.warnings(), [FLAG_WARNING]);
    assert_eq!(named.cpu_slack(), plain.cpu_slack());
    let slack = config(args(&scratch, &["--cpu-slack", "1.5"])).unwrap();
    let both = ["--placement", "js-sched", "--cpu-slack", "1.5"];
    let slack_named = config(args(&scratch, &both)).unwrap();
    assert_eq!(slack.cpu_slack().to_string(), "1.5");
    assert_eq!(slack_named.cpu_slack(), slack.cpu_slack());
    assert!(slack.warnings().is_empty(), "{:?}", slack.warnings());
    assert_eq!(slack_named.warnings(), [FLAG_WARNING]);

    let caps = "{ arch = \"x86_64\" }";
    let keyless = load(&scratch, &joined_file(&scratch, caps, "")).unwrap();
    let text = joined_file(&scratch, caps, "placement = \"js-sched\"");
    let keyed = load(&scratch, &text).unwrap();
    assert_eq!(keyed.warnings(), [KEY_WARNING]);
    assert_eq!(keyed.cpu_slack(), keyless.cpu_slack());
}

/// Asserts that `registry` is refused, by the flag and by the key, with the whole message
/// naming the flag or key and saying to drop it; that the flag still needs
/// `--agent-pool-config` (refused naming that first, as every flag of agent registration
/// is); that any other value is a usage error or a refusal naming the key, which no
/// longer lists `registry` as a value; and that beside a pool refused for want of an
/// `arch`, `registry` is the refusal named, by the flag and by the key.
#[test]
fn placement_registry_is_refused_with_a_clear_message() {
    let scratch = Scratch::new("placement_registry");
    let said = parsed(args(&scratch, &["--placement", "registry"]))
        .err()
        .unwrap();
    assert_eq!(
        said,
        "--placement: the registry placement mode was removed; js-sched is the only placement (drop the flag)"
    );
    let said = parsed(args(&scratch, &["--placement", "elsewhere"]))
        .err()
        .unwrap();
    assert!(
        said.contains("invalid value 'elsewhere' for '--placement"),
        "{said}"
    );
    assert!(said.contains("[possible values: js-sched]"), "{said}");
    for value in ["js-sched", "registry"] {
        let mut bare = base_args(&scratch);
        bare.extend(["--placement", value].map(OsString::from));
        let said = parsed(bare).err().unwrap();
        assert_eq!(said, "--placement: requires --agent-pool-config");
    }

    let caps = "{ arch = \"x86_64\" }";
    let text = joined_file(&scratch, caps, "placement = \"registry\"");
    assert_eq!(
        refusal(&scratch, &text),
        "server.placement: the registry placement mode was removed; js-sched is the only placement (drop the key)"
    );
    let text = joined_file(&scratch, caps, "placement = \"elsewhere\"");
    assert_eq!(
        refusal(&scratch, &text),
        "server.placement: must be js-sched"
    );

    // Beside a pool that is refused too (no `arch`), `registry` is the refusal named.
    let bare = POOL.replace("arch = \"x86_64\"", "isa = \"x86-64-v3\"");
    let said = parsed(args_over(&scratch, &bare, &["--placement", "registry"]))
        .err()
        .unwrap();
    assert_eq!(
        said,
        "--placement: the registry placement mode was removed; js-sched is the only placement (drop the flag)"
    );
    let text = joined_file(
        &scratch,
        "{ isa = \"x86-64-v3\" }",
        "placement = \"registry\"",
    );
    assert_eq!(
        refusal(&scratch, &text),
        "server.placement: the registry placement mode was removed; js-sched is the only placement (drop the key)"
    );
}

/// Asserts that an agent or joined pool whose shape the scheduler cannot take as a node
/// (no `arch`) is refused with no flag and no key, naming the pool's flag
/// (`--agent-pool-config`) or the pool of the file, and the missing `arch`: with `js-sched`
/// the only placement, no lease could be placed on its hosts. Giving `--placement
/// js-sched` does not change the refusal. An elastic pool, whose floor names no `arch`
/// either, starts with no warning: its hosts are placed by the `arch` their agents report
/// at hello. It catches an arch-less agent or joined pool let through, and an elastic pool
/// refused for the floor its hosts are not placed by.
#[test]
fn a_pool_with_no_arch_is_refused_with_no_flag_but_not_an_elastic_one() {
    let scratch = Scratch::new("placement_no_arch");
    let bare = POOL.replace("arch = \"x86_64\"", "isa = \"x86-64-v3\"");
    let expected = "--agent-pool-config: the pool's shape cannot be taken as a node by the scheduler (shape does not describe a node: the node report has no \"arch\" entry); give the pool capabilities with an arch";
    let said = parsed(args_over(&scratch, &bare, &[])).err().unwrap();
    assert_eq!(said, expected);
    let named = args_over(&scratch, &bare, &["--placement", "js-sched"]);
    assert_eq!(parsed(named).err().unwrap(), expected);

    let text = joined_file(&scratch, "{ isa = \"x86-64-v3\" }", "");
    let said = refusal(&scratch, &text);
    assert_eq!(
        said,
        "pool \"burst\": the pool's shape cannot be taken as a node by the scheduler (shape does not describe a node: the node report has no \"arch\" entry); give the pool capabilities with an arch"
    );
    let listeners = "listen = \"127.0.0.1:0\"\nagent_listen = \"127.0.0.2:0\"";
    let table = server(&scratch, listeners);
    let elastic = load(&scratch, &file(&table, &[&elastic_pool(&scratch)])).unwrap();
    assert!(elastic.warnings().is_empty(), "{:?}", elastic.warnings());
}
