//! `--score-balance`, `--score-slack-tolerance-ppm` and `--key-share-percent`, and their keys
//! of `[server]`: the placement engine's scoring settings. These tests read the flags and the
//! keys and their refusals, and run the program with the flags; that the values reach the
//! settings the engine is built with is tested in process, in `src/config/placement.rs`, and
//! what each does to a placement in js-sched.

use std::ffi::OsString;
use std::process::{ExitCode, Stdio};

use js_lease_server::{Cli, Config, Tunings};
use rustix::process::{Pid, Signal, kill_process};

use crate::config_support::{PoolText, file, load, refusal, server};
use crate::support::{Scratch, TOKEN, base_args, config, request};

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
    scratch.write("agent-pool.toml", POOL, 0o644);
    args.extend(extra.iter().map(OsString::from));
    args
}

fn parsed(args: Vec<OsString>) -> Result<Config, String> {
    let cli = Cli::try_parse_from(args).map_err(|err| err.to_string())?;
    Config::from_cli(cli).map_err(|err| err.to_string())
}

/// The `[server]` table of a server with agents and `extra`, beside one joined pool.
fn text_of(scratch: &Scratch, extra: &str) -> String {
    let listeners = "listen = \"127.0.0.1:0\"\nagent_listen = \"127.0.0.2:0\"";
    let table = server(scratch, &format!("{listeners}\n{extra}"));
    file(&table, &[&PoolText::joined("burst")])
}

/// Behaviour: without the flags the server keeps every tuning at `None`, js-sched's default;
/// each flag given is carried as given, at both ends of its range, the others left out.
/// Catches a flag read and then dropped before the configuration the engine is built from,
/// and one carried into another's place.
#[test]
fn the_flags_are_carried_as_given_and_absent_they_are_none() {
    let scratch = Scratch::new("tunings_flags");
    let none = config(args(&scratch, &[])).unwrap();
    assert_eq!(none.tunings(), Tunings::default());
    let flagless = config(base_args(&scratch)).unwrap();
    assert_eq!(flagless.tunings(), Tunings::default());
    let cases = [
        (
            "--score-balance=0",
            Tunings {
                score_balance: Some(0),
                ..Tunings::default()
            },
        ),
        (
            "--score-balance=4",
            Tunings {
                score_balance: Some(4),
                ..Tunings::default()
            },
        ),
        (
            "--score-slack-tolerance-ppm=1000000",
            Tunings {
                score_slack_tolerance_ppm: Some(1_000_000),
                ..Tunings::default()
            },
        ),
        (
            "--key-share-percent=40",
            Tunings {
                key_share_percent: Some(40),
                ..Tunings::default()
            },
        ),
    ];
    for (flag, want) in cases {
        let read = config(args(&scratch, &[flag])).unwrap();
        assert_eq!(read.tunings(), want, "{flag}");
    }
}

/// Behaviour: a value outside its flag's range is refused at start naming the flag, and each
/// of the three flags without `--agent-pool-config` is refused naming that one. Catches a
/// value clamped and run instead of refused, and a tuning accepted where no engine reads it.
#[test]
fn values_outside_the_ranges_and_flags_without_a_pool_are_refused() {
    let scratch = Scratch::new("tunings_refused");
    for (flag, value) in [
        ("--score-balance", "5"),
        ("--score-slack-tolerance-ppm", "1000001"),
        ("--key-share-percent", "101"),
    ] {
        let said = parsed(args(&scratch, &[flag, value])).err().unwrap();
        assert!(
            said.contains(&format!("invalid value '{value}' for '{flag} <")),
            "{said}"
        );
    }
    for (flag, value) in [
        ("--score-balance", "1"),
        ("--score-slack-tolerance-ppm", "10000"),
        ("--key-share-percent", "25"),
    ] {
        let mut bare = crate::support::without_agent_pool(&scratch);
        bare.extend([flag, value].map(OsString::from));
        let said = parsed(bare).err().unwrap();
        assert_eq!(said, format!("{flag}: requires --agent-pool-config"));
    }
}

/// Behaviour: each key of `[server]` is read as its flag is, at both ends of its range, and
/// absent it is `None`. Catches a key that is not read and one read into another's place.
#[test]
fn the_keys_are_read_from_the_file() {
    let scratch = Scratch::new("tunings_keys_none");
    let none = load(&scratch, &text_of(&scratch, "")).unwrap();
    assert_eq!(none.tunings(), Tunings::default());
    let scratch = Scratch::new("tunings_keys_low");
    let low = "score_balance = 0\nscore_slack_tolerance_ppm = 0\nkey_share_percent = 0";
    let read = load(&scratch, &text_of(&scratch, low)).unwrap();
    let want = Tunings {
        score_balance: Some(0),
        score_slack_tolerance_ppm: Some(0),
        key_share_percent: Some(0),
    };
    assert_eq!(read.tunings(), want);
    let scratch = Scratch::new("tunings_keys_high");
    let high = "score_balance = 4\nscore_slack_tolerance_ppm = 1000000\n\
                key_share_percent = 100";
    let read = load(&scratch, &text_of(&scratch, high)).unwrap();
    let want = Tunings {
        score_balance: Some(4),
        score_slack_tolerance_ppm: Some(1_000_000),
        key_share_percent: Some(100),
    };
    assert_eq!(read.tunings(), want);
}

