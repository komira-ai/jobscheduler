//! One layer for the whole service: a provider receives the opaque payload and nothing
//! else of the network, so the same launch under two modes gives calls that differ only in
//! the payload bytes, and no provider crate names a network type.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use js_elastic::{
    AcquireId, AcquireRequest, Acquired, BootPlan, BootStep, ElasticProvider, Held, HostName,
    HostRef, Offer, PriceUnit, ProviderError, Released, SchedulerId, Secret,
};
use js_lease_server::bootstrap::{BootSecrets, payload};
use js_lease_server::network::{DefaultMode, NetworkMode};
use js_types::ExecMode;

use crate::fake::{AUTH_KEY, Fake};
use crate::support::{TEN_MINUTES, clock, ctx, mode, plan};

/// What a provider saw of a launch, the payload apart.
#[derive(Debug, PartialEq, Eq)]
struct Call {
    acquire: String,
    pool: String,
    name: String,
    offer: String,
    labels: Vec<(String, String)>,
}

/// A provider that records its acquires. It can only be handed an `AcquireRequest`.
#[derive(Default)]
struct Recorder {
    calls: Mutex<Vec<(Call, String)>>,
}

impl ElasticProvider for Recorder {
    fn catalog(&self) -> Result<Vec<Offer>, ProviderError> {
        Ok(Vec::new())
    }

    fn host_prep(&self, _offer: &Offer) -> Vec<BootStep> {
        vec![BootStep::Run(vec!["true".to_owned()])]
    }

    fn payload_limit(&self) -> usize {
        64 * 1024
    }

    fn acquire(&self, request: &AcquireRequest) -> Acquired {
        let call = Call {
            acquire: request.acquire.as_str().to_owned(),
            pool: request.pool.clone(),
            name: request.name.to_string(),
            offer: request.offer.id.clone(),
            labels: request.labels.clone(),
        };
        self.calls
            .lock()
            .unwrap()
            .push((call, request.payload.expose().to_owned()));
        Acquired::Deferred {
            wait: Duration::ZERO,
        }
    }

    fn list(&self) -> Result<Vec<Held>, ProviderError> {
        Ok(Vec::new())
    }

    fn release(&self, _id: &HostRef) -> Released {
        Released::Unknown
    }
}

fn offer() -> Offer {
    Offer {
        id: "gpu_1x".to_owned(),
        zone: Some("zone-a".to_owned()),
        cpu_millis: 8000,
        memory_bytes: 1 << 34,
        disk_bytes: 1 << 38,
        gpus: 1,
        gpu_class: None,
        os: "linux".to_owned(),
        arch: "x86_64".to_owned(),
        exec: ExecMode::Container,
        price_cents_per_hour: Some(129),
        price_unit: PriceUnit::UsCents,
        available: true,
    }
}

/// The launch path of the service, as the scaler will walk it: the mode contributes, the
/// plan is built, rendered, and the provider is handed the payload.
fn launch(mode: &dyn NetworkMode, provider: &dyn ElasticProvider, name: &HostName) {
    let launch = ctx(name, TEN_MINUTES);
    let contribution = mode.prepare(&launch).unwrap();
    let mut boot: BootPlan = plan(name);
    boot.host_prep = provider.host_prep(&offer());
    boot.network = contribution.steps;
    let secrets =
        BootSecrets::new(Secret::new("join-MARK".to_owned()), contribution.secrets).unwrap();
    let body = payload(&boot, &secrets, provider.payload_limit()).unwrap();
    let request = AcquireRequest {
        acquire: AcquireId::new("acquire-1").unwrap(),
        pool: "gpu".to_owned(),
        name: name.clone(),
        offer: offer(),
        labels: vec![("team".to_owned(), "ci".to_owned())],
        payload: body,
    };
    provider.acquire(&request);
}

