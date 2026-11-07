//! The Compute Engine project of the live conformance suite: what the flags say, the table of
//! the `gce` kind they make, and the transport that writes the answers of the two APIs to
//! files.
//!
//! It is included by `tests/live.rs`, which is only the `main` of the suite and is built with
//! the cargo feature `live`, and by `tests/live_cloud.rs`, an ordinary test target that
//! exercises everything here without a network: nothing in this file sends a request itself,
//! and the tests give the recording transport a canned one in place of the real client.

use std::fs;
use std::io::Write;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use js_elastic::conformance::live::{LiveArgs, LiveCloud, run};
use js_elastic::{CapacityProvider, Offer, SchedulerId};
use js_logstore::{
    HttpRequest, HttpResponse, SystemClock as WallClock, Transport, TransportError, UreqTransport,
};
use js_provider_gce::kind::{self, Context};
use js_provider_gce::{Credentials, HttpGce, SystemClock};
use toml::{Table, Value};

/// The flags beyond the shared ones.
pub const EXTRA: &[&str] = &[
    "project",
    "image",
    "arch",
    "network",
    "subnetwork",
    "capture-dir",
];

/// The hosts of the two APIs whose answers are captured. The token exchange and the
/// metadata server answer with access tokens and are on other hosts.
const API_HOSTS: [&str; 2] = ["compute.googleapis.com", "cloudbilling.googleapis.com"];

/// A transport over another that writes the body of each answer of the two APIs to a file. It
/// writes no request and no header: they carry access tokens. The suite puts it over the real
/// client; the tests put it over a canned one.
pub struct Recording<T> {
    inner: T,
    dir: PathBuf,
    next: AtomicUsize,
}

impl Recording<UreqTransport> {
    /// A recording transport over the real client, writing to `dir`, which is created if it
    /// is missing.
    pub fn new(dir: &Path) -> Result<Self, String> {
        Self::over(UreqTransport::new(kind::API_TIMEOUT), dir)
    }
}

impl<T> Recording<T> {
    /// A recording transport over `inner`, writing to `dir`, which is created if it is
    /// missing.
    pub fn over(inner: T, dir: &Path) -> Result<Self, String> {
        fs::create_dir_all(dir).map_err(|why| format!("--capture-dir {}: {why}", dir.display()))?;
        Ok(Self {
            inner,
            dir: dir.to_owned(),
            next: AtomicUsize::new(1),
        })
    }

    /// The name of the file that holds the answer numbered `seq`: `<seq>-<what>-<status>.body`,
    /// with the number in at least three digits.
    pub fn file_name(seq: usize, what: &str, status: u16) -> String {
        format!("{seq:03}-{what}-{status}.body")
    }

    /// Writes `body` to the next numbered file of the directory. A file that cannot be written
    /// is reported and does not stop the run.
    fn write(&self, what: &str, status: u16, body: &[u8]) {
        let seq = self.next.fetch_add(1, Ordering::SeqCst);
        let path = self.dir.join(Self::file_name(seq, what, status));
        if let Err(why) = fs::write(&path, body) {
            eprintln!("capture: cannot write {}: {why}", path.display());
        }
    }
}

/// What the request asked for: its method and the last two segments of its path, without
/// the query. `None` when the request is not to one of the two APIs over https.
pub fn what_of(request: &HttpRequest) -> Option<String> {
    let rest = request.url.strip_prefix("https://")?;
    let (host, path) = rest.split_once('/')?;
    if !API_HOSTS.contains(&host) {
        return None;
    }
    let path = path.split('?').next().unwrap_or_default();
    let segments: Vec<&str> = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    let tail = segments.len().saturating_sub(2);
    let what = format!("{}-{}", request.method.as_str(), segments[tail..].join("_"));
    Some(
        what.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect(),
    )
}

impl<T: Transport> Transport for Recording<T> {
    fn send(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
        let record = what_of(&request);
        let answer = self.inner.send(request)?;
        if let Some(what) = record {
            self.write(&what, answer.status, &answer.body);
        }
        Ok(answer)
    }
}

/// The project the run launches in.
pub struct GceCloud {
    project: String,
    zone: String,
    image: String,
    arch: String,
    network: String,
    subnetwork: String,
    shape: String,
    capture: Option<Arc<Recording<UreqTransport>>>,
}

