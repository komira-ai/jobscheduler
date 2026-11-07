//! The helpers of the Lambda Cloud live suite (`tests/live/cloud.rs`), tested without a
//! network: what the flags make of the account, the table of the `lambdalabs` kind, the key
//! file reader, and the provider the table builds. Building a provider reads the key file and
//! sends nothing, and no test reaches the real cloud.

use std::fs;
use std::num::NonZeroUsize;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use js_elastic::SchedulerId;
use js_elastic::conformance::live::{LiveArgs, LiveCloud};
use js_provider_lambdalabs::SystemClock;
use js_provider_lambdalabs::kind::{self, Context, SecretFiles};
use toml::Table;

#[path = "live/cloud.rs"]
mod cloud;

use cloud::{USAGE, main_with};

use cloud::{EXTRA, KeyFiles, LambdaCloud};

/// A directory for one test, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("lambdalabs-live-cloud")
            .join(format!("{name}-{}", NEXT.fetch_add(1, Ordering::SeqCst)));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    /// Writes `bytes` to the file `name` with `mode` and returns its path.
    fn write(&self, name: &str, bytes: &[u8], mode: u32) -> PathBuf {
        let path = self.0.join(name);
        fs::write(&path, bytes).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    /// A key file holding a made-up key, readable by its owner alone.
    fn key_file(&self) -> PathBuf {
        self.write("key", b"made-up-key\n", 0o600)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn raw(flags: &[&str]) -> Vec<String> {
    flags.iter().map(|arg| (*arg).to_owned()).collect()
}

/// The account the arguments `flags` make, as `main_with` makes it.
fn cloud_from(flags: &[&str]) -> Result<LambdaCloud, String> {
    let raw = raw(flags);
    let args = LiveArgs::parse(&raw, EXTRA)?;
    LambdaCloud::from_args(&args, &raw)
}

/// The account of a run in `us-east-1` for `gpu_1x_a10` with the key file at `key`.
fn cloud_with_key(key: &Path) -> LambdaCloud {
    cloud_from(&[
        "--run-id",
        "r1",
        "--region",
        "us-east-1",
        "--shape",
        "gpu_1x_a10",
        "--key-file",
        key.to_str().unwrap(),
    ])
    .unwrap()
}

fn context(files: &dyn SecretFiles) -> Context<'_> {
    Context {
        scheduler: SchedulerId::new("live-r1").unwrap(),
        max_machines: NonZeroUsize::new(1).unwrap(),
        live: true,
        clock: Arc::new(SystemClock::new()),
        files,
    }
}

#[test]
fn the_flags_make_the_table_of_the_lambdalabs_kind() {
    let cloud = cloud_from(&[
        "--run-id",
        "r1",
        "--region",
        "us-east-1",
        "--shape",
        "gpu_1x_a10",
        "--key-file",
        "/keys/lambda",
    ])
    .unwrap();
    let expected: Table = "api_key_file = \"/keys/lambda\"\n\
                           regions = [\"us-east-1\"]\n\
                           types = [{ name = \"gpu_1x_a10\" }]\n"
        .parse()
        .unwrap();
    assert_eq!(cloud.table(), expected);
}

#[test]
fn flags_the_account_cannot_use_are_refused_with_the_flag_named() {
    let no_key = cloud_from(&[
        "--run-id",
        "r1",
        "--region",
        "us-east-1",
        "--shape",
        "gpu_1x_a10",
    ]);
    assert_eq!(no_key.err().unwrap(), "--key-file is required");

    let zone = cloud_from(&[
        "--run-id",
        "r1",
        "--zone",
        "us-central1-a",
        "--shape",
        "gpu_1x_a10",
        "--key-file",
        "/keys/lambda",
    ]);
    assert_eq!(
        zone.err().unwrap(),
        "--zone is for Compute Engine; Lambda takes --region"
    );

    let no_shape = cloud_from(&[
        "--run-id",
        "r1",
        "--region",
        "us-east-1",
        "--key-file",
        "/keys/lambda",
    ]);
    assert_eq!(no_shape.err().unwrap(), "--shape is required");
}

#[test]
fn the_key_file_reader_gives_the_bytes_of_an_owner_only_file() {
    let scratch = Scratch::new("reader");
    let path = scratch.key_file();
    assert_eq!(KeyFiles.read(&path).unwrap(), b"made-up-key\n");
}

#[test]
fn the_key_file_reader_refuses_a_file_others_can_read_or_one_that_is_missing_naming_the_path() {
    let scratch = Scratch::new("refused");
    let open = scratch.write("open", b"made-up-key\n", 0o644);
    let why = KeyFiles.read(&open).unwrap_err();
    assert!(why.contains(open.to_str().unwrap()), "{why}");
    assert!(!why.contains("made-up-key"), "{why}");

    let missing = scratch.0.join("missing");
    let why = KeyFiles.read(&missing).unwrap_err();
    assert!(why.contains(missing.to_str().unwrap()), "{why}");

    let why = KeyFiles.read(&scratch.0).unwrap_err();
    assert!(why.contains(scratch.0.to_str().unwrap()), "{why}");
}

#[test]
fn the_kind_builds_a_provider_from_the_table_and_a_key_file() {
    let scratch = Scratch::new("builds");
    let cloud = cloud_with_key(&scratch.key_file());
    assert!(kind::build(&cloud.table(), &context(&KeyFiles)).is_ok());

    let scheduler = SchedulerId::new("live-r1").unwrap();
    assert!(
        cloud
            .provider(&scheduler, NonZeroUsize::new(3).unwrap())
            .is_ok()
    );
}

#[test]
fn a_key_file_others_can_read_is_refused_when_the_provider_is_built() {
    let scratch = Scratch::new("open-key");
    let open = scratch.write("key", b"made-up-key\n", 0o640);
    let cloud = cloud_with_key(&open);
    let refused = kind::build(&cloud.table(), &context(&KeyFiles))
        .err()
        .unwrap();
    assert_eq!(refused.field, "api_key_file");
    let scheduler = SchedulerId::new("live-r1").unwrap();
    let why = cloud
        .provider(&scheduler, NonZeroUsize::new(3).unwrap())
        .err()
        .unwrap();
    assert!(why.starts_with("api_key_file: "), "{why}");
    assert!(!why.contains("made-up-key"), "{why}");
}

#[test]
fn an_empty_key_file_and_a_missing_one_are_refused_when_the_provider_is_built() {
    let scratch = Scratch::new("bad-key");
    let empty = scratch.write("empty", b"\n", 0o600);
    let refused = kind::build(&cloud_with_key(&empty).table(), &context(&KeyFiles))
        .err()
        .unwrap();
    assert_eq!(refused.field, "api_key_file");

    let missing = scratch.0.join("missing");
    let refused = kind::build(&cloud_with_key(&missing).table(), &context(&KeyFiles))
        .err()
        .unwrap();
    assert_eq!(refused.field, "api_key_file");
}

/// The flags every entry-point case starts from: a run id, a location and a shape.
const BASE: &[&str] = &[
    "--run-id",
    "r1",
    "--region",
    "us-east-1",
    "--shape",
    "gpu_1x_a10",
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
        (BASE.to_vec(), "--key-file is required"),
        (
            vec![
                "--run-id",
                "r1",
                "--zone",
                "us-east-1a",
                "--shape",
                "gpu_1x_a10",
                "--key-file",
                "key",
            ],
            "--zone is for Compute Engine; Lambda takes --region",
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

/// Flags the parser accepts but the kind refuses (a key file that does not exist) reach the provider's
/// construction and end with exit status 1 and the cause, before any call to the cloud: the
/// entry point runs the driver, not only the parser.
#[test]
fn the_entry_point_runs_the_driver_and_exits_1_when_the_provider_cannot_be_built() {
    let missing = env!("CARGO_TARGET_TMPDIR").to_owned() + "/live-cloud-no-such-key-file";
    let raw = [BASE, &["--key-file", missing.as_str()][..]].concat();
    let (status, out, err) = entry(&raw);
    assert_eq!((status, err.as_str()), (1, ""), "{out}");
    assert!(out.starts_with("cannot build the provider: "), "{out}");
}
