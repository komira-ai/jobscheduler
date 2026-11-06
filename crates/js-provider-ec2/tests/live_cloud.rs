//! The helpers of the EC2 live suite (`tests/live/cloud.rs`), tested without a network: what
//! the flags make of the account, the table of the `ec2` kind, the provider that table builds,
//! the names of the files the recording transport writes, and the recording itself over a
//! canned transport. Nothing here sends a request, and no test reaches the real cloud.

use std::fs;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use js_elastic::conformance::live::{LiveArgs, LiveCloud};
use js_elastic::{Offer, PriceUnit, SchedulerId};
use js_logstore::{HttpRequest, HttpResponse, Method, Transport, TransportError};
use js_provider_ec2::SystemClock;
use js_provider_ec2::kind::{self, Context};
use js_types::ExecMode;
use toml::Table;

#[path = "live/cloud.rs"]
mod cloud;

use cloud::{USAGE, main_with};

use cloud::{EXTRA, Ec2Cloud, Recording, is_aws_api, offer_id, type_of, what_of};

const IMAGE: &str = "ami-0123456789abcdef0";

/// The account the arguments `raw` make, as `main_with` makes it.
fn cloud_from(raw: &[&str]) -> Result<Ec2Cloud, String> {
    let raw: Vec<String> = raw.iter().map(|arg| (*arg).to_owned()).collect();
    let mut args = LiveArgs::parse(&raw, EXTRA)?;
    args.shape = offer_id(&args.shape);
    Ec2Cloud::from_args(&args, &raw)
}

/// The account of a run with a shape and an image, and `more` besides.
fn cloud_of(more: &[&str]) -> Result<Ec2Cloud, String> {
    let base = ["--run-id", "r1", "--shape", "t3.micro", "--image", IMAGE];
    cloud_from(&[&base[..], more].concat())
}

fn table(text: &str) -> Table {
    text.parse().unwrap()
}

/// A directory for one test, empty, under the test target's scratch directory.
fn scratch(name: &str) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("ec2-live-cloud")
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
        clock: std::sync::Arc::new(SystemClock::new()),
    }
}

