//! The API key appears in no log line, at the most verbose level, from this crate or from
//! the HTTP client under it. This binary holds one test: it installs the process's `log`
//! sink.

#[allow(dead_code)]
#[path = "http/support.rs"]
mod support;

use std::io::Write;
use std::sync::{Arc, Mutex};

use js_provider_lambdalabs::{ApiKey, HttpApi, InstanceId, LambdaApi};
use support::{PATIENT, Reply, Scratch, Server, fixture};
use tracing_subscriber::fmt::MakeWriter;

const KEY: &str = "LLKEY-3f9a7c1e-canary-0b6d";

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
fn the_key_is_in_no_log_line() {
    log::set_logger(&Sink).unwrap();
    log::set_max_level(log::LevelFilter::Trace);
    let traced = Traced(Arc::default());
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(traced.clone())
        .finish();

    let server = Server::plain();
    server.on(
        "GET /api/v1/instance-types",
        Reply::json(200, &fixture("instance_types.json")),
    );
    server.on(
        "GET /api/v1/instances",
        Reply::json(429, &fixture("error_rate_limited.json")),
    );
    server.on(
        "GET /api/v1/instances/a1b2c3d4e5f60718293a4b5c6d7e8f90",
        Reply::json(404, &fixture("error_object_does_not_exist.json")),
    );
    let scratch = Scratch::new("logging");
    let key = ApiKey::from_file(&scratch.key_file(KEY)).unwrap();
    let mut shown = format!("{key:?}");
    tracing::subscriber::with_default(subscriber, || {
        let api = HttpApi::connect(&server.url("http"), &key, None, PATIENT).unwrap();
        shown.push_str(&format!("{api:?}"));
        shown.push_str(&format!("{:?}", api.instance_types().map(|t| t.len())));
        shown.push_str(&format!("{:?}", api.list_instances().map(|l| l.len())));
        let id = InstanceId::new("a1b2c3d4e5f60718293a4b5c6d7e8f90").unwrap();
        shown.push_str(&format!("{:?}", api.get_instance(&id).map(|i| i.status)));
    });

    // The request did carry the key; the logs did not.
    assert_eq!(
        server.seen()[0].header("authorization"),
        Some(format!("Bearer {KEY}").as_str())
    );
    let ours = String::from_utf8(traced.0.lock().unwrap().clone()).unwrap();
    let theirs = LINES.lock().unwrap().join("\n");
    // The capture works: this crate's lines and the HTTP client's both arrived.
    assert!(ours.contains("path=\"/instance-types\""), "{ours}");
    assert!(
        theirs.lines().any(|line| line.starts_with("ureq")),
        "{theirs}"
    );
    for (name, text) in [("tracing", &ours), ("log", &theirs), ("debug text", &shown)] {
        assert!(!text.contains(KEY), "the key is in the {name} output");
        assert!(
            !text.contains("Bearer "),
            "an authorization value is in the {name} output"
        );
    }
}
