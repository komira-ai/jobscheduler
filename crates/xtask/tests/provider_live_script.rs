//! The held script `tools/held/provider-live/provider_live.sh`, run by `bash` with a stand-in
//! `cargo` first on `PATH`: the stand-in writes the arguments it was given, one per line, and
//! the `GOOGLE_APPLICATION_CREDENTIALS` it saw, then exits with the status it is told. So
//! each test pins what the script would run (the live suite of which provider, with which
//! flags), that a dry run is the default and a spend needs `--spend` with a plan hash, and
//! what it refuses (exit 2, with the stand-in never run). Nothing reaches a cloud: the real
//! `cargo` is never started.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use js_testkit::spawn::install_script;

/// A plan hash of the right form.
const HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

const IMAGE: &str = "ami-0123456789abcdef0";

/// The repository root.
fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

/// A scratch directory of its own for each test (outside the checkout), holding the stand-in
/// `cargo` in `bin/`.
struct Rig {
    dir: PathBuf,
    /// Removes the directory when the rig is dropped.
    _scratch: js_testkit::dir::TestDir,
}

/// What one run of the script did.
#[derive(Debug)]
struct Ran {
    status: i32,
    stdout: String,
    stderr: String,
    /// The arguments the stand-in `cargo` got, or `None` when it was not run.
    cargo: Option<Vec<String>>,
    /// The `GOOGLE_APPLICATION_CREDENTIALS` the stand-in saw (`unset` when it was not set).
    credentials: Option<String>,
}

impl Rig {
    fn new(name: &str) -> Rig {
        let scratch = js_testkit::dir::TestDir::under(
            env!("CARGO_TARGET_TMPDIR"),
            &format!("provider-live-{name}"),
        );
        let dir = scratch.path().to_path_buf();
        fs::create_dir_all(dir.join("bin")).unwrap();
        let cargo = dir.join("bin/cargo");
        install_script(
            &cargo,
            "#!/bin/sh\n\
             printf '%s\\n' \"$@\" > \"$FAKE_LOG\"\n\
             printf '%s\\n' \"${GOOGLE_APPLICATION_CREDENTIALS-unset}\" > \"$FAKE_LOG.env\"\n\
             exit \"${FAKE_STATUS:-0}\"\n",
            "755",
        );
        Rig {
            dir,
            _scratch: scratch,
        }
    }

    /// A file in the scratch directory, made readable.
    fn file(&self, name: &str) -> PathBuf {
        let path = self.dir.join(name);
        fs::write(&path, "placeholder\n").unwrap();
        path
    }

    /// Runs the script from the scratch directory with `args`; the stand-in exits `status`.
    fn run_with(&self, args: &[&str], status: i32) -> Ran {
        let log = self.dir.join("cargo.log");
        let _ = fs::remove_file(&log);
        let _ = fs::remove_file(self.dir.join("cargo.log.env"));
        let path = format!(
            "{}:{}",
            self.dir.join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let out = Command::new("bash")
            .arg(root().join("tools/held/provider-live/provider_live.sh"))
            .args(args)
            .current_dir(&self.dir)
            .env("PATH", path)
            .env("FAKE_LOG", &log)
            .env("FAKE_STATUS", status.to_string())
            .env_remove("GOOGLE_APPLICATION_CREDENTIALS")
            .output()
            .unwrap();
        let lines = |path: &Path| {
            fs::read_to_string(path)
                .ok()
                .map(|text| text.lines().map(str::to_owned).collect::<Vec<_>>())
        };
        Ran {
            status: out.status.code().unwrap(),
            stdout: String::from_utf8(out.stdout).unwrap(),
            stderr: String::from_utf8(out.stderr).unwrap(),
            cargo: lines(&log),
            credentials: lines(&self.dir.join("cargo.log.env")).map(|l| l.join("\n")),
        }
    }

    fn run(&self, args: &[&str]) -> Ran {
        self.run_with(args, 0)
    }
}

/// The cargo command line the script runs for `provider`, then `flags`.
fn suite(provider: &str, flags: &[&str]) -> Vec<String> {
    let package = format!("js-provider-{provider}");
    let head = [
        "test",
        "--locked",
        "-p",
        package.as_str(),
        "--features",
        "live",
        "--test",
        "live",
        "--",
    ];
    head.iter().chain(flags).map(|s| (*s).to_owned()).collect()
}

/// The flags of an EC2 run, and `more` besides.
fn ec2<'a>(more: &[&'a str]) -> Vec<&'a str> {
    let base = [
        "--provider",
        "ec2",
        "--run-id",
        "r1",
        "--location",
        "us-east-1",
        "--shape",
        "t3.micro",
        "--image",
        IMAGE,
    ];
    [&base[..], more].concat()
}

