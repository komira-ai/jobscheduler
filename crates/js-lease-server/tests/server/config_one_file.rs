//! The configuration file as the only configuration (design PR 10): the server flags that
//! `[server]` now carries, and the flags refused beside `--config`.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::net::SocketAddr;
use std::time::Duration;

use clap::CommandFactory;
use js_lease_server::{Cli, Config};
use serde_json::{Value, json};

use crate::agents_support::{JOIN, hello_body};
use crate::config_support::{PoolText, file, quote, quoted, refusal, server};
use crate::support::{Harness, Reply, Scratch, request};

const MINT: &str = "/v1/agent/join-tokens";

/// An elastic pool that admits agents with [`JOIN`] and asks nothing of a host's contract.
fn pool(scratch: &Scratch) -> PoolText {
    let joins = scratch.write("joins", &format!("{JOIN}\n"), 0o600);
    PoolText::elastic(scratch)
        .set("name", &quote("burst"))
        .set("class", &quote("gpu-host"))
        .set("exec", &quote("none"))
        .set("join_token_file", &quoted(&joins))
}

/// A server on a file whose `[server]` table has `extra` besides two listeners and whose
/// only pool is [`pool`].
async fn serving(name: &str, extra: &str) -> Harness {
    let scratch = Scratch::new(name);
    let listeners = "listen = \"127.0.0.1:0\"\nagent_listen = \"127.0.0.2:0\"";
    let text = file(
        &server(&scratch, &format!("{listeners}\n{extra}")),
        &[&pool(&scratch)],
    );
    let config = crate::config_support::load(&scratch, &text).unwrap();
    Harness::over(scratch, config).await
}

/// A hello of `host` with `join` as its bearer, on the agent port.
async fn say(h: &Harness, join: &str, host: &str, instance: Option<&str>, asks: bool) -> Reply {
    let mut body = hello_body(host);
    body["shape"]["gpu_class"] = json!("nvidia-a10");
    if let Some(instance) = instance {
        body["instance"] = json!({ "provider": "lambdalabs", "instance_id": instance });
    }
    if asks {
        body["accepts_host_credential"] = json!(true);
    }
    let agent = h.running.agent_addr().unwrap();
    let bytes = body.to_string().into_bytes();
    request(agent, "POST", "/v1/agent/hello", Some(join), Some(&bytes)).await
}

/// The statuses of `count` hellos of one host with the pool's token, one after the other.
async fn burst(h: &Harness, count: usize) -> Vec<u16> {
    let mut statuses = Vec::new();
    for _ in 0..count {
        statuses.push(say(h, JOIN, "h1", None, false).await.status);
    }
    statuses
}

/// Asserts the defaults of the hello limit when the file names neither key: the same
/// burst of 60 and one more hello a second as the flags' defaults. Sixty hellos of one
/// token are held to nothing; the sixty-first is 429; a second later one more passes and
/// the next is held again. It catches a limit that was left out of the file's path, or
/// one whose defaults differ from the flags'.
#[tokio::test]
async fn the_hello_limit_defaults_to_the_flags_defaults() {
    let h = serving("one_file_hello_default", "").await;
    let first = burst(&h, 60).await;
    assert!(first.iter().all(|status| *status != 429), "{first:?}");
    assert_eq!(burst(&h, 1).await, [429]);
    h.clock.advance(Duration::from_secs(1));
    assert_ne!(burst(&h, 1).await, [429]);
    assert_eq!(burst(&h, 1).await, [429]);
}

