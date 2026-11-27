//! The runner as a farm node: the two packages from their apt channel, the agent's scratch
//! volume, files at the paths and modes the farm's config packages give them, and the units.

use std::fs::{self, DirBuilder};
use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

/// The apt repository both packages are published to.
const APT_URL: &str = "https://packages.komira.ai/apt";

/// The repository's signing keyring, a binary keyring as apt's `Signed-By` takes it.
const KEYRING: &str = "/usr/share/keyrings/komira-archive-keyring.gpg";

/// The source the test adds: `testing`, where a release's js-agent is published first, and
/// `stable`, tierstore's public channel.
const SOURCES: &str = "/etc/apt/sources.list.d/komira.sources";

/// The agent's scratch volume, which the packaged unit starts only on (it must be a mount point,
/// XFS with project quotas).
pub const SCRATCH: &str = "/var/lib/js-agent/scratch";

/// The agent's state root, beside the scratch volume.
const AGENT_STATE: &str = "/var/lib/js-agent/state";

/// The image file of the scratch volume.
const SCRATCH_IMAGE: &str = "/var/lib/js-tierstore-blackbox/scratch.img";

/// The js-agent unit the farm's Linux nodes run (the crun runtime).
pub const AGENT_UNIT: &str = "js-agent-crun.service";

/// The tierstore instance the test runs.
pub const TIERSTORE_UNIT: &str = "tierstore@a.service";

/// The versions the test installed.
pub struct Installed {
    /// js-agent's, as dpkg reports it.
    pub agent: String,
    /// tierstore's, as dpkg reports it.
    pub tierstore: String,
}

/// Fails the test unless it runs as root.
pub fn require_root() {
    assert!(
        rustix::process::geteuid().is_root(),
        "this test installs packages and starts units: run it as root"
    );
}

/// `program args`, which must succeed; its standard output.
pub fn run(program: &str, args: &[&str]) -> String {
    let output = Command::new(program)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("run {program}: {e}"));
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        output.status.success(),
        "{program} {} ended {}:\n{stdout}\n{}",
        args.join(" "),
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    stdout
}

/// `program args`, whose failure is not the test's: whether it succeeded.
fn try_run(program: &str, args: &[&str]) -> bool {
    Command::new(program)
        .args(args)
        .output()
        .is_ok_and(|output| output.status.success())
}

/// Installs js-agent at `agent_version` from `testing` and tierstore at its newest release from
/// `stable`, as an operator does (`apt-get install`).
pub fn install(agent_version: &str) -> Installed {
    run(
        "curl",
        &[
            "--proto",
            "=https",
            "--tlsv1.2",
            "--fail",
            "--silent",
            "--show-error",
            "--create-dirs",
            "--output",
            KEYRING,
            &format!("{APT_URL}/komira-archive-keyring.gpg"),
        ],
    );
    put(
        Path::new(SOURCES),
        &format!(
            "Types: deb\nURIs: {APT_URL}\nSuites: testing stable\nComponents: main\n\
             Architectures: amd64\nSigned-By: {KEYRING}\n"
        ),
        0o644,
    );
    wait_listed(agent_version);
    run(
        "apt-get",
        &[
            "install",
            "--yes",
            "--no-install-recommends",
            "--allow-downgrades",
            &format!("js-agent={agent_version}"),
            "tierstore/stable",
        ],
    );
    let version = |package: &str| {
        run(
            "dpkg-query",
            &["--show", "--showformat=${Version}", package],
        )
    };
    let installed = Installed {
        agent: version("js-agent"),
        tierstore: version("tierstore"),
    };
    assert_eq!(installed.agent, agent_version, "the js-agent apt installed");
    installed
}

/// How long the CDN in front of the apt repository may take to serve the index a release
/// wrote, before the test fails.
const INDEX_LIMIT: Duration = Duration::from_secs(10 * 60);

