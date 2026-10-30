//! What start-up refuses in the file as a whole and in its `[server]` table (design 4.4):
//! each refusal with its exact message, naming where it is.

use std::os::unix::fs::PermissionsExt;

use js_lease_server::{Config, Kinds, OwnerOnlyFiles};

use crate::config_support::{PoolText, clock, file, load, oauth_file, quoted, refusal, server};
use crate::support::Scratch;

/// A static pool, to stand beside the table under test.
fn hosts(scratch: &Scratch) -> PoolText {
    PoolText::static_pool(scratch, "hosts")
}

/// The refusal of a file whose `[server]` table has `extra` after its required keys,
/// beside a static pool.
fn refused_server(name: &str, extra: &str) -> String {
    let scratch = Scratch::new(name);
    let text = file(&server(&scratch, extra), &[&hosts(&scratch)]);
    refusal(&scratch, &text)
}

/// Asserts that a file that is not there is refused naming `--config` and the path with
/// the system's reason, and that a file that is not TOML is refused with the line only:
/// the value at fault, which here is a planted secret, is nowhere in the message.
#[test]
fn a_missing_file_and_a_syntax_error() {
    let scratch = Scratch::new("config_missing");
    let path = scratch.path("absent.toml");
    let refused = Config::from_config_file(&path, &Kinds::standard(), &OwnerOnlyFiles, clock());
    assert_eq!(
        refused.err().unwrap().to_string(),
        format!(
            "--config {}: cannot read: No such file or directory (os error 2)",
            path.display()
        )
    );
    let broken = "[server]\nstate_file = \"PLANTED-SYNTAX-VALUE\" oops\n";
    let said = refusal(&scratch, broken);
    let path = scratch.path("js.toml");
    assert_eq!(
        said,
        format!("--config {}: not valid TOML (line 2)", path.display())
    );
}

/// Asserts that an unknown table or field is refused wherever it is, naming it and the
/// fields that exist: at the top of the file, in `[server]` (a status token file, which
/// the design does not have, among them) and in a pool. It catches a typo read as a
/// default.
#[test]
fn an_unknown_table_or_field_is_refused_with_the_known_ones() {
    let scratch = Scratch::new("config_unknown");
    let pool = hosts(&scratch);
    let text = format!(
        "{}\n[extra]\nx = 1\n",
        file(&server(&scratch, ""), &[&pool])
    );
    assert_eq!(
        refusal(&scratch, &text),
        "extra: unknown field; known: server, pool"
    );
    assert_eq!(
        refused_server("config_unknown_server", "status_token_file = \"/x\""),
        "server.status_token_file: unknown field; known: listen, agent_listen, advertise_url, \
         scheduler_id, state_file, boot, client_token_file, tls_cert, tls_key, tick, default_ttl, max_ttl, \
         max_connections, handshake_timeout, header_timeout, read_timeout, write_timeout, \
         max_requests, drain_timeout, agent_hello_burst, agent_hello_interval, \
         require_host_credential, usage_db, usage_off, profile_store_bytes, \
         alert_webhook_url, alert_webhook_token_file, alert_repeat_after"
    );
    let text = file(&server(&scratch, ""), &[&pool.set("colour", "\"red\"")]);
    assert_eq!(
        refusal(&scratch, &text),
        "pool \"hosts\": colour: unknown field; known: name, class, exec, \
         memory_schedulable_percent, max_instances, max_spend_per_hour, max_spend_per_day, \
         idle, boot_timeout, launch, agent, join_token_file, provider, network"
    );
}