/// With no `--spend` the script is a dry run: it says so and runs the EC2 suite with the
/// location as `--region`, the flags given, empty flags dropped, and no `--spend`.
#[test]
fn a_run_without_spend_is_a_dry_run_of_the_providers_suite() {
    let rig = Rig::new("dry");
    let ran = rig.run(&ec2(&["--project", "", "--arch", "x86_64"]));
    assert_eq!(ran.status, 0, "{ran:?}");
    assert!(
        ran.stdout.starts_with("DRY RUN: nothing is launched\n"),
        "{ran:?}"
    );
    let expected = suite(
        "ec2",
        &[
            "--run-id",
            "r1",
            "--region",
            "us-east-1",
            "--shape",
            "t3.micro",
            "--image",
            IMAGE,
            "--arch",
            "x86_64",
        ],
    );
    assert_eq!(ran.cargo.as_deref(), Some(&expected[..]));
    assert!(!ran.cargo.unwrap().contains(&"--spend".to_owned()));
    assert_eq!(ran.credentials.as_deref(), Some("unset"));
}

/// `--spend --plan-hash H` is a spend: it says so and passes both to the suite, last.
#[test]
fn spend_with_a_plan_hash_forwards_both_to_the_suite() {
    let rig = Rig::new("spend");
    let ran = rig.run(&ec2(&["--spend", "--plan-hash", HASH]));
    assert_eq!(ran.status, 0, "{ran:?}");
    assert!(ran.stdout.starts_with("SPEND: "), "{ran:?}");
    let expected = suite(
        "ec2",
        &[
            "--run-id",
            "r1",
            "--region",
            "us-east-1",
            "--shape",
            "t3.micro",
            "--image",
            IMAGE,
            "--spend",
            "--plan-hash",
            HASH,
        ],
    );
    assert_eq!(ran.cargo.as_deref(), Some(&expected[..]));
}

/// For Compute Engine the location is `--zone` and the credential file, given by a relative
/// path, is exported as `GOOGLE_APPLICATION_CREDENTIALS` by its absolute path and is not a
/// flag of the suite; a capture directory outside the checkout is passed on. The script only
/// names the directory (the suite makes it), so nothing is written there.
#[test]
fn gce_gets_a_zone_and_the_credential_file_through_its_variable() {
    let rig = Rig::new("gce");
    let key = rig.file("key.json");
    // Outside the checkout (the scratch directory is under its `target/`), and per process.
    let answers =
        std::env::temp_dir().join(format!("provider-live-answers-{}", std::process::id()));
    let ran = rig.run(&[
        "--provider",
        "gce",
        "--run-id",
        "r1",
        "--location",
        "us-central1-a",
        "--shape",
        "n1-standard-1",
        "--project",
        "example-project",
        "--image",
        "projects/example-project/global/images/family/example",
        "--credential-file",
        "key.json",
        "--capture-dir",
        answers.to_str().unwrap(),
    ]);
    assert_eq!(ran.status, 0, "{ran:?}");
    let expected = suite(
        "gce",
        &[
            "--run-id",
            "r1",
            "--zone",
            "us-central1-a",
            "--shape",
            "n1-standard-1",
            "--image",
            "projects/example-project/global/images/family/example",
            "--project",
            "example-project",
            "--capture-dir",
            answers.to_str().unwrap(),
        ],
    );
    assert_eq!(ran.cargo.as_deref(), Some(&expected[..]));
    assert_eq!(ran.credentials.as_deref(), key.to_str());
}

