//! The helpers of the Compute Engine live suite (`tests/live/cloud.rs`), tested without a
//! network: what the flags make of the project, the table of the `gce` kind, the names of the
//! files the recording transport writes, and the recording itself over a canned transport.
//! Nothing here sends a request, and no test reaches the real cloud.

use std::fs;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use js_elastic::conformance::live::{LiveArgs, LiveCloud};
use js_elastic::{Offer, PriceUnit, SchedulerId};
use js_logstore::{HttpRequest, HttpResponse, Method, Transport, TransportError};
use js_provider_gce::kind::{self, Context};
use js_provider_gce::{FakeGce, ManualClock};
use js_types::ExecMode;
use toml::Table;

#[path = "live/cloud.rs"]
mod cloud;

use cloud::{USAGE, main_with};

use cloud::{EXTRA, GceCloud, Recording, offer_id, type_of, what_of};

const IMAGE: &str = "projects/debian-cloud/global/images/family/debian-12";

/// The account the arguments `raw` make, as `main_with` makes it.
fn cloud_from(raw: &[&str]) -> Result<GceCloud, String> {
    let raw: Vec<String> = raw.iter().map(|arg| (*arg).to_owned()).collect();
    let mut args = LiveArgs::parse(&raw, EXTRA)?;
    args.shape = offer_id(&args.shape);
    GceCloud::from_args(&args, &raw)
}

/// The account of a run in `us-central1-a` with a shape, a project and an image, and `more`
/// besides.
fn cloud_of(more: &[&str]) -> Result<GceCloud, String> {
    let base = [
        "--run-id",
        "r1",
        "--shape",
        "n1-standard-1",
        "--zone",
        "us-central1-a",
        "--project",
        "my-project",
        "--image",
        IMAGE,
    ];
    cloud_from(&[&base[..], more].concat())
}

fn table(text: &str) -> Table {
    text.parse().unwrap()
}

/// A directory for one test, empty, under the test target's scratch directory.
fn scratch(name: &str) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("gce-live-cloud")
        .join(format!("{name}-{}", NEXT.fetch_add(1, Ordering::SeqCst)));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

fn context(live: bool) -> Context {
    Context {
        scheduler: SchedulerId::new("live-r1").unwrap(),
        max_machines: NonZeroUsize::new(1).unwrap(),
        live,
        clock: Arc::new(ManualClock::new()),
    }
}

/// Builds the provider of `cloud`'s table over the in-memory project: the same rules as the
/// kind's `build`, which reads a credential from the environment and so is not called here.
fn build(cloud: &GceCloud, live: bool) -> Result<(), kind::Refused> {
    kind::build_over(&cloud.table(), &context(live), Arc::new(FakeGce::new())).map(|_| ())
}

fn offer(arch: &str) -> Offer {
    Offer {
        id: "n1-standard-1/on-demand".to_owned(),
        zone: None,
        cpu_millis: 2000,
        memory_bytes: 1 << 30,
        disk_bytes: 1 << 33,
        accelerator: None,
        os: "linux".to_owned(),
        arch: arch.to_owned(),
        isa_level: None,
        exec: ExecMode::Container,
        price_cents_per_hour: Some(1),
        price_unit: PriceUnit::UsCents,
        available: true,
        purchase: None,
        quota: None,
        block: None,
    }
}

fn request(method: Method, url: &str) -> HttpRequest {
    HttpRequest {
        method,
        url: url.to_owned(),
        headers: vec![("authorization".to_owned(), "Bearer secret-token".to_owned())],
        body: Vec::new(),
    }
}

/// A transport that answers every request with a canned answer and keeps the URLs it saw.
struct Canned {
    status: u16,
    body: &'static [u8],
    fail: bool,
    urls: Mutex<Vec<String>>,
}

impl Canned {
    fn new(status: u16, body: &'static [u8]) -> Self {
        Self {
            status,
            body,
            fail: false,
            urls: Mutex::new(Vec::new()),
        }
    }
}

impl Transport for Canned {
    fn send(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
        self.urls.lock().unwrap().push(request.url);
        match self.fail {
            true => Err(TransportError::Timeout),
            false => Ok(HttpResponse {
                status: self.status,
                headers: Vec::new(),
                body: self.body.to_vec(),
            }),
        }
    }
}

#[test]
fn the_flags_make_the_table_of_the_gce_kind() {
    let cloud = cloud_of(&[]).unwrap();
    let expected = table(&format!(
        "project = \"my-project\"\n\
         zones = [\"us-central1-a\"]\n\
         network = \"global/networks/default\"\n\
         subnetwork = \"regions/us-central1/subnetworks/default\"\n\
         allowed_machine_types = [\"n1-standard-1\"]\n\
         [images]\n\
         x86_64 = \"{IMAGE}\"\n"
    ));
    assert_eq!(cloud.table(), expected);
}