/// Asserts that the same launch under the default mode and under tailscale gives provider
/// calls equal in every field but the payload, and that the payloads differ: the mode
/// reaches the provider only as bytes it delivers as they are.
#[test]
fn a_provider_sees_the_same_call_under_every_mode() {
    let name = HostName::derive(
        &SchedulerId::new("sched").unwrap(),
        &AcquireId::new("acquire-1").unwrap(),
    );
    let fake = Fake::tailscale(name.as_str());
    let (_, shared) = clock();
    let tailscale = mode(&fake, shared);
    let default = DefaultMode::new("https://sched.example.org:7301", Vec::new());

    let under_default = Recorder::default();
    let under_tailscale = Recorder::default();
    launch(&default, &under_default, &name);
    launch(&tailscale, &under_tailscale, &name);

    let (call_d, payload_d) = under_default.calls.lock().unwrap().remove(0);
    let (call_t, payload_t) = under_tailscale.calls.lock().unwrap().remove(0);
    assert_eq!(call_d, call_t);
    assert_ne!(payload_d, payload_t);
    assert!(payload_t.contains(AUTH_KEY) && !payload_d.contains(AUTH_KEY));
    assert!(payload_t.contains("tailscale up"));
}

/// The words that make a file part of the network layer.
const NETWORK_WORDS: [&str; 8] = [
    "NetworkMode",
    "network::",
    "js_lease_server",
    "js-lease-server",
    "tailscale",
    "Contribution",
    "LaunchCtx",
    "BootSecrets",
];

/// The words a provider crate must not name beyond the network ones: the plan and the
/// renderer's fill belong to the service.
const SERVICE_WORDS: [&str; 2] = ["BootPlan", "Fill::"];

fn names_in(text: &str, words: &[&str]) -> Vec<String> {
    let lower = text.to_ascii_lowercase();
    words
        .iter()
        .filter(|w| lower.contains(&w.to_ascii_lowercase()))
        .map(|w| (*w).to_owned())
        .collect()
}

fn files_under(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files_under(&path, out);
        } else if matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("rs" | "toml")
        ) {
            out.push(path);
        }
    }
}

fn crate_dir(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join(name)
}

/// Asserts that the scan finds what it looks for: a planted network name is found, a
/// provider-side text with none is clean.
#[test]
fn the_scan_finds_a_network_name() {
    let planted = "use js_lease_server::network::NetworkMode;";
    assert_eq!(
        names_in(planted, &NETWORK_WORDS),
        ["NetworkMode", "network::", "js_lease_server"]
    );
    assert_eq!(names_in("Tailscale", &NETWORK_WORDS), ["tailscale"]);
    assert!(names_in("fn acquire(&self, r: &AcquireRequest)", &NETWORK_WORDS).is_empty());
    assert_eq!(names_in("BootPlan", &SERVICE_WORDS), ["BootPlan"]);
}

/// Asserts that no source or manifest of the Lambda provider names a network type, a mode,
/// the server crate, a boot plan or the renderer's fill, and that the elastic trait crate
/// names no network type: a provider cannot see a mode because nothing it depends on
/// holds one. (The layering test refuses a provider crate depending on the server.)
#[test]
fn no_provider_crate_names_a_network_type() {
    let mut lambda = Vec::new();
    files_under(&crate_dir("js-provider-lambdalabs"), &mut lambda);
    let mut elastic = Vec::new();
    files_under(&crate_dir("js-elastic"), &mut elastic);
    assert!(lambda.len() > 10, "the scan found {} files", lambda.len());
    assert!(elastic.len() >= 5, "the scan found {} files", elastic.len());

    let mut found = Vec::new();
    for (files, words) in [
        (
            &lambda,
            [NETWORK_WORDS.as_slice(), SERVICE_WORDS.as_slice()].concat(),
        ),
        (&elastic, NETWORK_WORDS.to_vec()),
    ] {
        for file in files {
            let text = fs::read_to_string(file).unwrap();
            for word in names_in(&text, &words) {
                found.push(format!("{}: {word}", file.display()));
            }
        }
    }
    assert!(found.is_empty(), "{found:#?}");
}