/// For Lambda the credential file, given by a relative path, becomes `--key-file` with its
/// absolute path, and no variable is set.
#[test]
fn lambdalabs_gets_the_credential_file_as_its_key_file() {
    let rig = Rig::new("lambdalabs");
    let key = rig.file("api-key");
    let ran = rig.run(&[
        "--provider",
        "lambdalabs",
        "--run-id",
        "r1",
        "--location",
        "us-east-1",
        "--shape",
        "gpu_1x_a10",
        "--image",
        "",
        "--credential-file",
        "api-key",
    ]);
    assert_eq!(ran.status, 0, "{ran:?}");
    let expected = suite(
        "lambdalabs",
        &[
            "--run-id",
            "r1",
            "--region",
            "us-east-1",
            "--shape",
            "gpu_1x_a10",
            "--key-file",
            key.to_str().unwrap(),
        ],
    );
    assert_eq!(ran.cargo.as_deref(), Some(&expected[..]));
    assert_eq!(ran.credentials.as_deref(), Some("unset"));
}

/// The suite's exit status is the script's: it runs the suite in its place.
#[test]
fn the_suites_exit_status_is_the_scripts() {
    let rig = Rig::new("status");
    assert_eq!(rig.run_with(&ec2(&[]), 1).status, 1);
}

/// What the script refuses: each is exit status 2 with the reason and the usage, and the
/// suite is never run.
#[test]
fn refusals_exit_2_and_never_run_the_suite() {
    let rig = Rig::new("refusals");
    let key = rig.file("key");
    let key = key.to_str().unwrap();
    let inside = root().join("captures");
    let inside = inside.to_str().unwrap();
    let cases: Vec<(Vec<&str>, &str)> = vec![
        (ec2(&["--spend"]), "--spend"),
        (ec2(&["--plan-hash", HASH]), "--plan-hash"),
        (
            ec2(&["--spend", "--plan-hash", "not-a-hash"]),
            "--plan-hash",
        ),
        (ec2(&["--force"]), "--force"),
        (ec2(&["--project", "example-project"]), "--project"),
        (ec2(&["--credential-file", key]), "--credential-file"),
        (ec2(&["--capture-dir", inside]), "inside the repository"),
        (
            vec![
                "--provider",
                "ec2",
                "--run-id",
                "r1",
                "--location",
                "us-east-1",
                "--image",
                IMAGE,
            ],
            "--shape is required",
        ),
        (
            vec![
                "--provider",
                "ec2",
                "--run-id",
                "Not_An_Id",
                "--location",
                "us-east-1",
                "--shape",
                "t3.micro",
                "--image",
                IMAGE,
            ],
            "--run-id is 1 to 40 characters",
        ),
        (
            vec![
                "--provider",
                "azure",
                "--run-id",
                "r1",
                "--location",
                "us-east-1",
                "--shape",
                "t3.micro",
            ],
            "provider",
        ),
        (
            vec![
                "--provider",
                "lambdalabs",
                "--run-id",
                "r1",
                "--location",
                "us-east-1",
                "--shape",
                "gpu_1x_a10",
            ],
            "--credential-file is required",
        ),
        (vec![], "usage"),
    ];
    for (args, reason) in cases {
        let ran = rig.run(&args);
        assert_eq!(ran.status, 2, "{args:?}: {ran:?}");
        assert!(ran.stderr.contains(reason), "{args:?}: {ran:?}");
        assert!(
            ran.stderr.contains("usage: provider_live.sh"),
            "{args:?}: {ran:?}"
        );
        assert_eq!(ran.cargo, None, "{args:?}: the suite ran");
    }
}