fn offer(arch: &str) -> Offer {
    Offer {
        id: "t3.micro/on-demand".to_owned(),
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

fn request(url: &str, body: &str, headers: &[(&str, &str)]) -> HttpRequest {
    HttpRequest {
        method: Method::Post,
        url: url.to_owned(),
        headers: headers
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect(),
        body: body.as_bytes().to_vec(),
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
fn the_flags_make_the_table_of_the_ec2_kind() {
    let cloud = cloud_of(&["--region", "us-east-1"]).unwrap();
    let expected = table(&format!(
        "region = \"us-east-1\"\n\
         allowed_instance_types = [\"t3.micro\"]\n\
         [images]\n\
         x86_64 = \"{IMAGE}\"\n"
    ));
    assert_eq!(cloud.table(), expected);
}

#[test]
fn an_arm_image_is_keyed_by_its_architecture_and_only_arm_offers_are_launchable() {
    let cloud = cloud_of(&["--region", "eu-west-2", "--arch", "arm64"]).unwrap();
    let expected = table(&format!(
        "region = \"eu-west-2\"\n\
         allowed_instance_types = [\"t3.micro\"]\n\
         [images]\n\
         arm64 = \"{IMAGE}\"\n"
    ));
    assert_eq!(cloud.table(), expected);
    assert!(cloud.launchable(&offer("arm64")));
    assert!(!cloud.launchable(&offer("x86_64")));
}

#[test]
fn an_x86_image_is_the_default_and_only_x86_offers_are_launchable() {
    let cloud = cloud_of(&["--region", "us-east-1"]).unwrap();
    assert!(cloud.launchable(&offer("x86_64")));
    assert!(!cloud.launchable(&offer("arm64")));
    let explicit = cloud_of(&["--region", "us-east-1", "--arch", "x86_64"]).unwrap();
    assert_eq!(explicit.table(), cloud.table());
}

#[test]
fn a_shape_naming_an_offer_allows_only_its_instance_type() {
    let cloud = cloud_from(&[
        "--run-id",
        "r1",
        "--region",
        "us-east-1",
        "--image",
        IMAGE,
        "--shape",
        "m7g.large/spot",
    ])
    .unwrap();
    assert_eq!(
        cloud.table()["allowed_instance_types"],
        toml::Value::Array(vec![toml::Value::String("m7g.large".to_owned())])
    );
}

#[test]
fn a_bare_instance_type_is_the_on_demand_offer_and_an_offer_id_is_kept() {
    assert_eq!(offer_id("t3.micro"), "t3.micro/on-demand");
    assert_eq!(offer_id("t3.micro/spot"), "t3.micro/spot");
    assert_eq!(type_of("t3.micro/on-demand"), "t3.micro");
    assert_eq!(type_of("t3.micro"), "t3.micro");
    assert_eq!(type_of("m5.large/spot/x"), "m5.large");
    assert_eq!(type_of(""), "");
}

#[test]
fn flags_the_account_cannot_use_are_refused_with_the_flag_named() {
    for (more, why) in [
        (
            vec!["--region", "us-east-1", "--arch", "riscv"],
            "--arch must be x86_64 or arm64",
        ),
        (
            vec!["--zone", "us-east-1a"],
            "--zone is for Compute Engine; EC2 takes --region",
        ),
    ] {
        assert_eq!(cloud_of(&more).err().unwrap(), why, "{more:?}");
    }
    let no_image = cloud_from(&["--run-id", "r1", "--shape", "t3.micro", "--region", "x"]);
    assert_eq!(no_image.err().unwrap(), "--image is required");
}

#[test]
fn a_capture_directory_that_cannot_be_made_is_refused_naming_it() {
    let dir = scratch("blocked");
    let file = dir.join("file");
    fs::write(&file, b"x").unwrap();
    let under_file = dir.join("file").join("captures");
    let why = cloud_of(&[
        "--region",
        "us-east-1",
        "--capture-dir",
        under_file.to_str().unwrap(),
    ])
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
    cloud_of(&[
        "--region",
        "us-east-1",
        "--capture-dir",
        dir.to_str().unwrap(),
    ])
    .unwrap();
    assert!(dir.is_dir());
}

#[test]
fn the_kind_builds_a_provider_from_the_table_and_refuses_a_bad_region_or_image() {
    let cloud = cloud_of(&["--region", "us-east-1"]).unwrap();
    assert!(kind::build(&cloud.table(), &context(true)).is_ok());
    assert!(kind::build(&cloud.table(), &context(false)).is_ok());

    let bad_region = cloud_of(&["--region", "Moon Base"]).unwrap();
    let refused = kind::build(&bad_region.table(), &context(true))
        .err()
        .unwrap();
    assert_eq!(refused.field, "region");

    let bad_image = cloud_from(&[
        "--run-id",
        "r1",
        "--shape",
        "t3.micro",
        "--region",
        "us-east-1",
        "--image",
        "not-an-image",
    ])
    .unwrap();
    let refused = kind::build(&bad_image.table(), &context(true))
        .err()
        .unwrap();
    assert_eq!(refused.field, "images.x86_64");
}

#[test]
fn the_account_builds_its_provider_with_and_without_a_capture_directory() {
    let scheduler = SchedulerId::new("live-r1").unwrap();
    let machines = NonZeroUsize::new(3).unwrap();
    let plain = cloud_of(&["--region", "us-east-1"]).unwrap();
    assert!(plain.provider(&scheduler, machines).is_ok());

    let dir = scratch("provider");
    let capturing = cloud_of(&[
        "--region",
        "us-east-1",
        "--capture-dir",
        dir.to_str().unwrap(),
    ])
    .unwrap();
    assert!(capturing.provider(&scheduler, machines).is_ok());
    assert!(names_in(&dir).is_empty(), "building sends nothing");

    let bad_region = cloud_of(&[
        "--region",
        "Moon Base",
        "--capture-dir",
        dir.to_str().unwrap(),
    ])
    .unwrap();
    let why = bad_region.provider(&scheduler, machines).err().unwrap();
    assert!(why.starts_with("region: "), "{why}");
}

#[test]
fn a_capture_file_is_named_by_number_operation_and_status() {
    assert_eq!(
        Recording::<Canned>::file_name(1, "DescribeInstances", 200),
        "001-DescribeInstances-200.body"
    );
    assert_eq!(
        Recording::<Canned>::file_name(42, "RunInstances_x", 400),
        "042-RunInstances_x-400.body"
    );
    assert_eq!(
        Recording::<Canned>::file_name(1234, "call", 503),
        "1234-call-503.body"
    );
}

#[test]
fn what_of_reads_the_form_action_then_the_target_header_then_says_call() {
    let url = "https://ec2.us-east-1.amazonaws.com/";
    let form = request(url, "Version=2016-11-15&Action=DescribeInstances&x=1", &[]);
    assert_eq!(what_of(&form), "DescribeInstances");

    let target = request(
        "https://servicequotas.us-east-1.amazonaws.com/",
        "{}",
        &[("x-amz-target", "ServiceQuotasV20190624.ListServiceQuotas")],
    );
    assert_eq!(what_of(&target), "ListServiceQuotas");

    let bare_target = request(url, "", &[("x-amz-target", "Operation")]);
    assert_eq!(what_of(&bare_target), "Operation");

    let neither = request(url, "Version=2016-11-15", &[]);
    assert_eq!(what_of(&neither), "call");

    let both = request(url, "Action=FromForm", &[("x-amz-target", "S.FromTarget")]);
    assert_eq!(what_of(&both), "FromForm");

    let odd = request(url, "Action=../../etc/passwd", &[]);
    assert_eq!(what_of(&odd), "______etc_passwd");
}

#[test]
fn only_https_requests_to_an_aws_api_host_are_captured() {
    for url in [
        "https://ec2.us-east-1.amazonaws.com/",
        "https://api.pricing.us-east-1.amazonaws.com/path?x=1",
        "https://servicequotas.eu-west-2.amazonaws.com",
    ] {
        assert!(is_aws_api(url), "{url}");
    }
    for url in [
        "http://ec2.us-east-1.amazonaws.com/",
        "http://169.254.169.254/latest/api/token",
        "https://169.254.169.254/latest/meta-data/",
        "https://example.com/",
        "https://amazonaws.com.example.com/",
        "https://example.com/.amazonaws.com",
        "https://example.com/?host=x.amazonaws.com",
        "ec2.us-east-1.amazonaws.com",
        "",
    ] {
        assert!(!is_aws_api(url), "{url}");
    }
}

#[test]
fn the_recording_writes_each_aws_answer_in_order_and_nothing_else() {
    let dir = scratch("recording");
    let recording = Recording::over(Canned::new(200, b"<ok/>"), &dir).unwrap();

    let first = request(
        "https://ec2.us-east-1.amazonaws.com/",
        "Action=DescribeInstances",
        &[("authorization", "secret-signature")],
    );
    assert_eq!(recording.send(first).unwrap().status, 200);
    let metadata = request("http://169.254.169.254/latest/meta-data/iam/", "", &[]);
    assert_eq!(recording.send(metadata).unwrap().body, b"<ok/>");
    let second = request(
        "https://api.pricing.us-east-1.amazonaws.com/",
        "",
        &[("x-amz-target", "AWSPriceListService.GetProducts")],
    );
    recording.send(second).unwrap();

    assert_eq!(
        names_in(&dir),
        ["001-DescribeInstances-200.body", "002-GetProducts-200.body"]
    );
    for name in names_in(&dir) {
        assert_eq!(fs::read(dir.join(name)).unwrap(), b"<ok/>");
    }
}

#[test]
fn a_failed_exchange_writes_nothing_and_a_file_that_cannot_be_written_does_not_stop_the_run() {
    let dir = scratch("failing");
    let mut canned = Canned::new(200, b"x");
    canned.fail = true;
    let recording = Recording::over(canned, &dir).unwrap();
    let call = || request("https://ec2.us-east-1.amazonaws.com/", "Action=A", &[]);
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
    "--region",
    "us-east-1",
    "--shape",
    "t3.micro",
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
        (BASE.to_vec(), "--image is required"),
        (
            vec![
                "--run-id",
                "r1",
                "--zone",
                "us-east-1a",
                "--shape",
                "t3.micro",
                "--image",
                "ami-0123456789abcdef0",
            ],
            "--zone is for Compute Engine; EC2 takes --region",
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

/// Flags the parser accepts but the kind refuses (an image that is not a machine image id) reach the provider's
/// construction and end with exit status 1 and the cause, before any call to the cloud: the
/// entry point runs the driver, not only the parser.
#[test]
fn the_entry_point_runs_the_driver_and_exits_1_when_the_provider_cannot_be_built() {
    let raw = [BASE, &["--image", "not-an-image"][..]].concat();
    let (status, out, err) = entry(&raw);
    assert_eq!((status, err.as_str()), (1, ""), "{out}");
    assert!(out.starts_with("cannot build the provider: "), "{out}");
}