impl GceCloud {
    /// What the flags say, checked as far as the flags can be: the provider's own rules
    /// (the forms of the zone, the image and the network) are checked when it is built.
    /// `args.shape` is the shape as the suite is to look for it, an offer id (see
    /// [`offer_id`]).
    pub fn from_args(args: &LiveArgs, raw: &[String]) -> Result<Self, String> {
        if raw.iter().any(|arg| arg == "--region") {
            return Err("--region is for EC2 and Lambda; Compute Engine takes --zone".to_owned());
        }
        let arch = args.extra.get("arch").map_or("x86_64", String::as_str);
        if !["x86_64", "arm64"].contains(&arch) {
            return Err("--arch must be x86_64 or arm64".to_owned());
        }
        let zone = args.location.clone();
        // A zone is its region and a letter: us-central1-a is in us-central1.
        let region = zone
            .rsplit_once('-')
            .map_or(zone.as_str(), |(region, _)| region);
        let network = args
            .extra
            .get("network")
            .cloned()
            .unwrap_or_else(|| "global/networks/default".to_owned());
        let subnetwork = args
            .extra
            .get("subnetwork")
            .cloned()
            .unwrap_or_else(|| format!("regions/{region}/subnetworks/default"));
        let capture = args
            .extra
            .get("capture-dir")
            .map(|dir| Recording::new(Path::new(dir)).map(Arc::new))
            .transpose()?;
        Ok(Self {
            project: args.required("project")?.to_owned(),
            image: args.required("image")?.to_owned(),
            arch: arch.to_owned(),
            network,
            subnetwork,
            shape: args.shape.clone(),
            zone,
            capture,
        })
    }

    /// The provider table of the `gce` kind.
    pub fn table(&self) -> Table {
        let mut images = Table::new();
        images.insert(self.arch.clone(), Value::String(self.image.clone()));
        let text = |value: &str| Value::String(value.to_owned());
        let mut table = Table::new();
        table.insert("project".to_owned(), text(&self.project));
        table.insert("zones".to_owned(), Value::Array(vec![text(&self.zone)]));
        table.insert("network".to_owned(), text(&self.network));
        table.insert("subnetwork".to_owned(), text(&self.subnetwork));
        table.insert("images".to_owned(), Value::Table(images));
        let allowed = Value::Array(vec![text(type_of(&self.shape))]);
        table.insert("allowed_machine_types".to_owned(), allowed);
        table
    }
}

impl LiveCloud for GceCloud {
    fn provider(
        &self,
        scheduler: &SchedulerId,
        machines: NonZeroUsize,
    ) -> Result<Box<dyn CapacityProvider>, String> {
        let table = self.table();
        let ctx = Context {
            scheduler: scheduler.clone(),
            max_machines: machines,
            live: true,
            clock: Arc::new(SystemClock::new()),
        };
        let built = match &self.capture {
            None => kind::build(&table, &ctx),
            Some(recording) => {
                // The credential error names the variable and never what the key file holds.
                let credentials = Credentials::standard().map_err(|why| why.to_string())?;
                let transport: Arc<dyn Transport> = recording.clone();
                let api = HttpGce::new(credentials, transport, Arc::new(WallClock));
                kind::build_over(&table, &ctx, Arc::new(api))
            }
        }
        .map_err(|refused| format!("{}: {}", refused.field, refused.reason))?;
        Ok(Box::new(built.provider))
    }

    fn launchable(&self, offer: &Offer) -> bool {
        offer.arch == self.arch
    }
}

/// The offer id for a `--shape` value: the value itself when it is an offer id (it holds a
/// '/'), else the on-demand offer of that machine type.
pub fn offer_id(shape: &str) -> String {
    match shape.contains('/') {
        true => shape.to_owned(),
        false => format!("{shape}/on-demand"),
    }
}

/// The machine type an offer id names: the part before the first '/' or '+'.
pub fn type_of(offer: &str) -> &str {
    offer.split(['/', '+']).next().unwrap_or(offer)
}

/// The usage line.
pub const USAGE: &str = "usage: live --run-id ID --zone ZONE --project PROJECT --image IMAGE \
                     --shape TYPE [--arch x86_64|arm64] [--network NETWORK] \
                     [--subnetwork NAME] [--capture-dir DIR] [--spend --plan-hash HASH]\n\
                     --shape is required: with no type allowed the catalog holds only the offers \
                     with an accelerator";

/// Runs the suite for `raw` (the arguments without the program name), writing the run to
/// `out` and a usage error to `err`: the exit status (0 passed or dry run done, 1 failed,
/// 2 usage error). `main` only forwards to it.
pub fn main_with(raw: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let mut usage_error = |message: &str| {
        let _ = writeln!(err, "{message}\n{USAGE}");
        2
    };
    let mut args = match LiveArgs::parse(raw, EXTRA) {
        Ok(args) => args,
        Err(message) => return usage_error(&message),
    };
    args.shape = offer_id(&args.shape);
    let cloud = match GceCloud::from_args(&args, raw) {
        Ok(cloud) => cloud,
        Err(message) => return usage_error(&message),
    };
    // The catalog runs its blocking calls on tokio's blocking pool, and the driver waits for
    // it without a runtime of its own.
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .build()
    {
        Ok(runtime) => runtime,
        Err(why) => {
            let _ = writeln!(err, "cannot start the runtime: {why}");
            return 1;
        }
    };
    let entered = runtime.enter();
    let status = run(&args, &cloud, out);
    let _ = out.flush();
    drop(entered);
    drop(runtime);
    status
}
