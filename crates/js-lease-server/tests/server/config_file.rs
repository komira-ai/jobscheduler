//! The configuration file that is accepted: the design's examples as written there, the
//! defaults of a pool, and the flags when there is no `--config`.

use std::time::Duration;

use js_lease_server::{Cli, Config, Launch, Limits, NetworkPlan, run};
use js_types::ExecMode;

use crate::config_support::{
    PoolText, doc_block, file, load, oauth_file, server, server_with_agents,
};
use crate::support::{Harness, Scratch, T0, base_args, lease_body};

/// Replaces the placeholder paths of a design example with files the test made, keeping
/// everything else as written.
fn with_files(scratch: &Scratch, example: &str) -> String {
    let key = scratch.write("api-key", "key-material", 0o600);
    let hosts = scratch.write("hosts.toml", js_pool_static::EXAMPLE_CONFIG, 0o644);
    let token = scratch.write("client-token", crate::support::TOKEN, 0o600);
    let oauth = oauth_file(scratch);
    example
        .replace("/run/credentials/js/lambda-key", &key.display().to_string())
        .replace("/etc/jobscheduler/hosts.toml", &hosts.display().to_string())
        .replace(
            "/run/credentials/js/client-token",
            &token.display().to_string(),
        )
        .replace("\"/run/credentials/js/ts-oauth\"", &oauth)
}

/// Asserts that the pool of section 4.1, read out of the design document and given a
/// server table and secret files, builds a pool in `plan` that launches nothing, with
/// the design's defaults: `container` for `exec` and fifteen minutes to boot. It catches
/// the example drifting from the code.
#[test]
fn the_pool_of_the_design_builds_in_plan() {
    let scratch = Scratch::new("config_4_1");
    let pool = with_files(&scratch, &doc_block("### 4.1 A user's pool"));
    let text = format!("{}\n{pool}", server_with_agents(&scratch));
    let config = load(&scratch, &text).unwrap();
    let plan = config.elastic_pool().unwrap();
    assert_eq!(plan.name, "gpu");
    assert_eq!(plan.launch, Launch::Plan);
    assert_eq!(plan.max_instances, 4);
    assert_eq!(plan.max_spend_per_hour_cents, 1200);
    assert_eq!(plan.max_spend_per_day_cents, 9600);
    assert_eq!(plan.idle, Some(Duration::from_secs(600)));
    assert_eq!(plan.boot_timeout, Duration::from_secs(900));
    assert_eq!(plan.exec, Some(ExecMode::Container));
    assert_eq!(plan.provider().payload_limit(), 0);
    match &plan.network {
        NetworkPlan::Tailscale { tag, .. } => assert_eq!(tag, "tag:js-agent"),
        other => panic!("{other:?}"),
    }
}

/// Asserts that the full file of section 4.2, read out of the design document, parses:
/// the listeners and the advertised address as written, a static pool and an elastic
/// pool beside it with every optional key (`exec`, `boot_timeout`, `launch`, regions
/// and an ssh key in the provider table), and the warning for no wildcard. It catches
/// the example drifting from the code, and a key the reader drops.
#[test]
fn the_full_file_of_the_design_parses() {
    let scratch = Scratch::new("config_4_2");
    let text = with_files(&scratch, &doc_block("### 4.2 A full service file"));
    let config = load(&scratch, &text).unwrap();
    assert_eq!(config.listen().to_string(), "100.64.0.1:7300");
    assert_eq!(
        config.agent_listen().unwrap().to_string(),
        "100.64.0.1:7301"
    );
    assert_eq!(
        config.advertise_url(),
        Some("https://js.example.ts.net:7301")
    );
    assert!(config.warnings().is_empty(), "{:?}", config.warnings());
    let plan = config.elastic_pool().unwrap();
    assert_eq!(plan.name, "gpu");
    assert_eq!(plan.launch, Launch::Plan);
    assert_eq!(plan.boot_timeout, Duration::from_secs(900));
    assert_eq!(plan.exec, Some(ExecMode::Container));
    assert_eq!(config.tick_period(), Duration::from_secs(5));
}

