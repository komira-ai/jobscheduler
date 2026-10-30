//! `js-agent image-pull` against the in-process registry, with a stand-in for `crun`
//! (`/usr/bin/true`) so that no privilege is needed: what these tests see is everything the
//! verb decides itself (what it refuses and when, what it prints, what it leaves in the
//! store) and the way it drives the image store. What a real helper container unpacks is
//! `tests/pull_root`'s, which runs as root against a real `crun`.

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::ExitCode;

use js_agent::image::pull::{Host, run};
use js_contain::oci::OciConfig;
use js_contain::oci::image::published_root;
use js_oci::{Allow, Client, Digest};
use js_testkit::dir::TestDir;
use js_testkit::oci_registry::{Artifact, Fixture, REGISTRY};

use crate::config::config_json;

/// What one run of the verb did.
struct Ran {
    status: String,
    stdout: String,
    stderr: String,
}

fn exit_status(code: u8) -> String {
    format!("{:?}", ExitCode::from(code))
}

/// One test's directories: a scratch root the verb may use (`0711`, owned by this user,
/// below a directory every user can search) and, apart from it, the stand-in files.
struct Cell {
    scratch: TestDir,
    files: TestDir,
}

impl Cell {
    fn new(name: &str) -> Self {
        let scratch = TestDir::under("/tmp", name);
        let mode = std::fs::Permissions::from_mode(0o711);
        std::fs::set_permissions(scratch.path(), mode).expect("mode");
        let files = TestDir::under(env!("CARGO_TARGET_TMPDIR"), name);
        Self { scratch, files }
    }

    fn scratch(&self) -> &Path {
        self.scratch.path()
    }
}

/// A small image: two layers (the verb never looks inside them here) and an entrypoint.
fn image() -> Artifact {
    let layers = vec![b"layer zero ".repeat(40), b"layer one ".repeat(40)];
    let config = config_json(r#"{"Entrypoint":["/bin/task"]}"#, layers.len());
    Artifact::image(layers, &config)
}

/// The stand-in files a start of the backend reads: ids for the running user, tools that
/// look setuid, and `/usr/bin/true` for `crun`. Nothing here is root's.
fn standins(dir: &Path, config: &mut OciConfig) {
    let ids = dir.join("ids");
    std::fs::create_dir_all(&ids).expect("ids");
    let files = js_contain::subids::IdFiles::in_dir(&ids);
    let uid = rustix::process::getuid().as_raw();
    std::fs::write(&files.passwd, "root:x:0:0::/:/bin/sh\n").expect("passwd");
    std::fs::write(&files.subuid, format!("{uid}:100000:65536\n")).expect("subuid");
    std::fs::write(&files.subgid, format!("{uid}:200000:65536\n")).expect("subgid");
    let tools = dir.join("tools");
    std::fs::create_dir_all(&tools).expect("tools");
    for name in ["newuidmap", "newgidmap"] {
        let path = tools.join(name);
        std::fs::write(&path, b"#!/bin/true\n").expect("tool");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o4755)).expect("mode");
    }
    config.id_files = files;
    config.id_tool_path = Some(tools.into_os_string());
    config.check_search = false;
}

/// The verb run as `euid` over the runtime `crun`, with the fixture as its registry (every
/// entry of the allow-list reaches it as a private registry on loopback, as the fixture's
/// own client does) and the stand-ins of `cell`.
fn pull_with(fx: &Fixture, cell: &Cell, euid: u32, crun: &str, args: &[&str]) -> Ran {
    let client = |allow: Vec<Allow>| -> Client {
        fx.client_with(
            allow
                .into_iter()
                .map(|entry| entry.private().loopback())
                .collect(),
        )
    };
    let configure = |config: &mut OciConfig| {
        standins(cell.files.path(), config);
    };
    let host = Host {
        euid,
        client: &client,
        configure: Some(&configure),
    };
    let mut argv: Vec<OsString> = vec!["js-agent".into(), "image-pull".into()];
    argv.extend(args.iter().map(OsString::from));
    argv.extend(["--crun-bin", crun].map(OsString::from));
    let (mut out, mut err) = (Vec::new(), Vec::new());
    // The verb blocks its thread on a runtime of its own, while the fixture is served by
    // this test's: the thread leaves the runtime for the call, and another worker serves.
    let status = tokio::task::block_in_place(|| run(&argv, &mut out, &mut err, &host));
    Ran {
        status: format!("{status:?}"),
        stdout: String::from_utf8(out).expect("utf-8"),
        stderr: String::from_utf8(err).expect("utf-8"),
    }
}

