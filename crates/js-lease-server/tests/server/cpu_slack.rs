//! `--cpu-slack` and `server.cpu_slack`: how far above a host's cores the placement
//! engine may charge it. These tests read the flag and the key and their refusals; what the
//! slack does to a start is tested in process, in `src/sched/engine_tests.rs`.

use std::ffi::OsString;

use js_lease_server::{Cli, Config, CpuSlack};

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

/// The refusal of `given` as a slack, after what names the flag or key.
fn out_of_range(given: &str) -> String {
    format!(
        "cpu_slack must be a multiplier of the cores from 1.0 to 2.0, with at most three decimals, such as 1.1; got {given:?}"
    )
}

/// Asserts the flag's parse: absent it is js-sched's default (1.1x, read from js-sched and
/// not written here); `1.0`, `1.25` and `2.0` are read as thousandths above the cores. It
/// catches a flag that is not read and a default restated here that drifts.
#[test]
fn the_flag_is_read_and_defaults_to_the_engines_slack() {
    let scratch = Scratch::new("cpu_slack_flag");
    let none = config(args(&scratch, &[])).unwrap();
    let default = js_sched::LoadConfig::default().s_cpu_permille;
    assert_eq!(none.cpu_slack().permille(), default);
    assert_eq!(none.cpu_slack(), CpuSlack::default());
    for (given, permille) in [("1.0", 0), ("1.25", 250), ("2.0", 1000)] {
        let read = config(args(&scratch, &["--cpu-slack", given])).unwrap();
        assert_eq!(read.cpu_slack().permille(), permille, "{given}");
    }
    let flagless = config(base_args(&scratch)).unwrap();
    assert_eq!(flagless.cpu_slack(), CpuSlack::default());
}

/// Asserts that a slack below 1.0, above js-sched's ceiling of 2.0, not a number or empty
/// is refused at start, by the flag and by the key, each error naming `cpu_slack`; and that
/// the flag without `--agent-pool-config` is refused naming that one. It catches a parse
/// that accepts any of them, a value clamped instead of refused, and a slack accepted
/// where nothing reads it.
#[test]
fn cpu_slack_values_outside_the_range_are_refused() {
    let scratch = Scratch::new("cpu_slack_refused");
    for bad in ["0.9", "2.5", "abc", ""] {
        let said = parsed(args(&scratch, &["--cpu-slack", bad])).err().unwrap();
        assert!(said.contains("'--cpu-slack <CPU_SLACK>'"), "{bad}: {said}");
        assert!(said.contains(&out_of_range(bad)), "{bad}: {said}");
    }
    let mut bare = base_args(&scratch);
    bare.extend(["--cpu-slack", "1.1"].map(OsString::from));
    let said = parsed(bare).err().unwrap();
    assert_eq!(said, "--cpu-slack: requires --agent-pool-config");

    let listeners = "listen = \"127.0.0.1:0\"\nagent_listen = \"127.0.0.2:0\"";
    let pool = PoolText::joined("burst");
    let text_of = |scratch: &Scratch, extra: &str| {
        let table = server(scratch, &format!("{listeners}\n{extra}"));
        file(&table, &[&pool])
    };
    for (value, given) in [("0.9", "0.9"), ("2.5", "2.5"), ("3", "3"), ("nan", "NaN")] {
        let scratch = Scratch::new("cpu_slack_key_range");
        let text = text_of(&scratch, &format!("cpu_slack = {value}"));
        let expected = format!("server.cpu_slack: {}", out_of_range(given));
        assert_eq!(refusal(&scratch, &text), expected, "{value}");
    }
    for value in ["\"abc\"", "\"\"", "\"1.1\"", "true"] {
        let scratch = Scratch::new("cpu_slack_key_type");
        let text = text_of(&scratch, &format!("cpu_slack = {value}"));
        let expected = "server.cpu_slack: must be a number such as 1.1";
        assert_eq!(refusal(&scratch, &text), expected, "{value}");
    }
}

/// Asserts the file's key: absent it is js-sched's default; `1.1`, `1.0` and `2` are read,
/// a whole number as a multiplier too. It catches a key that is not read.
#[test]
fn the_key_is_read_from_the_file() {
    let listeners = "listen = \"127.0.0.1:0\"\nagent_listen = \"127.0.0.2:0\"";
    let pool = PoolText::joined("burst");
    let default = CpuSlack::default().permille();
    for (name, extra, permille) in [
        ("cpu_slack_key_off", "", default),
        ("cpu_slack_key_1_1", "cpu_slack = 1.1", 100),
        ("cpu_slack_key_1_0", "cpu_slack = 1.0", 0),
        ("cpu_slack_key_2", "cpu_slack = 2", 1000),
    ] {
        let scratch = Scratch::new(name);
        let extra = format!("{listeners}\n{extra}");
        let text = file(&server(&scratch, &extra), &[&pool]);
        let config = load(&scratch, &text).unwrap();
        assert_eq!(config.cpu_slack().permille(), permille, "{name}");
    }
}
