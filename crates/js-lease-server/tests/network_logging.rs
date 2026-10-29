//! No secret of the network layer appears in a log line, at the most verbose level, from
//! this crate or from the HTTP client under it. This binary holds one test: it installs the
//! process's `log` sink and a `tracing` subscriber.

#[allow(dead_code)]
#[path = "network/fake.rs"]
mod fake;

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use js_elastic::{AcquireId, HostName, SchedulerId, Secret};
use js_lease::{ManualClock, Timestamp};
use js_lease_server::SharedClock;
use js_lease_server::network::{
    API_TIMEOUT, LaunchCtx, NetworkMode, OAuthClient, TailscaleApi, TailscaleMode,
    TailscaleSettings,
};
use tracing_subscriber::fmt::MakeWriter;

use fake::{AUTH_KEY, CLIENT_SECRET, Fake, Reply, TOKEN};

static LINES: Mutex<Vec<String>> = Mutex::new(Vec::new());

struct Sink;

impl log::Log for Sink {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &log::Record<'_>) {
        LINES
            .lock()
            .unwrap()
            .push(format!("{} {}", record.target(), record.args()));
    }

    fn flush(&self) {}
}

#[derive(Clone)]
struct Traced(Arc<Mutex<Vec<u8>>>);

impl Write for Traced {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Traced {
    type Writer = Self;

    fn make_writer(&'a self) -> Self {
        self.clone()
    }
}

#[test]
fn the_secrets_are_in_no_log_line() {
    log::set_logger(&Sink).unwrap();
    log::set_max_level(log::LevelFilter::Trace);
    let traced = Traced(Arc::default());
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(traced.clone())
        .finish();

    let host = HostName::derive(
        &SchedulerId::new("sched").unwrap(),
        &AcquireId::new("a").unwrap(),
    );
    let fake = Fake::tailscale(host.as_str());
    // A server that echoes the secrets back in its errors, on the routes after the first.
    fake.on(
        "POST /api/v2/tailnet/-/keys",
        Reply::json(
            400,
            &format!(r#"{{"message":"{CLIENT_SECRET} {TOKEN} {AUTH_KEY}"}}"#),
        ),
    );
    fake.set(
        "DELETE /api/v2/device/d1",
        Reply::json(
            500,
            &format!(r#"{{"message":"{CLIENT_SECRET} {TOKEN} {AUTH_KEY}"}}"#),
        ),
    );
    let manual = ManualClock::new(Timestamp::from_millis(1_000));
    let client = OAuthClient::new("cid", Secret::new(CLIENT_SECRET.to_owned()));
    let api = TailscaleApi::new(
        &fake.url(),
        "-",
        client,
        SharedClock::new(manual),
        API_TIMEOUT,
    )
    .unwrap();
    let settings = TailscaleSettings {
        tag: "tag:js-agent".to_owned(),
        binary: js_elastic::Artifact {
            url: "https://pkgs.example.org/ts.tgz".to_owned(),
            sha256: [1; 32],
        },
    };
    let mode = TailscaleMode::new(api, settings).unwrap();

    let mut shown = String::new();
    tracing::subscriber::with_default(subscriber, || {
        let launch = LaunchCtx {
            pool: "gpu",
            host: &host,
            valid_for: Duration::from_secs(600),
        };
        // The first mint works, the second is refused with an echo; the delete fails.
        shown.push_str(&format!("{:?}", mode.prepare(&launch)));
        shown.push_str(&format!("{:?}", mode.prepare(&launch)));
        shown.push_str(&format!("{:?}", mode.check()));
        shown.push_str(&format!("{:?}", mode.forget(&host)));
        shown.push_str(&format!("{mode:?}"));
    });

    // Every authorised request did carry the token; the logs did not.
    let seen = fake.seen();
    assert!(seen.len() >= 5, "{} requests", seen.len());
    let carried = seen
        .iter()
        .filter(|s| s.header("authorization") == Some(format!("Bearer {TOKEN}").as_str()))
        .count();
    assert!(carried >= 4, "{carried} requests carried the token");

    let ours = String::from_utf8(traced.0.lock().unwrap().clone()).unwrap();
    let theirs = LINES.lock().unwrap().join("\n");
    // The capture works: the HTTP client's lines arrived.
    assert!(
        theirs.lines().any(|line| line.starts_with("ureq")),
        "{theirs}"
    );
    for (name, text) in [("tracing", &ours), ("log", &theirs), ("debug text", &shown)] {
        for secret in [CLIENT_SECRET, TOKEN, AUTH_KEY] {
            assert!(!text.contains(secret), "{secret} is in the {name} output");
        }
        assert!(
            !text.contains("Bearer "),
            "an authorization value is in the {name} output"
        );
    }
}