fn pull(fx: &Fixture, cell: &Cell, euid: u32, args: &[&str]) -> Ran {
    pull_with(fx, cell, euid, "/usr/bin/true", args)
}

/// The flags of a pull of `reference` into `scratch`, with the allow-list entry for the
/// fixture's `team/` namespace.
fn flags(fx: &Fixture, scratch: &Path, reference: &str) -> Vec<String> {
    let ids = format!(
        "{}:{}",
        rustix::process::getuid().as_raw(),
        rustix::process::getgid().as_raw()
    );
    vec![
        reference.to_owned(),
        "--image-allow".to_owned(),
        format!("{}/team/", fx.at(REGISTRY)),
        "--scratch-root".to_owned(),
        scratch.display().to_string(),
        "--job-user".to_owned(),
        ids,
        "--cgroup-parent".to_owned(),
        "/js-image-pull".to_owned(),
    ]
}

fn run_flags(fx: &Fixture, cell: &Cell, reference: &str) -> Ran {
    let flags = flags(fx, cell.scratch(), reference);
    let args: Vec<&str> = flags.iter().map(String::as_str).collect();
    pull(fx, cell, 0, &args)
}

/// The reference of `artifact` in the fixture's `team/app` repository.
fn reference(fx: &Fixture, artifact: &Artifact) -> String {
    format!("{}/team/app@{}", fx.at(REGISTRY), artifact.digest)
}

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

fn hex(artifact: &Artifact) -> String {
    artifact.digest.hex().to_owned()
}

/// What the verb makes below the scratch root before it pulls anything: nothing, so a
/// refusal leaves the directory as it was.
fn assert_untouched(scratch: &Path, fx: &Fixture) {
    assert_eq!(names(scratch), Vec::<String>::new());
    assert_eq!(
        fx.requests(),
        Vec::<String>::new(),
        "the registry was asked"
    );
}

/// A pull publishes the image under its digest in the layout the runtime reads, says
/// so with the path and the digest, and leaves nothing else in the store: no stage and no
/// downloaded layers. Catches a store at another place than `published_root` names (the
/// runtime would not find the image), a stage or blobs left behind, and an output that
/// leaves the operator to guess the path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_digest_pinned_image_is_pulled_and_published_where_the_runtime_reads_it() {
    let fx = Fixture::start().await;
    let artifact = image();
    fx.publish(&artifact);
    let cell = Cell::new("pull-publish");
    let ran = run_flags(&fx, &cell, &reference(&fx, &artifact));
    assert_eq!(ran.status, exit_status(0), "{}", ran.stderr);
    let root = published_root(cell.scratch(), artifact.digest.as_str());
    assert_eq!(
        ran.stdout,
        format!(
            "image-pull: status=pulled digest={} root={}\n",
            artifact.digest,
            root.display()
        )
    );
    assert!(ran.stderr.is_empty(), "{}", ran.stderr);
    assert!(root.parent().expect("image directory").is_dir());
    assert_eq!(names(&cell.scratch().join("oci-images")), [hex(&artifact)]);
}