/// Asserts that `agent_hello_burst` and `agent_hello_interval` are read and reach the
/// limiter: with a burst of 2 and an interval of 10 seconds the third hello is 429 and
/// says to retry in 10 seconds, and ten seconds later one hello passes. It catches a key
/// that is parsed and dropped.
#[tokio::test]
async fn the_hello_limit_keys_are_read() {
    let extra = "agent_hello_burst = 2\nagent_hello_interval = \"10s\"";
    let h = serving("one_file_hello_keys", extra).await;
    let first = burst(&h, 2).await;
    assert!(first.iter().all(|status| *status != 429), "{first:?}");
    let held = say(&h, JOIN, "h1", None, false).await;
    assert_eq!(held.status, 429, "{}", held.body);
    assert!(held.head.contains("retry-after: 10"), "{}", held.head);
    h.clock.advance(Duration::from_secs(10));
    assert_ne!(burst(&h, 1).await, [429]);
    assert_eq!(burst(&h, 1).await, [429]);
}

/// A one-time join token minted for `instance` on the client port.
async fn minted(h: &Harness, instance: &str) -> String {
    let body = json!({ "provider": "lambdalabs", "instance_id": instance });
    let reply = h.call("POST", MINT, Some(&body)).await;
    assert_eq!(reply.status, 201, "{}", reply.body);
    reply.json()["token"].as_str().unwrap().to_owned()
}

/// The error code of a refusal.
fn code(reply: &Reply) -> Value {
    reply.json()["error"].clone()
}

/// Asserts `require_host_credential`: without the key the first hello of a one-time token
/// that does not say the agent keeps a credential is welcomed, as the flag's default
/// does; with `require_host_credential = true` it is refused 403
/// `join-token-credential-required`, and a hello that asks gets its credential. It
/// catches a key that is dropped, and one that refuses when it is off.
#[tokio::test]
async fn require_host_credential_is_read_from_the_file() {
    let off = serving("one_file_cred_off", "").await;
    let token = minted(&off, "i-1").await;
    let welcomed = say(&off, &token, "h1", Some("i-1"), false).await;
    assert_eq!(welcomed.status, 200, "{}", welcomed.body);

    let on = serving("one_file_cred_on", "require_host_credential = true").await;
    let token = minted(&on, "i-1").await;
    let refused = say(&on, &token, "h1", Some("i-1"), false).await;
    assert_eq!(refused.status, 403, "{}", refused.body);
    assert_eq!(code(&refused), "join-token-credential-required");
    let asked = say(&on, &token, "h1", Some("i-1"), true).await;
    assert_eq!(asked.status, 200, "{}", asked.body);
    assert!(asked.json()["host_credential"].is_string());
}

/// Asserts that the registration keys are refused naming `server.<key>` when no pool
/// registers agents, as the flags are refused without `--agent-pool-config` (a static
/// pool serves no agent), also when the value is the default, and that a bad value of
/// each is refused naming the key. It catches a key that does nothing without saying so,
/// and a value that is not checked.
#[test]
fn the_registration_keys_need_an_elastic_pool_and_a_valid_value() {
    let needs = "needs an elastic pool, whose hosts register agents";
    for (key, value) in [
        ("agent_hello_burst", "60"),
        ("agent_hello_interval", "\"1s\""),
        ("require_host_credential", "false"),
        ("placement", "\"registry\""),
    ] {
        let scratch = Scratch::new(&format!("one_file_needs_{key}"));
        let stat = PoolText::static_pool(&scratch, "hosts");
        let text = file(&server(&scratch, &format!("{key} = {value}")), &[&stat]);
        assert_eq!(refusal(&scratch, &text), format!("server.{key}: {needs}"));
    }
    for (line, said) in [
        (
            "agent_hello_burst = 0",
            "server.agent_hello_burst: must be from 1 to 4294967295",
        ),
        (
            "agent_hello_burst = \"5\"",
            "server.agent_hello_burst: must be an integer",
        ),
        (
            "agent_hello_interval = \"0s\"",
            "server.agent_hello_interval: must be a positive duration such as 600s, 10m or 1h",
        ),
        (
            "require_host_credential = \"yes\"",
            "server.require_host_credential: must be true or false",
        ),
        (
            "placement = \"elsewhere\"",
            "server.placement: must be registry or js-sched",
        ),
        ("placement = 3", "server.placement: must be a string"),
    ] {
        let scratch = Scratch::new("one_file_bad_value");
        let text = file(&server(&scratch, line), &[&pool(&scratch)]);
        assert_eq!(refusal(&scratch, &text), said);
    }
}