/// Asserts the `[server]` table's required keys, its types and its values: each missing
/// key is `required`, a key of the wrong type says what it must be without printing what
/// it was, and a value out of range says the range.
#[test]
fn the_server_table_is_checked_key_by_key() {
    let scratch = Scratch::new("config_server_table");
    let pool = hosts(&scratch);
    let no_server = refusal(&scratch, &pool.text());
    assert_eq!(no_server, "server: required: add a [server] table");
    assert_eq!(
        refusal(&scratch, &format!("server = 5\n{}", pool.text())),
        "server: must be a table"
    );
    let token = scratch.write("client-token", crate::support::TOKEN, 0o600);
    let all = [
        (
            "state_file",
            format!("{:?}", scratch.state().display().to_string()),
        ),
        ("boot", "\"initialise\"".to_owned()),
        ("client_token_file", quoted(&token)),
    ];
    for (missing, _) in &all {
        let kept: String = all
            .iter()
            .filter(|(key, _)| key != missing)
            .map(|(key, value)| format!("{key} = {value}\n"))
            .collect();
        let text = format!("[server]\n{kept}\n{}", pool.text());
        assert_eq!(
            refusal(&scratch, &text),
            format!("server.{missing}: required")
        );
    }
    let cases = [
        (
            "tick = \"soon\"",
            "server.tick: must be a positive duration such as 600s, 10m or 1h",
        ),
        ("tick = 5", "server.tick: must be a string"),
        (
            "tick = \"0s\"",
            "server.tick: must be a positive duration such as 600s, 10m or 1h",
        ),
        (
            "max_connections = 0",
            "server.max_connections: must be from 1 to 65535",
        ),
        (
            "max_connections = 70000",
            "server.max_connections: must be from 1 to 65535",
        ),
        (
            "max_requests = 0",
            "server.max_requests: must be from 1 to 4294967295",
        ),
        (
            "max_requests = \"many\"",
            "server.max_requests: must be an integer",
        ),
        (
            "max_requests = -1",
            "server.max_requests: must not be negative",
        ),
        (
            "listen = \"here\"",
            "server.listen: must be an address such as 127.0.0.1:7300",
        ),
        (
            "agent_listen = 7301",
            "server.agent_listen: must be a string",
        ),
    ];
    for (extra, expected) in cases {
        assert_eq!(
            refused_server("config_server_cases", extra),
            expected,
            "{extra}"
        );
    }
    let later = server(&scratch, "").replace("\"initialise\"", "\"later\"");
    assert_eq!(
        refusal(&scratch, &file(&later, &[&pool])),
        "server.boot: must be initialise or resume"
    );
    let said = refused_server("config_ttl", "default_ttl = \"2h\"\nmax_ttl = \"1h\"");
    assert!(said.starts_with("server.default_ttl: "), "{said}");
}

/// Asserts the client token file: it is read owner-only like `--token-file`, so a
/// world-readable one is refused naming `server.client_token_file`, and a token that is
/// too short is refused without printing it.
#[test]
fn the_client_token_file_is_a_secret_file() {
    let scratch = Scratch::new("config_client_token");
    let text = file(&server(&scratch, ""), &[&hosts(&scratch)]);
    let path = scratch.path("client-token");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let said = refusal(&scratch, &text);
    let prefix = format!(
        "server.client_token_file: {} must be a regular file",
        path.display()
    );
    assert!(said.starts_with(&prefix), "{said}");
    assert!(said.contains("not mode 0644"), "{said}");
    scratch.write("client-token", "PLANTED-SHORT", 0o600);
    assert_eq!(
        refusal(&scratch, &text),
        format!(
            "server.client_token_file: {} holds a token of 13 bytes; a token must have at least 32 (use a random one)",
            path.display()
        )
    );
}

/// Asserts that no pool at all is refused, and that `pool` as a value that is not an
/// array of tables is refused with the way to write it.
#[test]
fn a_file_needs_pools_written_as_tables() {
    let scratch = Scratch::new("config_pools");
    let bare = server(&scratch, "");
    assert_eq!(
        refusal(&scratch, &bare),
        "pool: required: add at least one [[pool]] table"
    );
    for pools in ["pool = 7", "pool = [1]"] {
        assert_eq!(
            refusal(&scratch, &format!("{pools}\n{bare}")),
            "pool: must be an array of tables, written [[pool]]"
        );
    }
}