#[test]
fn a_network_and_a_subnetwork_given_are_kept_and_the_default_subnetwork_is_the_zones_region() {
    let cloud = cloud_of(&[
        "--network",
        "global/networks/lab",
        "--subnetwork",
        "regions/us-central1/subnetworks/lab-a",
    ])
    .unwrap();
    let table = cloud.table();
    assert_eq!(table["network"].as_str(), Some("global/networks/lab"));
    assert_eq!(
        table["subnetwork"].as_str(),
        Some("regions/us-central1/subnetworks/lab-a")
    );

    let other = cloud_from(&[
        "--run-id",
        "r1",
        "--shape",
        "n1-standard-1",
        "--zone",
        "europe-west4-b",
        "--project",
        "my-project",
        "--image",
        IMAGE,
    ])
    .unwrap()
    .table();
    assert_eq!(
        other["subnetwork"].as_str(),
        Some("regions/europe-west4/subnetworks/default")
    );
    assert_eq!(other["zones"].as_array().unwrap().len(), 1);
    assert_eq!(other["zones"][0].as_str(), Some("europe-west4-b"));
}

#[test]
fn an_arm_image_is_keyed_by_its_architecture_and_only_arm_offers_are_launchable() {
    let cloud = cloud_of(&["--arch", "arm64"]).unwrap();
    let images = cloud.table()["images"].clone();
    assert_eq!(images, table(&format!("arm64 = \"{IMAGE}\"")).into());
    assert!(cloud.launchable(&offer("arm64")));
    assert!(!cloud.launchable(&offer("x86_64")));
}

#[test]
fn an_x86_image_is_the_default_and_only_x86_offers_are_launchable() {
    let cloud = cloud_of(&[]).unwrap();
    assert!(cloud.launchable(&offer("x86_64")));
    assert!(!cloud.launchable(&offer("arm64")));
    let explicit = cloud_of(&["--arch", "x86_64"]).unwrap();
    assert_eq!(explicit.table(), cloud.table());
}

#[test]
fn a_bare_machine_type_is_the_on_demand_offer_and_its_type_is_the_part_before_a_slash_or_plus() {
    assert_eq!(offer_id("n1-standard-1"), "n1-standard-1/on-demand");
    assert_eq!(offer_id("n1-standard-1/spot"), "n1-standard-1/spot");
    assert_eq!(type_of("n1-standard-1/on-demand"), "n1-standard-1");
    assert_eq!(
        type_of("a2-highgpu-1g+nvidia-tesla-a100/spot"),
        "a2-highgpu-1g"
    );
    assert_eq!(type_of("n1-standard-1"), "n1-standard-1");
    assert_eq!(type_of(""), "");
}

#[test]
fn a_shape_naming_an_offer_allows_only_its_machine_type() {
    let cloud = cloud_of(&["--shape", "n1-standard-1/spot"]);
    assert_eq!(cloud.err().unwrap(), "--shape is given twice");
    let cloud = cloud_from(&[
        "--run-id",
        "r1",
        "--shape",
        "g2-standard-4+nvidia-l4/on-demand",
        "--zone",
        "us-central1-a",
        "--project",
        "my-project",
        "--image",
        IMAGE,
    ])
    .unwrap();
    assert_eq!(
        cloud.table()["allowed_machine_types"][0].as_str(),
        Some("g2-standard-4")
    );
}

#[test]
fn flags_the_project_cannot_use_are_refused_with_the_flag_named() {
    assert_eq!(
        cloud_of(&["--arch", "riscv"]).err().unwrap(),
        "--arch must be x86_64 or arm64"
    );
    let with_region = cloud_from(&[
        "--run-id",
        "r1",
        "--shape",
        "n1-standard-1",
        "--region",
        "us-central1",
        "--project",
        "my-project",
        "--image",
        IMAGE,
    ]);
    assert_eq!(
        with_region.err().unwrap(),
        "--region is for EC2 and Lambda; Compute Engine takes --zone"
    );
    let no_project = cloud_from(&[
        "--run-id",
        "r1",
        "--shape",
        "n1-standard-1",
        "--zone",
        "us-central1-a",
        "--image",
        IMAGE,
    ]);
    assert_eq!(no_project.err().unwrap(), "--project is required");
    let no_image = cloud_from(&[
        "--run-id",
        "r1",
        "--shape",
        "n1-standard-1",
        "--zone",
        "us-central1-a",
        "--project",
        "my-project",
    ]);
    assert_eq!(no_image.err().unwrap(), "--image is required");
}