/// A digest that is already published is not pulled again: the verb says so, names the
/// same root, and never reaches the registry (none of the image is served here, so a pull
/// would fail). Catches a second run that fetches the layers again, one that fails on what
/// it has, and one that changes what it prints into something a script cannot tell from a
/// pull.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_image_already_published_is_a_no_op_that_says_so() {
    let fx = Fixture::start().await;
    let artifact = image();
    let cell = Cell::new("pull-noop");
    let root = published_root(cell.scratch(), artifact.digest.as_str());
    std::fs::create_dir_all(&root).expect("published");
    let ran = run_flags(&fx, &cell, &reference(&fx, &artifact));
    assert_eq!(ran.status, exit_status(0), "{}", ran.stderr);
    assert_eq!(
        ran.stdout,
        format!(
            "image-pull: status=already-published digest={} root={}\n",
            artifact.digest,
            root.display()
        )
    );
    assert_eq!(
        fx.requests(),
        Vec::<String>::new(),
        "the registry was asked"
    );
    assert_eq!(
        names(cell.scratch()),
        ["oci-images"],
        "the no-op made files"
    );
}

/// What is not a digest-pinned reference is refused as a usage error before anything is
/// made or asked: a tag, a tag beside a digest, no digest at all, a digest that is not
/// 64 lowercase hex digits of sha256, another algorithm. Catches a tag resolved to
/// whatever it points at today (the pin is the whole point) and a refusal that has
/// already created files or asked the registry.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reference_that_is_not_pinned_by_digest_is_refused_before_anything_is_made() {
    let fx = Fixture::start().await;
    let cell = Cell::new("pull-tags");
    let repo = format!("{}/team/app", fx.at(REGISTRY));
    let good = format!("sha256:{}", "a".repeat(64));
    let cases = [
        (format!("{repo}:latest"), "names no digest"),
        (format!("{repo}:v1@{good}"), "carries a tag"),
        (repo.clone(), "names no digest"),
        (format!("{repo}@sha256:abc"), "sha256"),
        (
            format!("{}/Team/app@{good}", fx.at(REGISTRY)),
            "not a well formed registry reference",
        ),
        (format!("{repo}@sha256:{}", "A".repeat(64)), "sha256"),
        (format!("{repo}@sha512:{}", "a".repeat(128)), "sha256"),
        (format!("{repo}@{}", good.to_uppercase()), "sha256"),
    ];
    for (reference, why) in cases {
        let ran = run_flags(&fx, &cell, &reference);
        assert_eq!(ran.status, exit_status(2), "{reference}: {}", ran.stderr);
        assert!(ran.stderr.contains(why), "{reference}: {}", ran.stderr);
        assert!(ran.stdout.is_empty(), "{reference}: {}", ran.stdout);
        assert_untouched(cell.scratch(), &fx);
    }
}

/// A registry or namespace that no `--image-allow` entry admits is refused before
/// anything is made or asked, whatever the digest, and the answer names the reference.
/// Catches an allow-list that is read and then not enforced for the verb (the runtime
/// would pull from any host a job names), and a refusal that comes only after the first
/// request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_registry_outside_the_allow_list_is_refused_before_anything_is_made() {
    let fx = Fixture::start().await;
    let artifact = image();
    fx.publish(&artifact);
    let cell = Cell::new("pull-allow");
    let elsewhere = [
        format!("other.test:{}/team/app@{}", fx.port, artifact.digest),
        format!("{}/other/app@{}", fx.at(REGISTRY), artifact.digest),
        format!("{}/teamwork/app@{}", fx.at(REGISTRY), artifact.digest),
    ];
    for reference in elsewhere {
        let ran = run_flags(&fx, &cell, &reference);
        assert_eq!(ran.status, exit_status(2), "{reference}: {}", ran.stderr);
        assert!(
            ran.stderr.contains("allow-list") && ran.stderr.contains(&reference),
            "{reference}: {}",
            ran.stderr
        );
        assert_untouched(cell.scratch(), &fx);
    }
}