/// Asserts the pair of TLS files: one without the other is refused naming the missing
/// one, and a listen address off the loopback without them is refused naming the key
/// (`listen` and `agent_listen` alike), with the reason the flags give. It catches a
/// bearer token that would cross a network in clear.
#[test]
fn tls_files_come_together_and_a_public_listener_needs_them() {
    let scratch = Scratch::new("config_tls");
    assert_eq!(
        refused_server("config_tls_a", "tls_cert = \"/c\""),
        "server.tls_key: required with server.tls_cert"
    );
    assert_eq!(
        refused_server("config_tls_b", "tls_key = \"/k\""),
        "server.tls_cert: required with server.tls_key"
    );
    let clear = "is not a loopback address, so server.tls_cert and server.tls_key are required: a bearer token never crosses a network in clear";
    assert_eq!(
        refused_server("config_tls_c", "listen = \"0.0.0.0:7300\""),
        format!("server.listen: 0.0.0.0:7300 {clear}")
    );
    assert_eq!(
        refused_server("config_tls_d", "agent_listen = \"10.1.2.3:7301\""),
        format!("server.agent_listen: 10.1.2.3:7301 {clear}")
    );
    let with_tls = "listen = \"0.0.0.0:7300\"\ntls_cert = \"/c\"\ntls_key = \"/k\"";
    let text = file(&server(&scratch, with_tls), &[&hosts(&scratch)]);
    let config = load(&scratch, &text).unwrap();
    assert_eq!(
        config.warnings(),
        [
            "server: warning: binds all interfaces (server.listen 0.0.0.0:7300); /v1/pools and /status are served only to private peers"
        ]
    );
    // The status pages are on `server.listen` alone, so the agent port's warning does
    // not mention them.
    let agent_wide = "agent_listen = \"0.0.0.0:7301\"\ntls_cert = \"/c\"\ntls_key = \"/k\"";
    let text = file(&server(&scratch, agent_wide), &[&hosts(&scratch)]);
    let config = load(&scratch, &text).unwrap();
    assert_eq!(
        config.warnings(),
        ["server: warning: binds all interfaces (server.agent_listen 0.0.0.0:7301)"]
    );
}

/// Asserts that an elastic pool needs the agent routes on a port of their own, with the
/// design's message: no `agent_listen`, and one equal to `listen`, are refused naming
/// `server.agent_listen`; a static pool alone needs none.
#[test]
fn an_elastic_pool_needs_an_agent_port_of_its_own() {
    let scratch = Scratch::new("config_agent_port");
    let pool = PoolText::elastic(&scratch);
    let wanted =
        "server.agent_listen: required with an elastic pool; must differ from server.listen";
    let none = file(&server(&scratch, ""), &[&pool]);
    assert_eq!(refusal(&scratch, &none), wanted);
    let same = "listen = \"127.0.0.1:7301\"\nagent_listen = \"127.0.0.1:7301\"";
    let text = file(&server(&scratch, same), &[&pool]);
    assert_eq!(refusal(&scratch, &text), wanted);
    let apart = "listen = \"127.0.0.1:7300\"\nagent_listen = \"127.0.0.1:7301\"";
    assert!(load(&scratch, &file(&server(&scratch, apart), &[&pool])).is_ok());
}

/// Asserts that `http://` off the loopback is refused for an elastic pool in each network
/// mode, naming `server.advertise_url`, and that `https`, the loopback names (`localhost`,
/// `127.0.0.1`, `[::1]`) and an address that does not parse as a URL are not refused by
/// this check; a static pool alone may advertise `http://`. The agent refuses a plain
/// `http://` address that is not loopback, so a pool that passed start would bill for
/// every launch it could not boot.
#[test]
fn plain_http_is_refused_for_agents_off_the_loopback() {
    let scratch = Scratch::new("config_advertise");
    let wanted = "server.advertise_url: http:// is refused by agents; use https (or loopback)";
    let oauth = oauth_file(&scratch);
    let tailscale =
        format!("{{ kind = \"tailscale\", oauth_client_file = {oauth}, tag = \"tag:js\" }}");
    for network in ["{ kind = \"default\" }", tailscale.as_str()] {
        let pool = PoolText::elastic(&scratch).set("network", network);
        let extra = "listen = \"127.0.0.1:0\"\nagent_listen = \"127.0.0.2:0\"\n\
                     advertise_url = \"http://js.example:7300\"";
        let text = file(&server(&scratch, extra), &[&pool]);
        assert_eq!(refusal(&scratch, &text), wanted, "{network}");
    }
    let pool = PoolText::elastic(&scratch);
    for url in [
        "https://js.example:7300",
        "http://localhost:7300",
        "http://127.0.0.1:7300",
        "http://[::1]:7300",
        "not a url",
    ] {
        let extra = format!(
            "listen = \"127.0.0.1:0\"\nagent_listen = \"127.0.0.2:0\"\nadvertise_url = {url:?}"
        );
        let text = file(&server(&scratch, &extra), &[&pool]);
        assert!(load(&scratch, &text).is_ok(), "{url}");
    }
    let plain = "advertise_url = \"http://js.example:7300\"";
    assert!(
        load(
            &scratch,
            &file(&server(&scratch, plain), &[&hosts(&scratch)])
        )
        .is_ok()
    );
}
