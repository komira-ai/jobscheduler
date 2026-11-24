//! `--pack-limit` and `server.pack_limit`: how full the placement engine packs a host
//! before it places on the least full one. These tests read the flag and the key and
//! their refusals; what the limit does to a placement is tested on the job engine, in
//! `crates/js-sched-placer/src/engine_pack_tests.rs`.

use std::ffi::OsString;

use js_lease_server::{Cli, Config, PackLimit};

use crate::agents_support::JOIN;
use crate::config_support::{PoolText, file, load, refusal, server};
use crate::support::{Scratch, base_args, config};

/// A pool of up to two machines whose capabilities name an `arch`, as the scheduler needs.
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

/// The refusal of `given` as a limit, after what names the flag or key.
fn out_of_range(given: &str) -> String {
    format!(
        "pack_limit must be a fraction of the host's capacity from 0.0 to 1.0, with at most three decimals, such as 0.8; got {given:?}"
    )
}

/// Asserts the flag's parse: absent it is js-sched's default (0.8, read from js-sched and
/// not written here); `0.0`, `0.65` and `1.0` are read as thousandths of the capacity. It
/// catches a flag that is not read and a default restated here that drifts.
#[test]
fn the_flag_is_read_and_defaults_to_the_engines_limit() {
    let scratch = Scratch::new("pack_limit_flag");
    let none = config(args(&scratch, &[])).unwrap();
    let default = js_sched::LoadConfig::default().pack_limit_permille;
    assert_eq!(none.pack_limit().permille(), default);
    assert_eq!(none.pack_limit(), PackLimit::default());
    for (given, permille) in [("0.0", 0), ("0.65", 650), ("1.0", 1000)] {
        let read = config(args(&scratch, &["--pack-limit", given])).unwrap();
        assert_eq!(read.pack_limit().permille(), permille, "{given}");
    }
    let flagless = config(base_args(&scratch)).unwrap();
    assert_eq!(flagless.pack_limit(), PackLimit::default());
}

/// Asserts that a limit above 1.0, not a number or empty is refused at start, by the flag
/// and by the key, each error naming `pack_limit`; and that the flag without
/// `--agent-pool-config` is refused naming that one. It catches a parse that accepts any of
/// them, a value clamped instead of refused, and a limit accepted where nothing reads it.
#[test]
fn pack_limit_values_outside_the_range_are_refused() {
    let scratch = Scratch::new("pack_limit_refused");
    for bad in ["1.5", "abc", "2", ""] {
        let said = parsed(args(&scratch, &["--pack-limit", bad]))
            .err()
            .unwrap();
        assert!(
            said.contains("'--pack-limit <PACK_LIMIT>'"),
            "{bad}: {said}"
        );
        assert!(said.contains(&out_of_range(bad)), "{bad}: {said}");
    }
    let mut bare = crate::support::without_agent_pool(&scratch);
    bare.extend(["--pack-limit", "0.8"].map(OsString::from));
    let said = parsed(bare).err().unwrap();
    assert_eq!(said, "--pack-limit: requires --agent-pool-config");

    let listeners = "listen = \"127.0.0.1:0\"\nagent_listen = \"127.0.0.2:0\"";
    let pool = PoolText::joined("burst");
    let text_of = |scratch: &Scratch, extra: &str| {
        let table = server(scratch, &format!("{listeners}\n{extra}"));
        file(&table, &[&pool])
    };
    for (value, given) in [
        ("1.5", "1.5"),
        ("2", "2"),
        ("nan", "NaN"),
        ("0.0001", "0.0001"),
    ] {
        let scratch = Scratch::new("pack_limit_key_range");
        let text = text_of(&scratch, &format!("pack_limit = {value}"));
        let expected = format!("server.pack_limit: {}", out_of_range(given));
        assert_eq!(refusal(&scratch, &text), expected, "{value}");
    }
    for value in ["\"abc\"", "\"\"", "\"0.8\"", "true"] {
        let scratch = Scratch::new("pack_limit_key_type");
        let text = text_of(&scratch, &format!("pack_limit = {value}"));
        let expected = "server.pack_limit: must be a number such as 0.8";
        assert_eq!(refusal(&scratch, &text), expected, "{value}");
    }
}

/// Asserts the file's key: absent it is js-sched's default; `0.8`, `0.0`, `0.65` and `1`
/// are read, a whole number as a fraction too. It catches a key that is not read.
#[test]
fn the_key_is_read_from_the_file() {
    let listeners = "listen = \"127.0.0.1:0\"\nagent_listen = \"127.0.0.2:0\"";
    let pool = PoolText::joined("burst");
    let default = PackLimit::default().permille();
    for (name, extra, permille) in [
        ("pack_limit_key_off", "", default),
        ("pack_limit_key_0_8", "pack_limit = 0.8", 800),
        ("pack_limit_key_0_0", "pack_limit = 0.0", 0),
        ("pack_limit_key_0_65", "pack_limit = 0.65", 650),
        ("pack_limit_key_1", "pack_limit = 1", 1000),
    ] {
        let scratch = Scratch::new(name);
        let extra = format!("{listeners}\n{extra}");
        let text = file(&server(&scratch, &extra), &[&pool]);
        let config = load(&scratch, &text).unwrap();
        assert_eq!(config.pack_limit().permille(), permille, "{name}");
    }
}