/// The flags are the agent's own: an `--image-allow` entry is read as at start (a bare
/// host is refused, naming the flag), every flag the pull needs is required, a job user is
/// `UID:GID` and never root's, the cgroup parent is a path below the cgroup mount, and the
/// scratch root must be the `0711` directory the runtime would use. `--help` is not an
/// error. Each is a usage error that has made nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn flags_are_checked_as_the_agent_checks_them() {
    let fx = Fixture::start().await;
    let artifact = image();
    let cell = Cell::new("pull-flags");
    let good = flags(&fx, cell.scratch(), &reference(&fx, &artifact));
    let swap = |flag: &str, value: &str| -> Vec<String> {
        let mut flags = good.clone();
        let at = flags.iter().position(|f| f == flag).expect("flag") + 1;
        flags[at] = value.to_owned();
        flags
    };
    // Readable by others: a job could list the names of the runs.
    let open = TestDir::under("/tmp", "pull-flags-open");
    let mode = std::fs::Permissions::from_mode(0o755);
    std::fs::set_permissions(open.path(), mode).expect("mode");
    let cases: Vec<(Vec<String>, &str)> = vec![
        (swap("--image-allow", "ghcr.io"), "--image-allow"),
        (swap("--job-user", "agent"), "--job-user"),
        (swap("--job-user", "0:0"), "--job-user"),
        (swap("--job-user", "1000:0"), "--job-user"),
        (swap("--job-user", "0:1000"), "--job-user"),
        (swap("--job-user", "agent:1000"), "--job-user"),
        (swap("--job-user", "1000:agent"), "--job-user"),
        (swap("--job-user", "-1:1000"), "--job-user"),
        (swap("--cgroup-parent", "js-image-pull"), "--cgroup-parent"),
        (swap("--cgroup-parent", "/a/../b"), "--cgroup-parent"),
        (swap("--cgroup-parent", "/"), "--cgroup-parent"),
        (swap("--cgroup-parent", ""), "--cgroup-parent"),
        (swap("--cgroup-parent", "/a//b"), "--cgroup-parent"),
        (swap("--cgroup-parent", "/a/./b"), "--cgroup-parent"),
        (swap("--cgroup-parent", "/a/b/"), "--cgroup-parent"),
        (
            swap("--scratch-root", "/nonexistent-js-agent"),
            "--scratch-root",
        ),
        (
            swap("--scratch-root", &open.path().display().to_string()),
            "--scratch-root",
        ),
        (good[..1].to_vec(), "--image-allow"),
    ];
    for (args, flag) in cases {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let ran = pull(&fx, &cell, 0, &args);
        assert_eq!(ran.status, exit_status(2), "{args:?}: {}", ran.stderr);
        assert!(ran.stderr.contains(flag), "{args:?}: {}", ran.stderr);
    }
    assert_untouched(cell.scratch(), &fx);

    let ran = pull(&fx, &cell, 0, &["--help"]);
    assert_eq!(ran.status, exit_status(0), "{}", ran.stderr);
    assert!(ran.stdout.contains("image-pull"), "{}", ran.stdout);
    assert!(ran.stderr.is_empty(), "{}", ran.stderr);
}

/// Only root runs it: the helpers give files to the job's ids, which no other user can.
/// Every check that needs no privilege comes first, so a user who is not root learns of
/// a wrong flag before being told to become root, and the refusal has made nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_user_that_is_not_root_is_refused_before_anything_is_made() {
    let fx = Fixture::start().await;
    let artifact = image();
    fx.publish(&artifact);
    let cell = Cell::new("pull-not-root");
    let flags = flags(&fx, cell.scratch(), &reference(&fx, &artifact));
    let args: Vec<&str> = flags.iter().map(String::as_str).collect();
    let ran = pull(&fx, &cell, 1000, &args);
    assert_eq!(ran.status, exit_status(2), "{}", ran.stderr);
    assert!(ran.stderr.contains("root"), "{}", ran.stderr);
    assert_untouched(cell.scratch(), &fx);
}

