//! The boot payload (it holds the machine's join token) and the private half of a key the
//! API makes appear in no log line, no `Debug`, no call log and no answer, at the most
//! verbose level, from this crate or from the HTTP client under it. This binary holds one
//! test: it installs the process's `log` sink.

#[allow(dead_code)]
#[path = "http/support.rs"]
mod support;

use std::io::Write;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use js_elastic::{
    AcquireId, AcquireRequest, Acquired, BootPayload, ElasticProvider, GpuClass, HostName, Offer,
    PriceUnit, SchedulerId,
};
use js_provider_lambdalabs::{
    ApiKey, Architecture, ElasticConfig, FakeLambda, HttpApi, InstanceTypeOffer, LambdaProvider,
    ManualClock, OptionTable, Region,
};
use js_types::ExecMode;
use support::{PATIENT, Reply, Scratch, Server, fixture};
use tracing_subscriber::fmt::MakeWriter;

const SECRET: &str = "JOIN-TOKEN-CANARY-5d2b";
const PRIVATE: &str = "PRIVATE-CANARY-7c1e";

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

fn ask(acquire: &str) -> AcquireRequest {
    let acquire = AcquireId::new(acquire).unwrap();
    AcquireRequest {
        name: HostName::derive(&SchedulerId::new("sched-one").unwrap(), &acquire),
        acquire,
        pool: "pool-a".to_owned(),
        offer: Offer {
            id: "gpu_1x_sample".to_owned(),
            zone: Some("region-a".to_owned()),
            cpu_millis: 8000,
            memory_bytes: 64 << 30,
            disk_bytes: 512 << 30,
            gpus: 1,
            gpu_class: Some(GpuClass::parse("nvidia-sample").unwrap()),
            os: "linux".to_owned(),
            arch: "x86_64".to_owned(),
            exec: ExecMode::Container,
            price_cents_per_hour: Some(75),
            price_unit: PriceUnit::UsCents,
            available: true,
        },
        labels: Vec::new(),
        payload: BootPayload::new(format!("#!/bin/sh\nprintf '%s' '{SECRET}' > /etc/token\n")),
    }
}

fn table() -> OptionTable {
    OptionTable::parse(
        "[[option]]\ninstance_type = \"gpu_1x_sample\"\ngpu_class = \"nvidia-sample\"\ngpus = 1\n",
    )
    .unwrap()
}

fn account_key(scheduler: SchedulerId) -> ElasticConfig {
    ElasticConfig::with_account_key(scheduler, NonZeroUsize::new(3).unwrap())
        .enable()
        .live()
}

#[test]
fn the_payload_and_a_made_keys_private_half_are_in_no_log_line() {
    log::set_logger(&Sink).unwrap();
    log::set_max_level(log::LevelFilter::Trace);
    let traced = Traced(Arc::default());
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(traced.clone())
        .finish();

    let server = Server::plain();
    server.on("GET /api/v1/ssh-keys", Reply::json(200, r#"{"data": []}"#));
    server.on(
        "POST /api/v1/ssh-keys",
        Reply::json(200, &fixture("ssh_key_generated.json")),
    );
    server.on(
        "GET /api/v1/firewall-rulesets",
        Reply::json(200, r#"{"data": []}"#),
    );
    server.on(
        "POST /api/v1/firewall-rulesets",
        Reply::json(200, &fixture("firewall_ruleset.json")),
    );
    server.on(
        "GET /api/v1/instances",
        Reply::json(200, &fixture("instances_empty.json")),
    );
    server.on(
        "POST /api/v1/instance-operations/launch",
        Reply::json(200, &fixture("launch.json")),
    );
    server.on(
        "POST /api/v1/instance-operations/launch",
        Reply::json(400, &fixture("error_insufficient_capacity.json")),
    );
    let scratch = Scratch::new("user-data-logging");
    let key = ApiKey::from_file(&scratch.key_file("an-api-key")).unwrap();
    let mut shown = String::new();
    tracing::subscriber::with_default(subscriber, || {
        let api = HttpApi::connect(&server.url("http"), &key, None, PATIENT).unwrap();
        let clock = Arc::new(ManualClock::new());
        let config = account_key(SchedulerId::new("sched-one").unwrap());
        let provider = LambdaProvider::new(Arc::new(api), clock.clone(), table(), config);
        let request = ask("acq-1");
        shown.push_str(&format!("{request:?}"));
        let first = provider.acquire(&request);
        assert!(matches!(first, Acquired::Accepted { .. }), "{first:?}");
        shown.push_str(&format!("{first:?}"));
        // The second launch is refused by the API; the refusal is quoted nowhere with the
        // request in it.
        clock.advance(Duration::from_secs(12));
        let second = provider.acquire(&ask("acq-2"));
        assert!(matches!(second, Acquired::Refused(_)), "{second:?}");
        shown.push_str(&format!("{second:?}"));

        // The same payload through the in-memory account: its launch log and call log.
        let fake = Arc::new(FakeLambda::new());
        fake.set_offers(vec![InstanceTypeOffer {
            name: "gpu_1x_sample".to_owned(),
            description: String::new(),
            gpu_description: String::new(),
            price_cents_per_hour: 75,
            vcpus: 8,
            memory_gib: 64,
            storage_gib: 512,
            gpus: 1,
            architecture: Architecture::X86_64,
            regions: vec![Region {
                name: "region-a".to_owned(),
                description: String::new(),
            }],
        }]);
        let config = account_key(SchedulerId::new("sched-one").unwrap());
        let provider =
            LambdaProvider::new(fake.clone(), Arc::new(ManualClock::new()), table(), config);
        let answer = provider.acquire(&ask("acq-3"));
        assert!(matches!(answer, Acquired::Accepted { .. }), "{answer:?}");
        // The fake did receive the text, for a test to check; its debug views do not show it.
        assert!(fake.launches()[0].user_data().unwrap().contains(SECRET));
        shown.push_str(&format!(
            "{:?}{:?}{answer:?}",
            fake.launches(),
            fake.calls()
        ));
    });

    // The server was sent the payload in the launch body: the logs are clean of a text
    // that did travel.
    let sent = server
        .seen()
        .into_iter()
        .filter(|s| s.method == "POST" && s.target.ends_with("/launch"))
        .map(|s| String::from_utf8(s.body).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(sent.len(), 2);
    for body in &sent {
        assert!(body.contains(SECRET), "the launch body lacks the payload");
    }
    let ours = String::from_utf8(traced.0.lock().unwrap().clone()).unwrap();
    let theirs = LINES.lock().unwrap().join("\n");
    // The capture works: this crate's lines and the HTTP client's both arrived.
    assert!(ours.contains("launching one instance"), "{ours}");
    assert!(
        ours.contains("path=\"/instance-operations/launch\""),
        "{ours}"
    );
    assert!(
        theirs.lines().any(|line| line.starts_with("ureq")),
        "{theirs}"
    );
    for (name, text) in [("tracing", &ours), ("log", &theirs), ("debug text", &shown)] {
        assert!(
            !text.contains(SECRET),
            "the payload is in the {name} output"
        );
        assert!(
            !text.contains(PRIVATE),
            "a private key is in the {name} output"
        );
    }
}