/// Asserts the defaults of a pool that says nothing about them: `launch` is `plan`,
/// `exec` is `container`, there is no idle time, the network is `default`, and the pool
/// has no join token file. It catches a pool that launches by default, and a default
/// that drifts from the design.
#[test]
fn a_pool_that_says_nothing_launches_nothing_and_requires_the_container_contract() {
    let scratch = Scratch::new("config_defaults");
    let pool = PoolText::elastic(&scratch).without("idle");
    let config = load(&scratch, &file(&server_with_agents(&scratch), &[&pool])).unwrap();
    let plan = config.elastic_pool().unwrap();
    assert_eq!(plan.launch, Launch::Plan);
    assert_eq!(plan.exec, Some(ExecMode::Container));
    assert_eq!(plan.idle, None);
    assert!(matches!(plan.network, NetworkPlan::Default));
    let shown = format!("{plan:?}");
    assert!(shown.contains("launch: Plan"), "{shown}");
    assert!(shown.contains("network: Default"), "{shown}");
    assert_eq!(config.listen().to_string(), "127.0.0.1:0");
}

/// Asserts that `exec = "none"` asks for no host contract, `exec = "sandbox-exec"` asks
/// for that mode of a kind that can launch it, and `launch = "live"` and
/// `network = { kind = "default" }` are accepted and read. It catches an opt-out that
/// still requires a runtime, and `live` read as `plan`.
#[test]
fn exec_can_be_opted_out_and_launch_can_be_live() {
    let scratch = Scratch::new("config_optout");
    let server = server_with_agents(&scratch);
    let dialed = crate::config_support::server(
        &scratch,
        "listen = \"127.0.0.1:0\"\nagent_listen = \"127.0.0.2:0\"\nadvertise_url = \"https://js.example:7301\"",
    );
    let pin = format!(
        "{{ x86_64 = {{ url = \"https://pkgs.example.org/agent\", sha256 = \"{}\" }} }}",
        "5a".repeat(32)
    );
    let pool = PoolText::elastic(&scratch).set("exec", "\"none\"");
    let pool = pool.set("launch", "\"live\"").set("agent", &pin);
    let pool = pool.set("network", "{ kind = \"default\" }");
    let config = load(&scratch, &file(&dialed, &[&pool])).unwrap();
    let plan = config.elastic_pool().unwrap();
    assert_eq!(plan.exec, None);
    assert_eq!(plan.launch, Launch::Live);
    assert!(format!("{plan:?}").contains("launch: Live"));
    let mac = PoolText::elastic(&scratch)
        .set("class", "\"gpu-host\"")
        .set("exec", "\"sandbox-exec\"")
        .set("provider", "{ kind = \"any-mode\" }");
    let config = load(&scratch, &file(&server, &[&mac])).unwrap();
    assert_eq!(
        config.elastic_pool().unwrap().exec,
        Some(ExecMode::SandboxExec)
    );
}

/// Asserts that a file with a static pool alone builds with no elastic pool, that a
/// server table with nothing but its required keys gets the flags' defaults (loopback
/// port 7300, a tick of five seconds), and that the file's tick is read. It catches a
/// default that differs from the flag's.
#[test]
fn a_static_pool_alone_and_the_defaults_of_the_server_table() {
    let scratch = Scratch::new("config_static");
    let pool = PoolText::static_pool(&scratch, "hosts");
    let config = load(&scratch, &file(&server(&scratch, ""), &[&pool])).unwrap();
    assert!(config.elastic_pool().is_none());
    assert_eq!(config.listen().to_string(), "127.0.0.1:7300");
    assert_eq!(config.agent_listen(), None);
    assert_eq!(config.advertise_url(), None);
    assert_eq!(config.tick_period(), Duration::from_secs(5));
    let slow = server(&scratch, "tick = \"2m\"");
    let config = load(&scratch, &file(&slow, &[&pool])).unwrap();
    assert_eq!(config.tick_period(), Duration::from_secs(120));
}

