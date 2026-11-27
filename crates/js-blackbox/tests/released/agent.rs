//! The released js-agent: the `.deb` installed with apt from the suite `testing` of the
//! project's repository, as `check-apt` installs it, on a host made ready as a farm host is
//! (the scratch disk, hostkeeper's identity and facts files, tierstore's account and roots, the
//! flags file of the farm's shape), which the wrapper does; and run by the package's own unit,
//! `js-agent-crun.service`, which the scenarios start, restart and update.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use js_blackbox::{AGENT_FLAGS, Version, previous_version, render};

use crate::host::{directory, http_get, put, run, run_in, try_run};

/// The unit the package installs and the farm's hosts run.
pub const UNIT: &str = "js-agent-crun.service";

/// The address the unit gives the agent's health route (`--health-listen`).
pub const HEALTH: &str = "127.0.0.1:7311";

/// The project's apt repository, and the keyring its index is signed with.
const APT: &str = "https://packages.komira.ai/apt";
const KEYRING: &str = "/usr/share/keyrings/komira-archive-keyring.gpg";

/// The state root the unit gives the agent, where the update hook writes the handover file.
const STATE_ROOT: &str = "/var/lib/js-agent/state";

/// The scratch root's mount point, which the unit needs mounted (`ConditionPathIsMountPoint`).
const SCRATCH: &str = "/var/lib/js-agent/scratch";

/// The host's id, the one DNS name of its certificate and the `host_id` of its facts file.
pub const HOST_ID: &str = "blackbox-node-1";

/// The name the agent reaches the server by (the server's certificate holds it), resolved
/// through `/etc/hosts`.
pub const SERVER_NAME: &str = "js.blackbox.test";

/// The line of `/etc/hosts` that resolves [`SERVER_NAME`].
pub const HOSTS_LINE: &str = "127.0.0.1 js.blackbox.test";

/// An endpoint for `--egress-allow` (a documentation address: no job of the test asks for a
/// network).
const EGRESS_ENDPOINT: &str = "192.0.2.5:9300";

/// Adds the project's apt repository with the suite `testing`, as `check-apt` does, and reads
/// its index.
pub fn add_repository(arch: &str) {
    run(
        "curl",
        &[
            "--proto",
            "=https",
            "--tlsv1.2",
            "--fail",
            "--silent",
            "--show-error",
            "--retry",
            "3",
            "--create-dirs",
            "--output",
            KEYRING,
            &format!("{APT}/komira-archive-keyring.gpg"),
        ],
    );
    put(
        Path::new("/etc/apt/sources.list.d/komira.sources"),
        format!(
            "Types: deb\nURIs: {APT}\nSuites: testing\nComponents: main\nArchitectures: {arch}\nSigned-By: {KEYRING}\n"
        ),
        0o644,
        (0, 0),
    );
    run("apt-get", &["update"]);
}

/// The newest js-agent in the suite below `released`.
pub fn previous(released: Version) -> Version {
    let listed = run("apt-cache", &["madison", "js-agent"]);
    previous_version(&listed, released).unwrap_or_else(|| {
        panic!("the suite testing lists no js-agent older than {released}: {listed}")
    })
}

/// Downloads the package of js-agent `version` from the suite into `dir`: its path.
pub fn download(version: Version, dir: &Path) -> PathBuf {
    run_in(
        dir,
        "apt-get",
        &["download", &format!("js-agent={version}")],
    );
    let name = format!("js-agent_{version}_amd64.deb");
    let path = dir.join(&name);
    assert!(
        path.is_file(),
        "apt-get download made no {name} in {}",
        dir.display()
    );
    path
}

/// Installs the package file `deb` (an older one too), as the update does, and checks the
/// installed program says `version`.
pub fn install_file(deb: &Path, version: Version) {
    run(
        "apt-get",
        &[
            "install",
            "--yes",
            "--no-install-recommends",
            "--allow-downgrades",
            deb.to_str().expect("a UTF-8 path"),
        ],
    );
    check_version(version);
}

/// Installs js-agent `version` from the suite, and checks the installed program says it.
pub fn install(version: Version) {
    run(
        "apt-get",
        &[
            "install",
            "--yes",
            "--no-install-recommends",
            "--allow-downgrades",
            &format!("js-agent={version}"),
        ],
    );
    check_version(version);
}

fn check_version(version: Version) {
    let said = run("/usr/bin/js-agent", &["--version"]);
    assert_eq!(
        said.lines().next(),
        Some(format!("js-agent {version}").as_str()),
        "the installed program"
    );
}

