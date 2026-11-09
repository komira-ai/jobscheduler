//! `--metrics-listen` and `server.metrics_listen`: the setting, the rule that only a private
//! address serves the metrics, an address in use, and a scrape over the listener of a running
//! server. A server without the setting binds no metrics listener.

use std::ffi::OsString;
use std::net::SocketAddr;

use js_lease_server::{Config, SharedClock, StartupError, channel_ticker, start};
use js_types::ManualClock;

use crate::config_support::{PoolText, file, load, refusal, server};
use crate::support::{Harness, Scratch, TOKEN, args_with, base_args, config, request};

/// The content type of the Prometheus text format.
const TEXT_FORMAT: &str = "text/plain; version=0.0.4; charset=utf-8";

/// A file of one static pool, with `extra` in `[server]`.
fn static_file(scratch: &Scratch, extra: &str) -> String {
    let hosts = PoolText::static_pool(scratch, "hosts");
    file(&server(scratch, extra), &[&hosts])
}

/// The base flags with the metrics on `listen`.
fn metrics_args(scratch: &Scratch, listen: &str) -> Vec<OsString> {
    args_with(scratch, &["--metrics-listen", listen])
}

/// What the refusal of a start says: its flag and its message.
fn refused<T>(result: Result<T, StartupError>) -> (&'static str, String) {
    let err = result.err().expect("the start should be refused");
    (err.flag(), err.to_string())
}

/// Starts the server `config` describes on a clock that stands still.
async fn started(config: Config) -> Result<js_lease_server::Running, StartupError> {
    let (_, ticker) = channel_ticker();
    start(config, SharedClock::new(ManualClock::default()), ticker).await
}

/// Asserts that the flag and the file key give the same address, that neither given is off,
/// and that the key must be an address. Unlike the job service's, the metrics need no agent
/// registration: a static pool serves them. It catches a key read and dropped, and metrics
/// tied to the agent side.
#[test]
fn metrics_listen_flag_and_config_key_agree() {
    let scratch = Scratch::new("metrics_listen_agree");
    let want: SocketAddr = "10.1.2.3:9100".parse().unwrap();
    let by_flag = config(metrics_args(&scratch, "10.1.2.3:9100")).unwrap();
    assert_eq!(by_flag.metrics_listen(), Some(want));
    let by_key = load(
        &scratch,
        &static_file(&scratch, "metrics_listen = \"10.1.2.3:9100\""),
    );
    assert_eq!(by_key.unwrap().metrics_listen(), Some(want));
    assert_eq!(config(base_args(&scratch)).unwrap().metrics_listen(), None);
    let without = load(&scratch, &static_file(&scratch, "")).unwrap();
    assert_eq!(without.metrics_listen(), None);
    assert_eq!(
        refusal(&scratch, &static_file(&scratch, "metrics_listen = 9100")),
        "server.metrics_listen: must be a string"
    );
}

/// Asserts the address rule: the metrics take no token, so an address that is not a private
/// one is refused at start naming `--metrics-listen` or `server.metrics_listen`, with the whole
/// reason, and needs no TLS; a private address of each kind (loopback, RFC 1918, the shared
/// range a tailnet uses, IPv6 unique-local) and a wildcard (whose peers the route checks one
/// by one) are accepted without TLS. It catches metrics served on a public address, and a
/// metrics port held to the TLS rule of the ports that carry a token.
#[test]
fn a_metrics_address_that_is_not_private_is_refused_naming_the_flag() {
    let scratch = Scratch::new("metrics_listen_private");
    let reason = "is not a private address: the metrics take no token, so they are served on a private address only";
    for public in ["192.0.2.1:9100", "[2001:db8::1]:9100"] {
        assert_eq!(
            refused(config(metrics_args(&scratch, public))),
            (
                "--metrics-listen",
                format!("--metrics-listen: {public} {reason}")
            ),
            "{public}"
        );
        let key = format!("metrics_listen = \"{public}\"");
        assert_eq!(
            refusal(&scratch, &static_file(&scratch, &key)),
            format!("server.metrics_listen: {public} {reason}"),
        );
    }
    let private = [
        "127.0.0.1:9100",
        "10.0.0.5:9100",
        "100.64.0.1:9100",
        "[fd00::1]:9100",
    ];
    for addr in private.into_iter().chain(["0.0.0.0:9100"]) {
        assert!(config(metrics_args(&scratch, addr)).is_ok(), "{addr}");
        let key = format!("metrics_listen = \"{addr}\"");
        assert!(
            load(&scratch, &static_file(&scratch, &key)).is_ok(),
            "{addr}"
        );
    }
}