/// A layer that does not hash to the digest the manifest gives, or that is cut short, fails
/// the pull (status 1, the registry's words on stderr) and leaves no published root, no
/// stage and no downloaded layer; a pull of the good image then succeeds. Catches a
/// publish before the layers are verified, and leftovers a retry would trip on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_layer_that_does_not_verify_leaves_no_published_root_and_no_staging() {
    let fx = Fixture::start().await;
    let artifact = image();
    fx.publish(&artifact);
    let cell = Cell::new("pull-mismatch");
    let layer = Digest::of(&artifact.layers[1]);
    let good = artifact.layers[1].clone();
    let bad = [
        ("different bytes", vec![7u8; good.len()]),
        ("cut short", good[..good.len() / 2].to_vec()),
    ];
    for (what, bytes) in bad {
        fx.publish_blob_as(&layer, &bytes);
        let ran = run_flags(&fx, &cell, &reference(&fx, &artifact));
        assert_eq!(ran.status, exit_status(1), "{what}: {}", ran.stderr);
        assert!(ran.stderr.contains("image-pull:"), "{what}: {}", ran.stderr);
        assert!(ran.stdout.is_empty(), "{what}: {}", ran.stdout);
        assert_eq!(
            names(&cell.scratch().join("oci-images")),
            Vec::<String>::new(),
            "{what}"
        );
    }
    fx.publish_blob_as(&layer, &good);
    let ran = run_flags(&fx, &cell, &reference(&fx, &artifact));
    assert_eq!(ran.status, exit_status(0), "{}", ran.stderr);
    assert_eq!(names(&cell.scratch().join("oci-images")), [hex(&artifact)]);
}

/// Two pulls into one scratch root do not run together: the second is refused (status 1)
/// while the first holds the lock, and goes through once it is released. Catches the
/// second start sweeping the first one's stage as a leftover of a dead run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_pull_is_refused_while_another_holds_the_store() {
    let fx = Fixture::start().await;
    let artifact = image();
    fx.publish(&artifact);
    let cell = Cell::new("pull-lock");
    let lock =
        std::fs::File::create(cell.scratch().join(".js-image-pull.lock")).expect("lock file");
    rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive).expect("held");
    let ran = run_flags(&fx, &cell, &reference(&fx, &artifact));
    assert_eq!(ran.status, exit_status(1), "{}", ran.stderr);
    assert!(ran.stderr.contains("another image-pull"), "{}", ran.stderr);
    assert_eq!(
        fx.requests(),
        Vec::<String>::new(),
        "the registry was asked"
    );
    // Unlocked, not just closed: another test's fork may hold a copy of the descriptor.
    rustix::fs::flock(&lock, rustix::fs::FlockOperation::Unlock).expect("released");
    drop(lock);
    let ran = run_flags(&fx, &cell, &reference(&fx, &artifact));
    assert_eq!(ran.status, exit_status(0), "{}", ran.stderr);
}

/// A failure of the runtime itself (here a `crun` that exits 1) is reported as a failed
/// pull, with the helper's words, and leaves no published root. Catches a failed unpack
/// reported as a pull.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_unpack_is_a_failed_pull_and_publishes_nothing() {
    let fx = Fixture::start().await;
    let artifact = image();
    fx.publish(&artifact);
    let cell = Cell::new("pull-unpack-fails");
    let flags = flags(&fx, cell.scratch(), &reference(&fx, &artifact));
    let args: Vec<&str> = flags.iter().map(String::as_str).collect();
    let ran = pull_with(&fx, &cell, 0, "/usr/bin/false", &args);
    assert_eq!(ran.status, exit_status(1), "{}", ran.stderr);
    assert!(ran.stderr.contains("crun run (helper"), "{}", ran.stderr);
    assert!(ran.stdout.is_empty(), "{}", ran.stdout);
    let left = names(&cell.scratch().join("oci-images"));
    assert!(
        left.iter().all(|name| !name.ends_with("-blobs")),
        "{left:?}"
    );
    assert!(!left.contains(&hex(&artifact)), "{left:?}");
}

/// The verb is reached from the agent's own entry point by its first argument, before any
/// flag of the agent is read (`--server` is required of the agent, and not of this):
/// a refusal comes back from `main_with` with the verb's status and words.
#[test]
fn the_agents_entry_point_runs_the_verb_by_its_first_argument() {
    let args: Vec<OsString> = ["js-agent", "image-pull", "registry.test/team/app:latest"]
        .iter()
        .map(OsString::from)
        .collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let install = || -> std::io::Result<js_agent::SystemSleeper> {
        Err(std::io::Error::other("the verb installs no signal handler"))
    };
    let status = js_agent::main_with(&install, args, &mut out, &mut err);
    let err = String::from_utf8(err).expect("utf-8");
    // clap answers first: the required flags are missing.
    assert_eq!(format!("{status:?}"), exit_status(2), "{err}");
    assert!(err.contains("--image-allow"), "{err}");
    assert!(!err.contains("signal handler"), "{err}");
}