/// Makes the host ready as a farm host is, with what `dir` keeps (the scratch disk's image):
/// the scratch disk, hostkeeper's files, tierstore's user and roots, the server's name, the CA
/// file and the flags file. `ca_pem` is the server's certificate, `identity` the host's
/// certificate and key, `agent_port` the server's agent port, `registry_port` the job image's
/// registry.
pub fn prepare_host(
    dir: &Path,
    ca_pem: &str,
    (host_cert, host_key): (&str, &str),
    agent_port: u16,
    registry_port: u16,
) {
    // The scratch disk: a file system of its own with project quota (each job's /tmp is bounded
    // by one), mounted where the unit wants it, then the package's directories made over it.
    let image = dir.join("scratch.img");
    let image = image.to_str().expect("a UTF-8 path");
    run("truncate", &["--size", "8G", image]);
    run("mkfs.xfs", &["-q", image]);
    directory(Path::new(SCRATCH), 0o711, (0, 0));
    run("mount", &["-o", "loop,prjquota", image, SCRATCH]);
    run(
        "systemd-tmpfiles",
        &["--create", "/usr/lib/tmpfiles.d/js-agent-crun.conf"],
    );
    // hostkeeper's identity and facts files, as its daemon keeps them.
    directory(Path::new("/var/lib/hostkeeper/identity"), 0o700, (0, 0));
    put(
        Path::new("/var/lib/hostkeeper/facts.json"),
        format!("{{\"schema\":1,\"host_id\":\"{HOST_ID}\"}}\n"),
        0o644,
        (0, 0),
    );
    put(
        Path::new("/var/lib/hostkeeper/identity/host.crt"),
        host_cert,
        0o644,
        (0, 0),
    );
    put(
        Path::new("/var/lib/hostkeeper/identity/host.key"),
        host_key,
        0o600,
        (0, 0),
    );
    // tierstore's account, its socket's directory and its file system roots. No tierstore runs:
    // no job of the test names its provider.
    if try_run("id", &["tierstore"]).is_err() {
        run(
            "useradd",
            &[
                "--system",
                "--no-create-home",
                "--shell",
                "/usr/sbin/nologin",
                "tierstore",
            ],
        );
    }
    let tierstore = (uid_of("tierstore"), gid_of("tierstore"));
    directory(Path::new("/var/lib/tierstore"), 0o750, tierstore);
    for root in ["/run/tierstore/fs/trees", "/run/tierstore/fs/put"] {
        directory(Path::new(root), 0o755, (0, 0));
    }
    // The server's name.
    let hosts = std::fs::read_to_string("/etc/hosts").expect("read /etc/hosts");
    if !hosts.lines().any(|l| l.trim() == HOSTS_LINE) {
        put(
            Path::new("/etc/hosts"),
            format!("{hosts}\n{HOSTS_LINE}\n"),
            0o644,
            (0, 0),
        );
    }
    // The config package's files: the CA the server's certificate chains to, and the flags.
    directory(Path::new("/etc/js-agent"), 0o700, (0, 0));
    put(Path::new("/etc/js-agent/ca.pem"), ca_pem, 0o644, (0, 0));
    let flags = render(
        AGENT_FLAGS,
        &BTreeMap::from([
            ("server_name", SERVER_NAME.to_owned()),
            ("agent_port", agent_port.to_string()),
            ("pool", crate::server::POOL.to_owned()),
            ("registry_port", registry_port.to_string()),
            ("egress_endpoint", EGRESS_ENDPOINT.to_owned()),
        ]),
    )
    .unwrap_or_else(|e| panic!("the agent's flags: {e}"));
    put(Path::new("/etc/js-agent/flags"), flags, 0o644, (0, 0));
    run("systemctl", &["daemon-reload"]);
}

/// Starts the unit.
pub fn start(unit: &str) {
    run("systemctl", &["start", unit]);
}

/// Restarts the unit after a new package: systemd reads the unit again, and the stop takes the
/// handover file if there is one.
pub fn restart(unit: &str) {
    run("systemctl", &["daemon-reload"]);
    run("systemctl", &["restart", unit]);
}

/// Writes the file the update hook writes before it stops the agent, which makes the stop leave
/// the runs to the next process: `handover` in the state root, a regular file of root's, 0600.
pub fn ask_for_handover() {
    put(
        &Path::new(STATE_ROOT).join("handover"),
        "the black-box test's update\n",
        0o600,
        (0, 0),
    );
}

/// The agent's health route: `Ok` only for 200 (`placeable`: a session's welcome held, no taint).
pub fn healthy(addr: SocketAddr) -> Result<(), String> {
    match http_get(addr, "/healthz")? {
        (200, _) => Ok(()),
        (status, body) => Err(format!("the agent's /healthz answered {status}: {body}")),
    }
}

/// The unit's last journal lines, for a failure message.
pub fn journal(unit: &str) -> String {
    try_run(
        "journalctl",
        &["--unit", unit, "--no-pager", "--lines", "40"],
    )
    .unwrap_or_else(|e| e)
}

fn uid_of(user: &str) -> u32 {
    run("id", &["-u", user]).trim().parse().expect("a uid")
}

fn gid_of(user: &str) -> u32 {
    run("id", &["-g", user]).trim().parse().expect("a gid")
}