/// Updates apt's lists until `testing` lists js-agent at `version`: the release wrote the index
/// moments before, and the CDN serves it once its invalidation has spread.
fn wait_listed(version: &str) {
    let limit = Instant::now() + INDEX_LIMIT;
    loop {
        run("apt-get", &["update"]);
        let listed = run("apt-cache", &["madison", "js-agent"]);
        if listed
            .lines()
            .any(|line| line.split('|').nth(1).map(str::trim) == Some(version))
        {
            return;
        }
        assert!(
            Instant::now() < limit,
            "testing did not list js-agent {version} within {} s:\n{listed}",
            INDEX_LIMIT.as_secs()
        );
        std::thread::sleep(Duration::from_secs(20));
    }
}

/// The agent's scratch volume: an XFS file system with project quotas on a loop device, mounted
/// where the packaged unit looks for it, and the directories the package's tmpfiles entry makes.
pub fn scratch() {
    if !try_run("findmnt", &["--noheadings", SCRATCH]) {
        let image = Path::new(SCRATCH_IMAGE);
        make_dir(image.parent().expect("a parent"), 0o700);
        run("truncate", &["--size", "8G", SCRATCH_IMAGE]);
        run("mkfs.xfs", &["-q", "-f", SCRATCH_IMAGE]);
        make_dir(Path::new(SCRATCH), 0o711);
        run("mount", &["-o", "loop,prjquota", SCRATCH_IMAGE, SCRATCH]);
    }
    tmpfiles();
}

/// The directories of the package's tmpfiles entry.
fn tmpfiles() {
    run(
        "systemd-tmpfiles",
        &["--create", "/usr/lib/tmpfiles.d/js-agent-crun.conf"],
    );
}

/// Stops `unit`; a unit that is not running is not a failure.
pub fn stop(unit: &str) {
    let _ = try_run("systemctl", &["stop", unit]);
    let _ = try_run("systemctl", &["reset-failed", unit]);
}

/// Starts `unit`.
pub fn start(unit: &str) {
    run("systemctl", &["start", unit]);
}

/// Forgets what an earlier test left in the agent's state root and scratch, so that each test's
/// agent says hello as a new host of a new server. The agent is stopped.
pub fn forget_agent() {
    for root in [AGENT_STATE, &format!("{SCRATCH}/runs")] {
        if let Ok(entries) = fs::read_dir(root) {
            for entry in entries.map_while(Result::ok) {
                let path = entry.path();
                let removed = if path.is_dir() {
                    fs::remove_dir_all(&path)
                } else {
                    fs::remove_file(&path)
                };
                removed.unwrap_or_else(|e| panic!("remove {}: {e}", path.display()));
            }
        }
    }
    tmpfiles();
}

/// The last lines the units logged, for a failure's report.
pub fn journal() -> String {
    Command::new("journalctl")
        .args([
            "--no-pager",
            "--lines=200",
            "--unit",
            AGENT_UNIT,
            "--unit",
            TIERSTORE_UNIT,
        ])
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
        .unwrap_or_else(|e| format!("journalctl: {e}"))
}

/// `path` as a directory with `mode`, its missing parents made 0755.
pub fn make_dir(path: &Path, mode: u32) {
    DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create(path)
        .unwrap_or_else(|e| panic!("make {}: {e}", path.display()));
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .unwrap_or_else(|e| panic!("chmod {}: {e}", path.display()));
}

/// `contents` at `path`, mode `mode`, owned by root, its missing parents made 0755.
pub fn put(path: &Path, contents: &str, mode: u32) {
    let parent = path.parent().expect("a parent");
    DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create(parent)
        .unwrap_or_else(|e| panic!("make {}: {e}", parent.display()));
    fs::write(path, contents).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .unwrap_or_else(|e| panic!("chmod {}: {e}", path.display()));
}

/// Gives `path` to `owner` (`user:group`).
pub fn chown(path: &Path, owner: &str) {
    run("chown", &[owner, path.to_str().expect("a UTF-8 path")]);
}