/// A lock file that cannot be opened (here a directory stands in its place) fails the pull
/// with the file named, before the registry is asked. Catches a pull that goes on without
/// the lock.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lock_file_that_cannot_be_opened_fails_the_pull_before_the_registry_is_asked() {
    let fx = Fixture::start().await;
    let artifact = image();
    fx.publish(&artifact);
    let cell = Cell::new("pull-lock-open");
    std::fs::create_dir(cell.scratch().join(".js-image-pull.lock")).expect("directory");
    let ran = run_flags(&fx, &cell, &reference(&fx, &artifact));
    assert_eq!(ran.status, exit_status(1), "{}", ran.stderr);
    assert!(ran.stderr.contains(".js-image-pull.lock"), "{}", ran.stderr);
    assert!(ran.stdout.is_empty(), "{}", ran.stdout);
    assert_eq!(
        fx.requests(),
        Vec::<String>::new(),
        "the registry was asked"
    );
}

/// A runtime that is not there (the path given by `--crun-bin` names no file) fails the
/// pull, status 1 and the path on stderr, with nothing published and no stage made, and the
/// registry not asked. Catches a runtime check that is skipped, and a failure to set up the
/// store reported as a usage error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_runtime_that_is_missing_fails_the_pull_before_the_registry_is_asked() {
    let fx = Fixture::start().await;
    let artifact = image();
    fx.publish(&artifact);
    let cell = Cell::new("pull-no-crun");
    let flags = flags(&fx, cell.scratch(), &reference(&fx, &artifact));
    let args: Vec<&str> = flags.iter().map(String::as_str).collect();
    let missing = cell.files.path().join("no-such-crun");
    let ran = pull_with(&fx, &cell, 0, &missing.display().to_string(), &args);
    assert_eq!(ran.status, exit_status(1), "{}", ran.stderr);
    assert!(ran.stderr.contains("no-such-crun"), "{}", ran.stderr);
    assert!(ran.stdout.is_empty(), "{}", ran.stdout);
    assert!(
        !published_root(cell.scratch(), artifact.digest.as_str()).exists(),
        "published"
    );
    assert_eq!(
        fx.requests(),
        Vec::<String>::new(),
        "the registry was asked"
    );
}

/// The agent's own entry point judges the allow-list with the machine's registry client:
/// an image on no entry is refused as a usage error, naming the reference, with nothing
/// made or asked (no registry is reachable here, so a request would fail differently).
#[test]
fn the_agents_entry_point_refuses_a_registry_outside_the_allow_list() {
    let cell = Cell::new("pull-entry-allow");
    let reference = format!("elsewhere.test/team/app@sha256:{}", "a".repeat(64));
    let args: Vec<OsString> = [
        "js-agent",
        "image-pull",
        reference.as_str(),
        "--image-allow",
        "registry.test/team/",
        "--scratch-root",
        &cell.scratch().display().to_string(),
        "--job-user",
        "1000:1000",
        "--cgroup-parent",
        "/js-image-pull",
    ]
    .iter()
    .map(OsString::from)
    .collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let install = || -> std::io::Result<js_agent::SystemSleeper> {
        Err(std::io::Error::other("the verb installs no signal handler"))
    };
    let status = js_agent::main_with(&install, args, &mut out, &mut err);
    let err = String::from_utf8(err).expect("utf-8");
    assert_eq!(format!("{status:?}"), exit_status(2), "{err}");
    assert!(
        err.contains("allow-list") && err.contains(&reference),
        "{err}"
    );
    assert_eq!(names(cell.scratch()), Vec::<String>::new());
}