/// The flags beside `--config`, with the refusal each gets. Every flag of the command
/// line but `--config` is here (the next test checks that).
fn beside() -> Vec<(Vec<&'static str>, String)> {
    let key = |flag: &'static str, value: &'static str| {
        let at = flag.trim_start_matches("--").replace('-', "_");
        let said = format!("{flag}: not allowed with --config; set server.{at}");
        (vec![flag, value], said)
    };
    let mut rows = vec![
        key("--listen", "127.0.0.1:0"),
        key("--state-file", "/state"),
        key("--boot", "resume"),
        key("--agent-hello-burst", "5"),
        key("--agent-hello-interval", "5s"),
        key("--placement", "js-sched"),
        key("--default-ttl", "5m"),
        key("--max-ttl", "2h"),
        key("--tick", "3s"),
        key("--tls-cert", "/cert"),
        key("--tls-key", "/key"),
        key("--max-connections", "7"),
        key("--handshake-timeout", "3s"),
        key("--header-timeout", "4s"),
        key("--read-timeout", "5s"),
        key("--write-timeout", "6s"),
        key("--max-requests", "9"),
        key("--drain-timeout", "7s"),
        key("--usage-db", "/usage.db"),
        key("--profile-store-bytes", "5"),
    ];
    rows.push((
        vec!["--usage-off"],
        "--usage-off: not allowed with --config; set server.usage_off".to_owned(),
    ));
    rows.push((
        vec!["--require-host-credential"],
        "--require-host-credential: not allowed with --config; set server.require_host_credential"
            .to_owned(),
    ));
    let moved = [
        ("--token-file", "set server.client_token_file"),
        ("--pool-config", "add a [[pool]] with provider kind static"),
        (
            "--agent-pool-config",
            "add a [[pool]] with an elastic provider",
        ),
        ("--agent-token-file", "set join_token_file of the [[pool]]"),
    ];
    for (flag, instead) in moved {
        let said = format!("{flag}: not allowed with --config; {instead}");
        rows.push((vec![flag, "/file"], said));
    }
    rows
}

/// A valid file at `js.toml` of `scratch` and the command line that names it.
fn with_file(scratch: &Scratch) -> Vec<OsString> {
    let stat = PoolText::static_pool(scratch, "hosts");
    let path = scratch.write("js.toml", &file(&server(scratch, ""), &[&stat]), 0o644);
    vec![
        "js-lease-server".into(),
        "--config".into(),
        path.into_os_string(),
    ]
}

/// The refusal of `args`, which must parse as a command line.
fn refused(args: Vec<OsString>) -> String {
    let cli = Cli::try_parse_from(args).unwrap();
    Config::from_cli(cli).err().expect("refused").to_string()
}

/// Asserts that each flag of the command line, given beside `--config`, is refused with
/// its own name and the key of the file that takes its place, before the file is read (the
/// file here is valid, so the flag is the only fault). The flag may come before or after
/// `--config`, and one that is given with its default value is refused too. It catches a
/// flag that is read and ignored beside the file, and a refusal that names no flag.
#[test]
fn every_flag_beside_config_is_refused_by_name() {
    let scratch = Scratch::new("one_file_beside");
    let base = with_file(&scratch);
    assert!(Config::from_cli(Cli::try_parse_from(base.clone()).unwrap()).is_ok());
    for (flag, said) in beside() {
        let mut after = base.clone();
        after.extend(flag.iter().map(OsString::from));
        assert_eq!(refused(after), said);
        let mut before = vec![base[0].clone()];
        before.extend(flag.iter().map(OsString::from));
        before.extend(base[1..].iter().cloned());
        assert_eq!(refused(before), said);
    }
    let mut default = base.clone();
    default.extend(["--listen", "127.0.0.1:7300"].map(OsString::from));
    assert_eq!(
        refused(default),
        "--listen: not allowed with --config; set server.listen"
    );
}