/// Behaviour: a key outside its range, negative, or not an integer is refused at start, the
/// whole message naming the key and the range. Catches a key clamped instead of refused, a
/// range that differs from the flag's, and a string or a fraction taken as a number.
#[test]
fn keys_outside_the_ranges_or_not_integers_are_refused() {
    let cases = [
        (
            "score_balance = 5",
            "server.score_balance: must be from 0 to 4",
        ),
        (
            "score_slack_tolerance_ppm = 1000001",
            "server.score_slack_tolerance_ppm: must be from 0 to 1000000",
        ),
        (
            "key_share_percent = 101",
            "server.key_share_percent: must be from 0 to 100",
        ),
        (
            "score_balance = -1",
            "server.score_balance: must not be negative",
        ),
        (
            "key_share_percent = \"25\"",
            "server.key_share_percent: must be an integer",
        ),
        (
            "score_slack_tolerance_ppm = 0.01",
            "server.score_slack_tolerance_ppm: must be an integer",
        ),
    ];
    for (line, expected) in cases {
        let scratch = Scratch::new("tunings_key_refused");
        assert_eq!(
            refusal(&scratch, &text_of(&scratch, line)),
            expected,
            "{line}"
        );
    }
}

/// Behaviour: the covered-lease CPU floor has no setting: `--cpu-floor-permille` is not a flag
/// and `cpu_floor_permille` is not a key of `[server]`, so the engine always charges a covered
/// job all of its predicted cores. Catches a floor setting added, which would let the engine
/// charge a job under its prediction and admit past the CPU slack.
#[test]
fn the_cpu_floor_has_no_flag_and_no_key() {
    let scratch = Scratch::new("tunings_no_floor");
    let said = parsed(args(&scratch, &["--cpu-floor-permille", "500"]))
        .err()
        .unwrap();
    assert!(
        said.contains("unexpected argument '--cpu-floor-permille' found"),
        "{said}"
    );
    let refused = refusal(&scratch, &text_of(&scratch, "cpu_floor_permille = 500"));
    assert!(
        refused.starts_with("server.cpu_floor_permille: unknown field; known: "),
        "{refused}"
    );
}

/// `args` run through the program's entry point, as `main` runs it: its exit status and what
/// it printed on standard output and on standard error.
fn run(args: Vec<OsString>) -> (ExitCode, String, String) {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = js_lease_server::run(args, &mut out, &mut err);
    let text = |bytes: Vec<u8>| String::from_utf8(bytes).unwrap();
    (code, text(out), text(err))
}

/// Behaviour: the program's entry point refuses a start with exit status 2, printing nothing
/// on standard output and the refusal on standard error, for a value outside its flag's range
/// (clap's usage error, naming the flag, the value and the range) and for each flag without
/// `--agent-pool-config` (naming the flag). Catches a refusal that the entry point turns into
/// another status, or raises only after the server has started.
#[test]
fn the_program_refuses_a_value_out_of_range_and_a_flag_without_a_pool_with_status_2() {
    let scratch = Scratch::new("tunings_program_refused");
    for (flag, value, range) in [
        ("--score-balance", "5", "0..=4"),
        ("--score-slack-tolerance-ppm", "1000001", "0..=1000000"),
        ("--key-share-percent", "101", "0..=100"),
    ] {
        let (code, out, err) = run(args(&scratch, &[flag, value]));
        assert!(code == ExitCode::from(2), "{flag}={value}: {err}");
        assert_eq!(out, "", "{flag}={value}");
        let name = flag
            .trim_start_matches("--")
            .replace('-', "_")
            .to_uppercase();
        let head = format!(
            "error: invalid value '{value}' for '{flag} <{name}>': {value} is not in {range}"
        );
        assert!(err.starts_with(&head), "{flag}={value}: {err}");
    }
    for (flag, value) in [
        ("--score-balance", "2"),
        ("--score-slack-tolerance-ppm", "5000"),
        ("--key-share-percent", "40"),
    ] {
        let mut bare = crate::support::without_agent_pool(&scratch);
        bare.extend([flag, value].map(OsString::from));
        let (code, out, err) = run(bare);
        assert!(code == ExitCode::from(2), "{flag}: {err}");
        assert_eq!(out, "", "{flag}");
        assert_eq!(
            err,
            format!("js-lease-server: {flag}: requires --agent-pool-config\n")
        );
    }
}

/// Behaviour: the real program, started with all three flags at values inside their ranges
/// beside an agent pool, starts, prints the address it listens on, answers `/healthz` and
/// `/v1/hosts`, and on SIGTERM stops with status 0. Catches a flag that the program refuses or
/// that stops the start after the configuration is read (building the placement engine with
/// the values included).
#[tokio::test]
async fn the_program_starts_and_serves_with_the_three_flags() {
    let scratch = Scratch::new("tunings_program_serves");
    let given = [
        "--score-balance",
        "2",
        "--score-slack-tolerance-ppm",
        "5000",
        "--key-share-percent",
        "40",
    ];
    let (mut child, addr) = crate::binary::launch(&args(&scratch, &given), Stdio::null());
    let health = request(addr, "GET", "/healthz", None, None).await;
    assert_eq!(health.status, 200);
    let hosts = request(addr, "GET", "/v1/hosts", Some(TOKEN), None).await;
    assert_eq!(hosts.status, 200, "{}", hosts.body);
    kill_process(Pid::from_child(&child.0), Signal::TERM).unwrap();
    let status = child.0.wait().unwrap();
    assert!(status.success(), "{status:?}");
}
