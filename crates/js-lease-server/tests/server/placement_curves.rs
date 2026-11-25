//! `--placement-curves` and `server.placement_curves`: whether the placement engine learns
//! per-class profiles in shadow or books by them, and `server.curve_hosts` and
//! `server.curve_max_wait_seconds`, which have no flags. These tests read the flag and the key and their refusals;
//! what the engine does with the mode is tested in `js-sched-placer`.

use std::ffi::OsString;
use std::time::Duration;

use js_lease_server::{Cli, Config};
use js_sched_placer::curves::{CurveHosts, CurveSettings};
use js_sched_placer::sizes::Curves;
use js_types::HostId;

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
    format!("placement_curves must be off, shadow or on; got {given:?}")
}

/// Asserts the flag's parse: absent it is `off`, and `off`, `shadow` and `on` are read as given.
/// Catches a flag that is not read, a default other than `off` and an `on` that is refused.
#[test]
fn the_flag_is_read_and_defaults_to_off() {
    let scratch = Scratch::new("curves_flag");
    let none = config(args(&scratch, &[])).unwrap();
    assert_eq!(none.placement_curves(), Curves::Off);
    for (given, mode) in [
        ("off", Curves::Off),
        ("shadow", Curves::Shadow),
        ("on", Curves::On),
    ] {
        let read = config(args(&scratch, &["--placement-curves", given])).unwrap();
        assert_eq!(read.placement_curves(), mode, "{given}");
    }
}

/// Asserts that a capitalised value, a padded one and an empty one are refused by the flag with
/// the message naming `off`, `shadow` and `on` and quoting the value given, and that the flag
/// without `--agent-pool-config` is refused naming that one, with `shadow` and with `on` alike.
/// Catches a parse that ignores case or trims, and a flag accepted where nothing reads it.
#[test]
fn the_flag_refuses_any_other_value_and_a_start_with_no_pool() {
    let scratch = Scratch::new("curves_flag_refused");
    for bad in ["On", " on", "Shadow", ""] {
        let said = parsed(args(&scratch, &["--placement-curves", bad]))
            .err()
            .unwrap();
        assert!(
            said.contains("'--placement-curves <PLACEMENT_CURVES>'"),
            "{bad}: {said}"
        );
        assert!(said.contains(&refused_value(bad)), "{bad}: {said}");
    }
    for mode in ["shadow", "on"] {
        let mut bare = crate::support::without_agent_pool(&scratch);
        bare.extend(["--placement-curves", mode].map(OsString::from));
        let said = parsed(bare).err().unwrap();
        assert_eq!(
            said, "--placement-curves: requires --agent-pool-config",
            "{mode}"
        );
    }
}

/// Asserts the key's refusals, each by the whole message: `On`, `Shadow` and an empty value name
/// the three accepted values, and a value that is not a string is told so. Catches a key that
/// ignores case, and a number or boolean read as text.
#[test]
fn the_key_refuses_any_other_value() {
    let listeners = "listen = \"127.0.0.1:0\"\nagent_listen = \"127.0.0.2:0\"";
    let pool = PoolText::joined("burst");
    for (value, given) in [("\"On\"", "On"), ("\"Shadow\"", "Shadow"), ("\"\"", "")] {
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

/// Asserts the file's key: absent it is `off`; `"off"`, `"shadow"` and `"on"` are read as given.
/// Catches a key that is not read and an `on` that is refused.
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
        ("curves_key_on", "placement_curves = \"on\"", Curves::On),
    ] {
        let scratch = Scratch::new(name);
        let extra = format!("{listeners}\n{extra}");
        let text = file(&server(&scratch, &extra), &[&pool]);
        let config = load(&scratch, &text).unwrap();
        assert_eq!(config.placement_curves(), mode, "{name}");
    }
}

/// The listeners of a server in a file, as the other tests of this file write them.
const LISTENERS: &str = "listen = \"127.0.0.1:0\"\nagent_listen = \"127.0.0.2:0\"";

/// The settings the file gives with `extra` after the listeners, or the refusal.
fn curve_settings(name: &str, extra: &str) -> Result<CurveSettings, String> {
    let scratch = Scratch::new(name);
    let pool = PoolText::joined("burst");
    let text = file(
        &server(&scratch, &format!("{LISTENERS}\n{extra}")),
        &[&pool],
    );
    let read = load(&scratch, &text);
    read.map(|config| config.curve_settings())
        .map_err(|err| err.to_string())
}

/// The hosts `names` as the placer reads them.
fn hosts(names: &[&str]) -> CurveHosts {
    CurveHosts::new(
        names
            .iter()
            .map(|name| HostId::new(name).unwrap())
            .collect(),
    )
}

