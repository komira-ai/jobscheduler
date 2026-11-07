//! The EC2 account of the live conformance suite: what the flags say, the table of the `ec2`
//! kind they make, the provider that table builds, and the transport that writes the answers
//! of the AWS APIs to files.
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
use js_provider_ec2::kind::{self, Context};
use js_provider_ec2::{Credentials, HttpEc2, Region, SystemClock};
use toml::{Table, Value};

/// The flags beyond the shared ones.
pub const EXTRA: &[&str] = &["image", "arch", "capture-dir"];

/// The suffix of the host name of every API whose answers are captured.
const AWS_HOST_SUFFIX: &str = ".amazonaws.com";

/// A transport over another that writes the body of each answer of an AWS API to a file. It
/// writes no request and no header: they carry signatures, and the answers of the instance
/// metadata service, which carry role credentials, are not from an AWS API host. The suite
/// puts it over the real client; the tests put it over a canned one.
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

/// The EC2 `Action` of a form body, or the operation of an `x-amz-target` header: what the
/// request asked for. `call` when the request names neither. Anything but a letter, a digit
/// or '-' becomes '_', so the text is safe in a file name.
pub fn what_of(request: &HttpRequest) -> String {
    let from_form = String::from_utf8_lossy(&request.body)
        .split('&')
        .find_map(|pair| pair.strip_prefix("Action=").map(str::to_owned));
    let from_target = request
        .header("x-amz-target")
        .map(|target| target.rsplit('.').next().unwrap_or(target).to_owned());
    let what = from_form
        .or(from_target)
        .unwrap_or_else(|| "call".to_owned());
    what.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Whether `url` is on an AWS API host over https.
pub fn is_aws_api(url: &str) -> bool {
    url.strip_prefix("https://")
        .and_then(|rest| rest.split('/').next())
        .is_some_and(|host| host.ends_with(AWS_HOST_SUFFIX))
}

impl<T: Transport> Transport for Recording<T> {
    fn send(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
        let record = is_aws_api(&request.url).then(|| what_of(&request));
        let answer = self.inner.send(request)?;
        if let Some(what) = record {
            self.write(&what, answer.status, &answer.body);
        }
        Ok(answer)
    }
}

/// The EC2 account the run launches in.
pub struct Ec2Cloud {
    region: String,
    image: String,
    arch: String,
    shape: String,
    capture: Option<Arc<Recording<UreqTransport>>>,
}

impl Ec2Cloud {
    /// What the flags say, checked as far as the flags can be: the provider's own rules
    /// (the region and image forms) are checked when it is built. `args.shape` is the shape
    /// as the suite is to look for it, an offer id (see [`offer_id`]).
    pub fn from_args(args: &LiveArgs, raw: &[String]) -> Result<Self, String> {
        if raw.iter().any(|arg| arg == "--zone") {
            return Err("--zone is for Compute Engine; EC2 takes --region".to_owned());
        }
        let arch = args.extra.get("arch").map_or("x86_64", String::as_str);
        if !["x86_64", "arm64"].contains(&arch) {
            return Err("--arch must be x86_64 or arm64".to_owned());
        }
        let capture = args
            .extra
            .get("capture-dir")
            .map(|dir| Recording::new(Path::new(dir)).map(Arc::new))
            .transpose()?;
        Ok(Self {
            region: args.location.clone(),
            image: args.required("image")?.to_owned(),
            arch: arch.to_owned(),
            shape: args.shape.clone(),
            capture,
        })
    }

    /// The provider table of the `ec2` kind.
    pub fn table(&self) -> Table {
        let mut images = Table::new();
        images.insert(self.arch.clone(), Value::String(self.image.clone()));
        let allowed = vec![Value::String(type_of(&self.shape).to_owned())];
        let mut table = Table::new();
        table.insert("region".to_owned(), Value::String(self.region.clone()));
        table.insert("images".to_owned(), Value::Table(images));
        table.insert("allowed_instance_types".to_owned(), Value::Array(allowed));
        table
    }
}

impl LiveCloud for Ec2Cloud {
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
                let region = Region::new(&self.region).map_err(|why| format!("region: {why}"))?;
                let transport: Arc<dyn Transport> = recording.clone();
                let api = HttpEc2::new(
                    region,
                    Credentials::standard(),
                    transport,
                    Arc::new(WallClock),
                );
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
/// '/'), else the on-demand offer of that instance type.
pub fn offer_id(shape: &str) -> String {
    match shape.contains('/') {
        true => shape.to_owned(),
        false => format!("{shape}/on-demand"),
    }
}

/// The instance type an offer id names: the part before the first '/'.
pub fn type_of(offer: &str) -> &str {
    offer.split(['/']).next().unwrap_or(offer)
}

/// The usage line.
pub const USAGE: &str = "usage: live --run-id ID --region REGION --image AMI --shape TYPE \
                     [--arch x86_64|arm64] [--capture-dir DIR] [--spend --plan-hash HASH]\n\
                     --shape is required: with no type allowed the catalog holds only the types \
                     with a GPU";

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
    let cloud = match Ec2Cloud::from_args(&args, raw) {
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