#[test]
fn a_capture_directory_that_cannot_be_made_is_refused_naming_it() {
    let dir = scratch("blocked");
    fs::write(dir.join("file"), b"x").unwrap();
    let under_file = dir.join("file").join("captures");
    let why = cloud_of(&["--capture-dir", under_file.to_str().unwrap()])
        .err()
        .unwrap();
    assert!(
        why.starts_with(&format!("--capture-dir {}: ", under_file.display())),
        "{why}"
    );
}

#[test]
fn a_capture_directory_is_made_when_it_is_missing() {
    let dir = scratch("made").join("a").join("b");
    cloud_of(&["--capture-dir", dir.to_str().unwrap()]).unwrap();
    assert!(dir.is_dir());
}

#[test]
fn the_kind_accepts_the_table_and_refuses_what_the_flags_could_not_check() {
    assert!(build(&cloud_of(&[]).unwrap(), true).is_ok());
    assert!(build(&cloud_of(&["--arch", "arm64"]).unwrap(), false).is_ok());

    let refused = build(&cloud_of(&["--network", "lab"]).unwrap(), true)
        .err()
        .unwrap();
    assert_eq!(refused.field, "network");

    let refused = build(
        &cloud_of(&["--subnetwork", "regions/asia-east1/subnetworks/x"]).unwrap(),
        true,
    )
    .err()
    .unwrap();
    assert_eq!(refused.field, "subnetwork");

    let bad_image = cloud_from(&[
        "--run-id",
        "r1",
        "--shape",
        "n1-standard-1",
        "--zone",
        "us-central1-a",
        "--project",
        "my-project",
        "--image",
        "debian-12",
    ])
    .unwrap();
    assert_eq!(
        build(&bad_image, true).err().unwrap().field,
        "images.x86_64"
    );

    let not_a_zone = cloud_from(&[
        "--run-id",
        "r1",
        "--shape",
        "n1-standard-1",
        "--zone",
        "uscentral1",
        "--project",
        "my-project",
        "--image",
        IMAGE,
    ])
    .unwrap();
    assert_eq!(build(&not_a_zone, true).err().unwrap().field, "zones");
}

#[test]
fn a_capture_file_is_named_by_number_operation_and_status() {
    assert_eq!(
        Recording::<Canned>::file_name(1, "GET-instances", 200),
        "001-GET-instances-200.body"
    );
    assert_eq!(
        Recording::<Canned>::file_name(42, "POST-a_b", 404),
        "042-POST-a_b-404.body"
    );
    assert_eq!(
        Recording::<Canned>::file_name(1234, "GET-x", 503),
        "1234-GET-x-503.body"
    );
}

#[test]
fn what_of_names_the_method_and_the_last_two_segments_of_an_api_path() {
    let list = request(
        Method::Get,
        "https://compute.googleapis.com/compute/v1/projects/p/zones/us-central1-a/instances?filter=x",
    );
    assert_eq!(
        what_of(&list).as_deref(),
        Some("GET-us-central1-a_instances")
    );
    let list = request(
        Method::Get,
        "https://compute.googleapis.com/compute/v1/projects/p/zones/z/instances",
    );
    assert_eq!(what_of(&list).as_deref(), Some("GET-z_instances"));

    let insert = request(
        Method::Post,
        "https://compute.googleapis.com/compute/v1/projects/p/zones/z/instances",
    );
    assert_eq!(what_of(&insert).as_deref(), Some("POST-z_instances"));

    let billing = request(
        Method::Get,
        "https://cloudbilling.googleapis.com/v1/services/6F81-5844-456A/skus?pageSize=5000",
    );
    assert_eq!(
        what_of(&billing).as_deref(),
        Some("GET-6F81-5844-456A_skus")
    );

    let short = request(Method::Delete, "https://compute.googleapis.com/one");
    assert_eq!(what_of(&short).as_deref(), Some("DELETE-one"));
    let bare = request(Method::Put, "https://compute.googleapis.com/");
    assert_eq!(what_of(&bare).as_deref(), Some("PUT-"));

    let odd = request(Method::Get, "https://compute.googleapis.com/a/b:c/d%20e");
    assert_eq!(what_of(&odd).as_deref(), Some("GET-b_c_d_20e"));
}

#[test]
fn only_https_requests_to_the_compute_and_billing_hosts_are_captured() {
    for url in [
        "http://compute.googleapis.com/compute/v1/x",
        "https://oauth2.googleapis.com/token",
        "https://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/token",
        "http://169.254.169.254/computeMetadata/v1/",
        "https://169.254.169.254/computeMetadata/v1/",
        "https://compute.googleapis.com.example.com/x",
        "https://example.com/compute.googleapis.com/x",
        "https://compute.googleapis.com",
        "compute.googleapis.com/x",
        "",
    ] {
        assert_eq!(what_of(&request(Method::Get, url)), None, "{url}");
    }
}

