//! `--placement`: who picks the host of a lease on a host that registered itself. The
//! default is the registry, which is the server as it was; `js-sched` has the scheduler
//! choose among the hosts with room and lets leases of a class that books part of a host
//! share one. These tests read the flag and the key; where the leases land is tested in
//! process, without a listener, in `src/sched/wiring_tests.rs`.

use std::ffi::OsString;

use js_lease_server::{Cli, Config, Placement};

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

/// The flags of a server with agent registration over [`POOL`], and `extra` after them.
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

/// Asserts the flag's parse: absent it is the registry; `registry` and `js-sched` are
/// read; any other value is a usage error; and without `--agent-pool-config` it is refused
/// naming the flag, as the other flags of agent registration are.
#[test]
fn the_flag_is_read_and_needs_an_agent_pool() {
    let scratch = Scratch::new("placement_flag");
    let none = config(args(&scratch, &[])).unwrap();
    assert_eq!(none.placement(), Placement::Registry);
    let sched = config(args(&scratch, &["--placement", "js-sched"])).unwrap();
    assert_eq!(sched.placement(), Placement::JsSched);
    let registry = config(args(&scratch, &["--placement", "registry"])).unwrap();
    assert_eq!(registry.placement(), Placement::Registry);
    let said = parsed(args(&scratch, &["--placement", "elsewhere"]))
        .err()
        .unwrap();
    assert!(
        said.contains("invalid value 'elsewhere' for '--placement"),
        "{said}"
    );
    let mut bare = base_args(&scratch);
    bare.extend(["--placement", "js-sched"].map(OsString::from));
    let said = parsed(bare).err().unwrap();
    assert_eq!(said, "--placement: requires --agent-pool-config");
    let flagless = config(base_args(&scratch)).unwrap();
    assert_eq!(flagless.placement(), Placement::Registry);
}

/// An elastic pool whose hosts register agents.
fn pool(scratch: &Scratch) -> PoolText {
    let joins = scratch.write("joins", &format!("{JOIN}\n"), 0o600);
    PoolText::elastic(scratch)
        .set("name", &quote("burst"))
        .set("class", &quote("gpu-host"))
        .set("exec", &quote("none"))
        .set("join_token_file", &quoted(&joins))
}

/// Asserts the file's key: `server.placement` is the registry when left out or named, and
/// `js-sched` when the file says so, for a pool whose capabilities name an `arch` (a joined
/// pool here); the refusal of one that names none is tested below, and the bad values in
/// `config_one_file`.
#[test]
fn the_key_is_read_from_the_file() {
    let arch = |_: &Scratch| PoolText::joined("burst").set("capabilities", "{ arch = \"x86_64\" }");
    for (name, extra, expected) in [
        ("placement_key_off", "", Placement::Registry),
        (
            "placement_key_reg",
            "placement = \"registry\"",
            Placement::Registry,
        ),
        (
            "placement_key_sched",
            "placement = \"js-sched\"",
            Placement::JsSched,
        ),
    ] {
        let scratch = Scratch::new(name);
        let listeners = "listen = \"127.0.0.1:0\"\nagent_listen = \"127.0.0.2:0\"";
        let table = server(&scratch, &format!("{listeners}\n{extra}"));
        let text = file(&table, &[&arch(&scratch)]);
        let config = load(&scratch, &text).unwrap();
        assert_eq!(config.placement(), expected);
    }
}

/// Asserts that `js-sched` is refused at start for a pool whose shape the scheduler cannot
/// take as a node (no `arch`), by the flag and by the key, naming the flag or key and the
/// missing `arch`; and that the same pool is accepted by the registry. Without the refusal
/// every start would fall back to the first host and the flag would do nothing.
#[test]
fn js_sched_is_refused_for_a_pool_with_no_arch() {
    let scratch = Scratch::new("placement_no_arch");
    let mut flags = args(&scratch, &["--placement", "js-sched"]);
    let at = flags
        .iter()
        .position(|arg| arg == "--agent-pool-config")
        .unwrap();
    let bare = POOL.replace("arch = \"x86_64\"", "isa = \"x86-64-v3\"");
    flags[at + 1] = scratch
        .write("bare-pool.toml", &bare, 0o644)
        .into_os_string();
    let said = parsed(flags.clone()).err().unwrap();
    assert!(said.starts_with("--placement: "), "{said}");
    assert!(said.contains("arch"), "{said}");
    let at = flags.iter().position(|arg| arg == "--placement").unwrap();
    flags[at + 1] = "registry".into();
    assert!(parsed(flags).is_ok());

    let listeners = "listen = \"127.0.0.1:0\"\nagent_listen = \"127.0.0.2:0\"";
    let table = server(&scratch, &format!("{listeners}\nplacement = \"js-sched\""));
    let text = file(&table, &[&pool(&scratch)]);
    let said = refusal(&scratch, &text);
    assert!(said.starts_with("server.placement: "), "{said}");
    assert!(said.contains("arch"), "{said}");
}
