//! `js-agent image-pull` as root against a real `crun`: the image is pulled from the
//! in-process registry, unpacked by the agent's own binary in a helper container that
//! runs as the job user, and published read-only in the store the runtime reads.
//! `#[ignore]`d: it needs root, `crun`, `uidmap`, a job user with a subordinate id range
//! and a cgroup v2 mount. The `agent-privilege` job of `.github/workflows/ci.yml` prepares
//! them and runs this file under `sudo`; the variables it sets are `JS_B_CRUN` (the
//! distribution's `/usr/bin/crun`, whose path is what AppArmor's profile attaches to) and
//! `JS_B_UID` and `JS_B_GID` (the job user, an account with a range in `/etc/subuid` and
//! `/etc/subgid`). A missing variable or a failed step is a failure, never a skip.
//!
//! The helper container runs `js-agent` itself, the binary this package builds, as the
//! daemon of the backend; the verb is the library's `run`, with the registry client
//! swapped for the fixture's (the production client speaks https to the registries
//! it is allowed, which the fixture cannot be).
#![cfg(target_os = "linux")]

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _, lchown};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use js_agent::image::pull::{Host, run};
use js_contain::oci::OciConfig;
use js_contain::oci::image::published_root;
use js_contain::subids::{DaemonIds, IdFiles, user_ids};
use js_oci::{Allow, Client, Digest, media};
use js_testkit::dir::TestDir;
use js_testkit::oci_registry::{Artifact, Fixture, REGISTRY};

fn var(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("set {name}: see the module docs"))
}

fn job() -> (u32, u32) {
    let ids = (
        var("JS_B_UID").parse().expect("uid"),
        var("JS_B_GID").parse().expect("gid"),
    );
    assert_ne!(ids.0, 0, "the job user is not root");
    ids
}

fn ids() -> DaemonIds {
    let (uid, gid) = job();
    user_ids(&IdFiles::system(), uid, gid).expect("the job user's range")
}

/// What one run of the verb did.
struct Ran {
    status: String,
    stdout: String,
    stderr: String,
}

fn exit_status(code: u8) -> String {
    format!("{:?}", ExitCode::from(code))
}

/// A scratch root for one test: `0711`, root's, below `/tmp`, and a cgroup the job user
/// may manage (a unit's `Delegate=` does the same for the agent's own user), with this
/// process in a leaf of it beside the helpers' cgroups.
struct Cell {
    scratch: TestDir,
    cgroup: String,
}

impl Cell {
    fn new(label: &str) -> Self {
        let scratch = TestDir::under("/tmp", label);
        fs::set_permissions(scratch.path(), fs::Permissions::from_mode(0o711)).expect("mode");
        let (uid, gid) = job();
        let cgroup = format!("/js-pull-{label}-{}", std::process::id());
        let dir = PathBuf::from(format!("/sys/fs/cgroup{cgroup}"));
        fs::create_dir_all(dir.join("supervisor")).expect("cgroup");
        for name in [
            "",
            "cgroup.procs",
            "cgroup.threads",
            "cgroup.subtree_control",
        ] {
            lchown(dir.join(name), Some(uid), Some(gid)).expect("delegate");
        }
        fs::write(
            dir.join("supervisor/cgroup.procs"),
            std::process::id().to_string(),
        )
        .expect("move into the leaf");
        Self { scratch, cgroup }
    }

    fn scratch(&self) -> &Path {
        self.scratch.path()
    }

    fn images(&self) -> PathBuf {
        self.scratch().join("oci-images")
    }
}

impl Drop for Cell {
    /// Takes this process out of the leaf and removes the cgroup. Nothing here fails: a
    /// test that is already unwinding must not panic again, and a cgroup a failed helper
    /// left stays until the machine restarts.
    fn drop(&mut self) {
        let pid = std::process::id().to_string();
        let _ = fs::write("/sys/fs/cgroup/cgroup.procs", pid);
        let dir = PathBuf::from(format!("/sys/fs/cgroup{}", self.cgroup));
        let _ = fs::remove_dir(dir.join("supervisor"));
        let _ = fs::remove_dir(&dir);
    }
}