#[test]
fn the_recording_writes_each_api_answer_in_order_and_nothing_else() {
    let dir = scratch("recording");
    let recording = Recording::over(Canned::new(200, b"{}"), &dir).unwrap();
    let compute = "https://compute.googleapis.com/compute/v1/projects/p/zones/z/instances";
    assert_eq!(
        recording
            .send(request(Method::Get, compute))
            .unwrap()
            .status,
        200
    );
    let token = request(Method::Post, "https://oauth2.googleapis.com/token");
    assert_eq!(recording.send(token).unwrap().body, b"{}");
    let metadata = request(Method::Get, "http://169.254.169.254/computeMetadata/v1/");
    recording.send(metadata).unwrap();
    let billing = "https://cloudbilling.googleapis.com/v1/services/S/skus";
    recording.send(request(Method::Get, billing)).unwrap();
    assert_eq!(
        names_in(&dir),
        ["001-GET-z_instances-200.body", "002-GET-S_skus-200.body"]
    );
    for name in names_in(&dir) {
        assert_eq!(fs::read(dir.join(name)).unwrap(), b"{}");
    }
}

#[test]
fn a_failed_exchange_writes_nothing_and_a_file_that_cannot_be_written_does_not_stop_the_run() {
    let call = || {
        request(
            Method::Get,
            "https://compute.googleapis.com/compute/v1/projects/p/zones/z/instances",
        )
    };
    let dir = scratch("failing");
    let mut canned = Canned::new(200, b"x");
    canned.fail = true;
    let recording = Recording::over(canned, &dir).unwrap();
    assert_eq!(recording.send(call()).err(), Some(TransportError::Timeout));
    assert!(names_in(&dir).is_empty());

    let gone = scratch("vanished");
    let recording = Recording::over(Canned::new(500, b"boom"), &gone).unwrap();
    fs::remove_dir_all(&gone).unwrap();
    let answer = recording.send(call()).unwrap();
    assert_eq!((answer.status, answer.body.as_slice()), (500, &b"boom"[..]));
    assert!(!gone.exists());
}

/// The flags every entry-point case starts from: a run id, a location and a shape.
const BASE: &[&str] = &[
    "--run-id",
    "r1",
    "--zone",
    "us-central1-a",
    "--shape",
    "n1-standard-1",
    "--image",
    "projects/example-project/global/images/family/example",
];

/// The entry point run on `raw`: its exit status, what it printed for the run, and what it
/// printed as a usage error.
fn entry(raw: &[&str]) -> (i32, String, String) {
    let raw: Vec<String> = raw.iter().map(|arg| (*arg).to_owned()).collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let status = main_with(&raw, &mut out, &mut err);
    let text = |bytes: Vec<u8>| String::from_utf8(bytes).unwrap();
    (status, text(out), text(err))
}

/// The entry point `main` forwards to refuses missing and misplaced flags with exit status 2,
/// the message naming the flag, then the usage, and prints nothing for the run: no flag at all,
/// no shape, the provider's own required flag missing, and the other kind of location.
#[test]
fn the_entry_point_refuses_missing_flags_with_exit_2_and_the_usage() {
    let cases: Vec<(Vec<&str>, &str)> = vec![
        (vec![], "--run-id is required"),
        (without_shape(BASE), "--shape is required"),
        (BASE.to_vec(), "--project is required"),
        (
            vec![
                "--run-id",
                "r1",
                "--region",
                "us-central1",
                "--shape",
                "n1-standard-1",
                "--project",
                "example-project",
                "--image",
                "projects/example-project/global/images/family/example",
            ],
            "--region is for EC2 and Lambda; Compute Engine takes --zone",
        ),
    ];
    for (raw, message) in cases {
        assert_eq!(
            entry(&raw),
            (2, String::new(), format!("{message}\n{USAGE}\n")),
            "{raw:?}"
        );
    }
}

/// `BASE` without `--shape` and its value.
fn without_shape(base: &[&'static str]) -> Vec<&'static str> {
    let at = base.iter().position(|arg| *arg == "--shape").unwrap();
    [&base[..at], &base[at + 2..]].concat()
}

/// Flags the parser accepts but the kind refuses (a project name that is not a project id) reach the provider's
/// construction and end with exit status 1 and the cause, before any call to the cloud: the
/// entry point runs the driver, not only the parser.
#[test]
fn the_entry_point_runs_the_driver_and_exits_1_when_the_provider_cannot_be_built() {
    let raw = [BASE, &["--project", "Not_A_Project"][..]].concat();
    let (status, out, err) = entry(&raw);
    assert_eq!((status, err.as_str()), (1, ""), "{out}");
    assert!(out.starts_with("cannot build the provider: "), "{out}");
}
