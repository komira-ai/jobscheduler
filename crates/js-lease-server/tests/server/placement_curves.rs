//! `--placement-curves` and `server.placement_curves`: whether the placement engine learns
//! per-class profiles in shadow. These tests read the flag and the key and their refusals;
//! what the engine does with the mode is tested in `js-sched-placer`.

use std::ffi::OsString;

use js_lease_server::{Cli, Config};
use js_sched_placer::sizes::Curves;

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
    args.push("--agent-token-file".into());
    args.push(tokens.into_os_string());
    args.extend(extra.iter().map(OsString::from));
    args
}

fn parsed(args: Vec<OsString>) -> Result<Config, String> {
    let cli = Cli::try_parse_from(args).map_err(|err| err.to_string())?;
    Config::from_cli(cli).map_err(|err| err.to_string())
}

/// The refusal of `given` as a mode, after what names the flag or key.
fn refused_value(given: &str) -> String {
    format!("placement_curves must be off or shadow; got {given:?}")
}

/// Asserts the flag's parse: absent it is `off`, and `off` and `shadow` are read as given.
/// Catches a flag that is not read and a default other than `off`.
#[test]
fn the_flag_is_read_and_defaults_to_off() {
    let scratch = Scratch::new("curves_flag");
    let none = config(args(&scratch, &[])).unwrap();
    assert_eq!(none.placement_curves(), Curves::Off);
    for (given, mode) in [("off", Curves::Off), ("shadow", Curves::Shadow)] {
        let read = config(args(&scratch, &["--placement-curves", given])).unwrap();
        assert_eq!(read.placement_curves(), mode, "{given}");
    }
}

/// Asserts that `on`, a capitalised value and an empty one are refused by the flag with the
/// message naming `off` and `shadow` and quoting the value given, and that the flag without
/// `--agent-pool-config` is refused naming that one. Catches a parse that accepts `on` and a
/// flag accepted where nothing reads it.
#[test]
fn the_flag_refuses_any_other_value_and_a_start_with_no_pool() {
    let scratch = Scratch::new("curves_flag_refused");
    for bad in ["on", "Shadow", ""] {
        let said = parsed(args(&scratch, &["--placement-curves", bad]))
            .err()
            .unwrap();
        assert!(
            said.contains("'--placement-curves <PLACEMENT_CURVES>'"),
            "{bad}: {said}"
        );
        assert!(said.contains(&refused_value(bad)), "{bad}: {said}");
    }
    let mut bare = crate::support::without_agent_pool(&scratch);
    bare.extend(["--placement-curves", "shadow"].map(OsString::from));
    let said = parsed(bare).err().unwrap();
    assert_eq!(said, "--placement-curves: requires --agent-pool-config");
}

/// Asserts the key's refusals, each by the whole message: `on` and `Shadow` name the two
/// accepted values, and a value that is not a string is told so. Catches a key that takes
/// `on`, and a number or boolean read as text.
#[test]
fn the_key_refuses_any_other_value() {
    let listeners = "listen = \"127.0.0.1:0\"\nagent_listen = \"127.0.0.2:0\"";
    let pool = PoolText::joined("burst");
    for (value, given) in [("\"on\"", "on"), ("\"Shadow\"", "Shadow"), ("\"\"", "")] {
        let scratch = Scratch::new("curves_key_value");
        let table = server(
            &scratch,
            &format!("{listeners}\nplacement_curves = {value}"),
        );
        let text = file(&table, &[&pool]);
        let expected = format!("server.placement_curves: {}", refused_value(given));
        assert_eq!(refusal(&scratch, &text), expected, "{value}");
    }
    for value in ["true", "1"] {
        let scratch = Scratch::new("curves_key_type");
        let table = server(
            &scratch,
            &format!("{listeners}\nplacement_curves = {value}"),
        );
        let text = file(&table, &[&pool]);
        let expected = "server.placement_curves: must be a string";
        assert_eq!(refusal(&scratch, &text), expected, "{value}");
    }
}

/// Asserts the file's key: absent it is `off`; `"off"` and `"shadow"` are read as given.
/// Catches a key that is not read.
#[test]
fn the_key_is_read_from_the_file() {
    let listeners = "listen = \"127.0.0.1:0\"\nagent_listen = \"127.0.0.2:0\"";
    let pool = PoolText::joined("burst");
    for (name, extra, mode) in [
        ("curves_key_absent", "", Curves::Off),
        ("curves_key_off", "placement_curves = \"off\"", Curves::Off),
        (
            "curves_key_shadow",
            "placement_curves = \"shadow\"",
            Curves::Shadow,
        ),
    ] {
        let scratch = Scratch::new(name);
        let extra = format!("{listeners}\n{extra}");
        let text = file(&server(&scratch, &extra), &[&pool]);
        let config = load(&scratch, &text).unwrap();
        assert_eq!(config.placement_curves(), mode, "{name}");
    }
}