/// Asserts that every key of the server table is read and reaches the server: the
/// limits and timeouts are the ones the file gives, and a server started on the file
/// grants a lease for the default ttl the file gives and cuts a longer one to its maximum.
/// It catches a key that is parsed and dropped.
#[tokio::test]
async fn every_key_of_the_server_table_is_read() {
    let scratch = Scratch::new("config_server_keys");
    let extra = "listen = \"127.0.0.1:0\"\ntick = \"3s\"\ndefault_ttl = \"5m\"\nmax_ttl = \"2h\"\n\
                 max_connections = 7\nhandshake_timeout = \"3s\"\nheader_timeout = \"4s\"\n\
                 read_timeout = \"5s\"\nwrite_timeout = \"6s\"\nmax_requests = 9\n\
                 drain_timeout = \"7s\"\nadvertise_url = \"http://localhost:7300\"";
    let pool = PoolText::static_pool(&scratch, "hosts");
    let config = load(&scratch, &file(&server(&scratch, extra), &[&pool])).unwrap();
    assert_eq!(config.tick_period(), Duration::from_secs(3));
    assert_eq!(config.advertise_url(), Some("http://localhost:7300"));
    let secs = Duration::from_secs;
    let wanted = Limits {
        max_connections: 7,
        handshake_timeout: secs(3),
        header_timeout: secs(4),
        read_timeout: secs(5),
        write_timeout: secs(6),
        max_requests: 9,
        drain_timeout: secs(7),
    };
    assert_eq!(config.limits(), wanted);
    let h = Harness::over(scratch, config).await;
    assert_eq!(h.running.agent_addr(), None);
    let reply = h.call("POST", "/v1/leases", Some(&lease_body())).await;
    assert_eq!(reply.json()["expires_at_ms"], T0 + 300_000);
    let long = serde_json::json!({ "ttl_seconds": 999_999 });
    let mut body = lease_body();
    body["ttl_seconds"] = long["ttl_seconds"].clone();
    let reply = h.call("POST", "/v1/leases", Some(&body)).await;
    assert_eq!(reply.json()["expires_at_ms"], T0 + 7_200_000);
}

/// Asserts that `--config` alone is a command line, that without it the flags that were
/// required still are, and that the program loads the file, refusing a missing one with
/// exit status 2 and the file's path. It catches a `--config` that is ignored, and a
/// flag that stopped being required.
#[test]
fn the_config_flag_replaces_the_required_flags() {
    let scratch = Scratch::new("config_flag");
    let path = scratch.path("js.toml");
    let args = ["js-lease-server", "--config", path.to_str().unwrap()];
    assert!(Cli::try_parse_from(args).is_ok());
    assert!(Cli::try_parse_from(["js-lease-server"]).is_err());
    let pool = PoolText::static_pool(&scratch, "hosts");
    scratch.write("js.toml", &file(&server(&scratch, ""), &[&pool]), 0o644);
    let config = Config::from_cli(Cli::try_parse_from(args).unwrap()).unwrap();
    assert_eq!(config.listen().to_string(), "127.0.0.1:7300");
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let missing = scratch.path("absent.toml");
    let args = ["js-lease-server", "--config", missing.to_str().unwrap()];
    let code = run(args.map(std::ffi::OsString::from), &mut out, &mut err);
    assert!(code == std::process::ExitCode::from(2));
    let said = String::from_utf8(err).unwrap();
    assert!(
        said.starts_with(&format!(
            "js-lease-server: --config {}: cannot read: ",
            missing.display()
        )),
        "{said}"
    );
}

/// Asserts that a command line without `--config` is read as before, and that the three
/// flags `--config` made optional are still refused by name when a `Cli` built by hand
/// lacks them. It catches a flag path that changed with the file.
#[test]
fn flags_still_work_without_the_config_flag() {
    let scratch = Scratch::new("config_flags_only");
    let args = base_args(&scratch);
    assert!(Config::from_cli(Cli::try_parse_from(args.clone()).unwrap()).is_ok());
    for (flag, strip) in [("--token-file", 0usize), ("--state-file", 1), ("--boot", 2)] {
        let mut cli = Cli::try_parse_from(args.clone()).unwrap();
        match strip {
            0 => cli.token_file = None,
            1 => cli.state_file = None,
            _ => cli.boot = None,
        }
        let refused = Config::from_cli(cli).err().unwrap();
        assert_eq!(
            refused.to_string(),
            format!("{flag}: required unless --config is given")
        );
    }
}