/// Asserts that a metrics address already in use refuses the start naming the flag that gave
/// it, `--metrics-listen` or `server.metrics_listen`, and that the refused start leaves the
/// lease file free: a server on the same files starts afterwards. It catches a bind failure
/// reported as another flag's, or one that leaves the server half started.
#[tokio::test]
async fn a_metrics_address_in_use_is_refused_naming_the_flag() {
    let busy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let taken = busy.local_addr().unwrap().to_string();
    let scratch = Scratch::new("metrics_listen_in_use");
    let flag = config(metrics_args(&scratch, &taken)).unwrap();
    assert_eq!(refused(started(flag).await).0, "--metrics-listen");
    let again = config(metrics_args(&scratch, "127.0.0.1:0")).unwrap();
    started(again).await.unwrap().shutdown().await.unwrap();

    let other = Scratch::new("metrics_listen_in_use_file");
    let key = format!("listen = \"127.0.0.1:0\"\nmetrics_listen = {taken:?}");
    let key = load(&other, &static_file(&other, &key)).unwrap();
    assert_eq!(refused(started(key).await).0, "server.metrics_listen");
}

/// Asserts a scrape over the listener of a running server: `GET /metrics` answers 200 with the
/// text format's content type, a family of each kind, and the static pool's hosts, free; no
/// job family, since the job service is off; and not the client token. The lease port does
/// not serve `/metrics`, even with the token, and the metrics port serves nothing else. Its
/// listener stops with the server. It catches metrics served on the lease port, and a scrape
/// that fails without the job tables.
#[tokio::test]
async fn a_scrape_answers_200_in_the_text_format_on_its_own_port_only() {
    let scratch = Scratch::new("metrics_listen_scrape");
    let args = metrics_args(&scratch, "127.0.0.1:0");
    let h = Harness::with(scratch, args, None).await;
    let addr = h.running.metrics_addr().unwrap();
    assert_ne!(addr, h.addr());
    let scraped = request(addr, "GET", "/metrics", None, None).await;
    assert_eq!(scraped.status, 200, "{}", scraped.body);
    assert!(
        scraped
            .head
            .contains(&format!("content-type: {TEXT_FORMAT}")),
        "{}",
        scraped.head
    );
    for line in [
        "# TYPE js_job_placements_total counter\njs_job_placements_total 0\n",
        "# TYPE js_queue_wait_seconds histogram\n",
        "js_host_cpu_booked_millicores{host=\"gpu-01\"} 0\n",
        "# TYPE js_host_whole_mark gauge\n",
    ] {
        assert!(
            scraped.body.contains(line),
            "{line} is not in:\n{}",
            scraped.body
        );
    }
    assert!(!scraped.body.contains("js_jobs{"), "{}", scraped.body);
    assert!(!scraped.body.contains(TOKEN), "{}", scraped.body);
    assert_eq!(
        request(h.addr(), "GET", "/metrics", Some(TOKEN), None)
            .await
            .status,
        404
    );
    assert_eq!(
        request(addr, "GET", "/v1/hosts", Some(TOKEN), None)
            .await
            .status,
        404
    );
    h.running.shutdown().await.unwrap();
    assert!(tokio::net::TcpStream::connect(addr).await.is_err());
}

/// Asserts that a server started without `--metrics-listen` binds no metrics listener.
#[tokio::test]
async fn without_metrics_listen_no_metrics_listener_is_bound() {
    let h = Harness::new("metrics_listen_off").await;
    assert_eq!(h.running.metrics_addr(), None);
}