/// Asserts that the table of the test above is every flag there is: the long flags of the
/// command line less `--config` are exactly the flags it refuses. It catches a flag added
/// later that the refusal test does not cover.
#[test]
fn the_refused_flags_are_every_flag() {
    let asked: BTreeSet<String> = Cli::command()
        .get_arguments()
        .filter_map(|arg| arg.get_long())
        .filter(|long| *long != "config")
        .map(|long| format!("--{long}"))
        .collect();
    let tried: BTreeSet<String> = beside()
        .into_iter()
        .map(|(flag, _)| flag[0].to_owned())
        .collect();
    assert_eq!(tried, asked);
}

/// Asserts that the first conflicting flag on the command line is the one named: with
/// two flags given, whichever was written first, wherever `--config` is.
#[test]
fn the_first_flag_given_is_the_one_named() {
    let scratch = Scratch::new("one_file_first");
    let base = with_file(&scratch);
    let mut args = base.clone();
    args.extend(["--max-ttl", "2h", "--listen", "127.0.0.1:0"].map(OsString::from));
    assert!(refused(args).starts_with("--max-ttl: not allowed with --config;"));
    let mut args = base.clone();
    args.extend(["--listen", "127.0.0.1:0", "--max-ttl", "2h"].map(OsString::from));
    assert!(refused(args).starts_with("--listen: not allowed with --config;"));
    let mut args = vec![base[0].clone(), "--tick".into(), "3s".into()];
    args.extend(base[1..].iter().cloned());
    args.extend(["--listen", "127.0.0.1:0"].map(OsString::from));
    assert!(refused(args).starts_with("--tick: not allowed with --config;"));
}

/// Asserts that the program, run with a flag beside `--config`, ends with status 2 and
/// prints the refusal; and that without `--config` the same flags start as before (the
/// listen address of the flag is the one served). It catches a refusal that is raised
/// after the server started, and a flags-only start that the file path changed.
#[tokio::test]
async fn the_program_refuses_a_flag_beside_config_and_flags_alone_still_start() {
    let scratch = Scratch::new("one_file_program");
    let mut args = with_file(&scratch);
    args.extend(["--listen", "127.0.0.1:0"].map(OsString::from));
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = js_lease_server::run(args, &mut out, &mut err);
    assert!(code == std::process::ExitCode::from(2));
    assert!(out.is_empty());
    assert_eq!(
        String::from_utf8(err).unwrap(),
        "js-lease-server: --listen: not allowed with --config; set server.listen\n"
    );

    let h = Harness::new("one_file_flags_only").await;
    let served: SocketAddr = h.addr();
    assert!(served.ip().is_loopback());
    let reply = h.call("GET", "/v1/leases", None).await;
    assert_eq!(reply.status, 200);
}

/// Asserts that a `Cli` made any way but `Cli::try_parse_from` (here clap's own
/// `Parser::try_parse_from`, which is what `parse` and `parse_from` call) is refused by
/// `Config::from_cli`, with and without `--config`, rather than let a flag beside `--config`
/// pass unseen. It catches a refusal that fails open.
#[test]
fn a_cli_parsed_another_way_is_refused() {
    let scratch = Scratch::new("one_file_other_parse");
    let mut args = with_file(&scratch);
    args.extend(["--listen", "127.0.0.1:0"].map(OsString::from));
    for args in [args, crate::support::base_args(&scratch)] {
        let cli = <Cli as clap::Parser>::try_parse_from(args).unwrap();
        let said = Config::from_cli(cli).err().unwrap().to_string();
        assert!(
            said.starts_with("--config: the command line was parsed without recording its flags"),
            "{said}"
        );
    }
}