/// A tar of `entries`, owned by root of the image: a directory when the data is `None`.
fn tar_of(entries: &[(&str, Option<&[u8]>)]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for (path, data) in entries {
        let mut header = tar::Header::new_gnu();
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(1_000_000);
        // The name is written as given, `..` included, which the setter refuses.
        let name = &mut header.as_old_mut().name;
        name.fill(0);
        name[..path.len()].copy_from_slice(path.as_bytes());
        match data {
            Some(bytes) => {
                header.set_entry_type(tar::EntryType::Regular);
                header.set_mode(0o644);
                header.set_size(bytes.len() as u64);
                header.set_cksum();
                builder.append(&header, *bytes).expect("append");
            }
            None => {
                header.set_entry_type(tar::EntryType::Directory);
                header.set_mode(0o755);
                header.set_size(0);
                header.set_cksum();
                builder.append(&header, std::io::empty()).expect("append");
            }
        }
    }
    builder.into_inner().expect("finish")
}

/// An image of uncompressed `layers` for this machine's platform.
fn image(layers: Vec<Vec<u8>>) -> Artifact {
    let ids: Vec<String> = (0..layers.len())
        .map(|n| format!(r#""sha256:{n:064x}""#))
        .collect();
    let config = format!(
        r#"{{"architecture":"amd64","os":"linux","config":{{}},"rootfs":{{"type":"layers","diff_ids":[{}]}}}}"#,
        ids.join(",")
    );
    Artifact::build(
        media::OCI_MANIFEST,
        None,
        media::OCI_CONFIG,
        config.into_bytes(),
        media::LAYER_TAR,
        layers,
    )
}

/// The verb, as root, over the real `crun` and the agent's own binary as the daemon.
fn pull(fx: &Fixture, cell: &Cell, artifact: &Artifact) -> Ran {
    let client = |allow: Vec<Allow>| -> Client {
        fx.client_with(
            allow
                .into_iter()
                .map(|entry| entry.private().loopback())
                .collect(),
        )
    };
    let configure = |config: &mut OciConfig| {
        config.daemon = PathBuf::from(env!("CARGO_BIN_EXE_js-agent"));
    };
    let host = Host {
        euid: rustix::process::geteuid().as_raw(),
        client: &client,
        configure: Some(&configure),
    };
    let (uid, gid) = job();
    let args = [
        "js-agent".to_owned(),
        "image-pull".to_owned(),
        format!("{}/team/app@{}", fx.at(REGISTRY), artifact.digest),
        "--image-allow".to_owned(),
        format!("{}/team/", fx.at(REGISTRY)),
        "--scratch-root".to_owned(),
        cell.scratch().display().to_string(),
        "--job-user".to_owned(),
        format!("{uid}:{gid}"),
        "--cgroup-parent".to_owned(),
        cell.cgroup.clone(),
        "--crun-bin".to_owned(),
        var("JS_B_CRUN"),
    ];
    let argv: Vec<OsString> = args.iter().map(OsString::from).collect();
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

/// The names in `dir`, which must exist: a listing that cannot be made fails the test, so
/// that an assertion of "nothing left" cannot pass on a directory it never read.
fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

/// A small image is pulled, unpacked into the job's id range and published read-only, and
/// a second run says the image is already there and asks the registry for nothing. Catches
/// an unpack that lands files owned by root (the container's id 0 would not own its own
/// root), a store the job's ids could write, a second pull that fetches again, and a
/// helper that cannot run as the job user at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "root and a real crun: the agent-privilege job"]
async fn a_small_image_is_pulled_unpacked_and_published_read_only_and_a_second_run_is_a_no_op() {
    let fx = Fixture::start().await;
    let artifact = image(vec![
        tar_of(&[
            ("etc", None),
            ("etc/hello", Some(b"hello\n")),
            ("bin", None),
            ("bin/task", Some(b"#!/bin/sh\n")),
        ]),
        tar_of(&[("etc/hello", Some(b"hello again\n")), ("extra", Some(b"x"))]),
    ]);
    fx.publish(&artifact);
    let cell = Cell::new("pull-real");
    let ran = pull(&fx, &cell, &artifact);
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

    // The later layer wins; both layers' files are there.
    assert_eq!(
        fs::read(root.join("etc/hello")).expect("hello"),
        b"hello again\n"
    );
    assert_eq!(
        fs::read(root.join("bin/task")).expect("task"),
        b"#!/bin/sh\n"
    );
    assert_eq!(fs::read(root.join("extra")).expect("extra"), b"x");

    // Owned by the container's root, which is the first id of the job user's range, and
    // closed to every other writer.
    let sub = ids().sub_uid;
    for path in [&root, &root.join("etc/hello"), &root.join("bin")] {
        let meta = fs::symlink_metadata(path).expect("entry");
        assert_eq!(meta.uid(), sub, "{}", path.display());
    }
    for path in [root.parent().expect("image directory"), &root] {
        let mode = fs::metadata(path).expect("mode").mode();
        assert_eq!(mode & 0o022, 0, "{} is writable by others", path.display());
    }
    assert_eq!(
        names(&cell.images()),
        [artifact.digest.hex().to_owned()],
        "stage or layers left"
    );

    let asked = fx.requests();
    let again = pull(&fx, &cell, &artifact);
    assert_eq!(again.status, exit_status(0), "{}", again.stderr);
    assert_eq!(
        again.stdout,
        format!(
            "image-pull: status=already-published digest={} root={}\n",
            artifact.digest,
            root.display()
        )
    );
    assert_eq!(fx.requests(), asked, "the second run asked the registry");
}

/// A whiteout in a later layer removes the file of an earlier one, an opaque directory
/// hides the earlier directory's contents, and the whiteout marker itself is not left in
/// the root. This is the unpacker's own rule, pulled through the verb: catches layers
/// applied as a plain overlay of files.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "root and a real crun: the agent-privilege job"]
async fn a_whiteout_layer_is_applied() {
    let fx = Fixture::start().await;
    let artifact = image(vec![
        tar_of(&[
            ("dir", None),
            ("dir/keep", Some(b"keep")),
            ("dir/gone", Some(b"gone")),
            ("opaque", None),
            ("opaque/old", Some(b"old")),
        ]),
        tar_of(&[
            ("dir/.wh.gone", Some(b"")),
            ("opaque/.wh..wh..opq", Some(b"")),
            ("opaque/new", Some(b"new")),
        ]),
    ]);
    fx.publish(&artifact);
    let cell = Cell::new("pull-whiteout");
    let ran = pull(&fx, &cell, &artifact);
    assert_eq!(ran.status, exit_status(0), "{}", ran.stderr);
    let root = published_root(cell.scratch(), artifact.digest.as_str());
    assert_eq!(fs::read(root.join("dir/keep")).expect("kept"), b"keep");
    assert!(
        !root.join("dir/gone").exists(),
        "the whiteout did not remove it"
    );
    assert!(
        !root.join("dir/.wh.gone").exists(),
        "the marker was written"
    );
    assert!(
        !root.join("opaque/old").exists(),
        "the opaque directory kept its old file"
    );
    assert_eq!(fs::read(root.join("opaque/new")).expect("new"), b"new");
}

/// A layer that fails to verify, and a layer that unpacks half and then names a path
/// outside the root, each fail the pull (status 1) and leave no published root, no stage
/// (the half-made one holds files only the job's ids own) and no downloaded layer; the
/// good image then pulls. Catches a publish before the unpack ends and a stage that stays
/// because its files cannot be removed by the daemon user.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "root and a real crun: the agent-privilege job"]
async fn a_failed_pull_leaves_no_published_root_and_no_staging() {
    let fx = Fixture::start().await;
    let good = image(vec![tar_of(&[("f", Some(b"file"))])]);
    fx.publish(&good);
    let cell = Cell::new("pull-fails");

    // Not the bytes the manifest names.
    let layer = Digest::of(&good.layers[0]);
    fx.publish_blob_as(&layer, b"not the layer");
    let ran = pull(&fx, &cell, &good);
    assert_eq!(ran.status, exit_status(1), "{}", ran.stderr);
    assert_eq!(names(&cell.images()), Vec::<String>::new());
    fx.publish_blob_as(&layer, &good.layers[0]);

    // Half unpacked: `a/b` is written, then an entry climbs out of the root.
    let hostile = image(vec![tar_of(&[
        ("a", None),
        ("a/b", Some(b"written first")),
        ("../escape", Some(b"x")),
    ])]);
    fx.publish(&hostile);
    let ran = pull(&fx, &cell, &hostile);
    assert_eq!(ran.status, exit_status(1), "{}", ran.stderr);
    assert!(ran.stdout.is_empty(), "{}", ran.stdout);
    assert_eq!(
        names(&cell.images()),
        Vec::<String>::new(),
        "{}",
        ran.stderr
    );
    assert!(!cell.scratch().join("escape").exists());

    let ran = pull(&fx, &cell, &good);
    assert_eq!(ran.status, exit_status(0), "{}", ran.stderr);
    assert_eq!(names(&cell.images()), [good.digest.hex().to_owned()]);
}