/// The files of `js-lease-server/src` that name the path `network::` besides the module
/// itself: the per-pool check calls a mode, the reader of the file builds the OAuth
/// client of a pool's `network` table, and the launch inputs hold a mode.
const MAY_NAME_NETWORK: [&str; 3] = ["checks.rs", "file.rs", "pool.rs"];

/// The sources (path under `src`, text) that name the path `network::` and are neither in
/// `network/` nor in `allowed`.
fn strays<'a>(sources: &'a [(String, String)], allowed: &[&str]) -> Vec<&'a str> {
    sources
        .iter()
        .filter(|(path, text)| {
            let mine = path.starts_with("network/") || allowed.contains(&path.as_str());
            !mine && text.to_ascii_lowercase().contains("network::")
        })
        .map(|(path, _)| path.as_str())
        .collect()
}

/// Every `.rs` and `.toml` file under `root`, as its path relative to `root` and its text.
fn sources_under(root: &Path) -> Vec<(String, String)> {
    let mut files = Vec::new();
    files_under(root, &mut files);
    let relative = |file: &PathBuf| file.strip_prefix(root).unwrap().display().to_string();
    files
        .iter()
        .map(|file| (relative(file), fs::read_to_string(file).unwrap()))
        .collect()
}

/// Asserts that the check of the design's 8.6 finds what it looks for: a file outside
/// `network/` that names the path is a stray, the allowed files and the module are not,
/// and a file that does not name it is clean.
#[test]
fn the_path_check_finds_a_stray_file() {
    let named = "use crate::network::NetworkMode;".to_owned();
    let sources = [
        ("agent_api.rs".to_owned(), named.clone()),
        ("checks.rs".to_owned(), named.clone()),
        ("network/mod.rs".to_owned(), named),
        (
            "rate.rs".to_owned(),
            "use crate::adapt::SharedClock;".to_owned(),
        ),
        ("util.rs".to_owned(), "use crate::NETWORK::Mode;".to_owned()),
    ];
    assert_eq!(
        strays(&sources, &MAY_NAME_NETWORK),
        ["agent_api.rs", "util.rs"]
    );
}

/// Asserts the check of the design's 8.6 over the tree: the service names `network::` only
/// in its `network/` module and the files that launch or check through a mode; and no
/// provider crate (every directory `js-provider*`, so a new one is scanned too),
/// `js-pool-static`, `js-registry` or `js-scaler` names a network type. It catches a
/// provider that reaches for a mode, and a core module of the service that starts to
/// depend on one.
#[test]
fn the_network_path_is_named_only_where_the_design_allows() {
    let service = sources_under(&crate_dir("js-lease-server").join("src"));
    assert!(service.len() > 30, "the scan found {} files", service.len());
    assert_eq!(strays(&service, &MAY_NAME_NETWORK), Vec::<&str>::new());

    let mut providers: Vec<PathBuf> = fs::read_dir(crate_dir("."))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("js-provider"))
        })
        .collect();
    providers.push(crate_dir("js-pool-static"));
    assert!(providers.len() >= 4, "the scan found {providers:?}");

    let all = [NETWORK_WORDS.as_slice(), SERVICE_WORDS.as_slice()].concat();
    let narrow = ["network::", "NetworkMode"];
    let mut found = Vec::new();
    let others = [crate_dir("js-registry"), crate_dir("js-scaler")];
    for (dir, words) in providers
        .iter()
        .map(|dir| (dir, all.as_slice()))
        .chain(others.iter().map(|dir| (dir, narrow.as_slice())))
    {
        for (path, text) in sources_under(dir) {
            for word in names_in(&text, words) {
                found.push(format!("{}/{path}: {word}", dir.display()));
            }
        }
    }
    assert!(found.is_empty(), "{found:#?}");
}