/// Asserts that with neither key the file gives every host and a wait bound of 30 s, and that
/// both are read in every mode (`off`, `shadow` and `on` each keep the hosts and the wait they
/// were given). Catches a default other than every host and 30 s, and a key read only under `on`.
#[test]
fn the_curve_keys_default_to_every_host_and_thirty_seconds_and_are_read_in_every_mode() {
    let default = curve_settings("curves_default", "").unwrap();
    assert_eq!(default, CurveSettings::default());
    assert_eq!(default.max_wait, Duration::from_secs(30));
    assert!(default.hosts.books(&HostId::new("any").unwrap()));
    for mode in ["off", "shadow", "on"] {
        let extra = format!(
            "placement_curves = \"{mode}\"\ncurve_hosts = [\"h-1\", \"h-2\"]\ncurve_max_wait_seconds = 45"
        );
        let given = curve_settings("curves_given", &extra).unwrap();
        assert_eq!(given.hosts, hosts(&["h-1", "h-2"]), "{mode}");
        assert_eq!(given.max_wait, Duration::from_secs(45), "{mode}");
    }
}

/// Asserts that the wait bound is read at both ends of its range, 0 and 300, and refused one
/// past either end (301; a negative number is refused as every key refuses it), with the whole message naming
/// the key and the range, and that a value that is not a whole number is refused. Catches a range that is off
/// by one at either end and a float or string read as a number.
#[test]
fn the_wait_bound_is_from_zero_to_three_hundred_seconds() {
    for (value, secs) in [("0", 0), ("1", 1), ("300", 300)] {
        let given = curve_settings(
            "curves_wait_ok",
            &format!("curve_max_wait_seconds = {value}"),
        );
        assert_eq!(
            given.unwrap().max_wait,
            Duration::from_secs(secs),
            "{value}"
        );
    }
    let said = curve_settings("curves_wait_bad", "curve_max_wait_seconds = 301");
    let expected = "server.curve_max_wait_seconds: must be from 0 to 300";
    assert_eq!(said.unwrap_err(), expected);
    let said = curve_settings("curves_wait_negative", "curve_max_wait_seconds = -1");
    let expected = "server.curve_max_wait_seconds: must not be negative";
    assert_eq!(said.unwrap_err(), expected);
    for value in ["30.5", "\"30\"", "true"] {
        let said = curve_settings(
            "curves_wait_type",
            &format!("curve_max_wait_seconds = {value}"),
        );
        assert!(
            said.unwrap_err()
                .starts_with("server.curve_max_wait_seconds: "),
            "{value}"
        );
    }
}

/// Asserts the host list's refusals, each by the whole message: an empty array, a name twice, an
/// empty name, a non-string item, a value that is not an array, and a name that is not a host
/// id (a capital, 64 characters), the last naming the key and the value. Catches a list that
/// accepts what no host can be called, and a refusal that does not name the key.
#[test]
fn the_host_list_refuses_what_names_no_host() {
    let long = "a".repeat(64);
    let not_id = "must be 1 to 63 characters of [a-z0-9._-]";
    let cases = [
        (
            "[]".to_owned(),
            "server.curve_hosts: must name at least one".to_owned(),
        ),
        (
            "[\"a\", \"a\"]".to_owned(),
            "server.curve_hosts: names \"a\" twice".to_owned(),
        ),
        (
            "[\"\"]".to_owned(),
            "server.curve_hosts: holds an empty name".to_owned(),
        ),
        (
            "[1]".to_owned(),
            "server.curve_hosts: must be an array of strings".to_owned(),
        ),
        (
            "\"a\"".to_owned(),
            "server.curve_hosts: must be an array of strings".to_owned(),
        ),
        (
            "[\"Host\"]".to_owned(),
            format!("server.curve_hosts: host id \"Host\" {not_id}"),
        ),
        (
            format!("[\"{long}\"]"),
            format!("server.curve_hosts: host id \"{long}\" {not_id}"),
        ),
    ];
    for (value, expected) in cases {
        let said = curve_settings("curves_hosts_bad", &format!("curve_hosts = {value}"));
        assert_eq!(said.unwrap_err(), expected, "{value}");
    }
    let longest = "a".repeat(63);
    let list = format!("curve_hosts = [\"{longest}\", \"a.b_c-1\"]");
    let ok = curve_settings("curves_hosts_ok", &list);
    assert_eq!(ok.unwrap().hosts, hosts(&[&longest, "a.b_c-1"]));
}

/// Asserts that the host list names the hosts that book by curve and no others: a named host
/// books and an unnamed one does not, and with no list every host books. Catches a list that
/// is read as the hosts that do not book.
#[test]
fn the_host_list_books_the_hosts_it_names_only() {
    let named = curve_settings("curves_named", "curve_hosts = [\"h-1\"]").unwrap();
    assert!(named.hosts.books(&HostId::new("h-1").unwrap()));
    assert!(!named.hosts.books(&HostId::new("h-2").unwrap()));
}
