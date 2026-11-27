//! The released js-server: the js-lease-server image of the release, fetched from the GitHub
//! release and checked against its `SHA256SUMS` and loaded into Docker (the wrapper's part), and
//! its container started as the control host starts it, with the command line of
//! `farm/js-server.args` (the scenarios' part).
//!
//! Its one neighbour, the store, is faked by a configuration option the server already has: a
//! SQLite store in its state directory (`--store` naming `sqlite:...`, with
//! `--allow-sqlite-store`) in place of the control host's Postgres database.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use js_blackbox::{FARM_POOL, MAC_POOL, SERVER_ARGS, Version, arguments, loaded_image, render};
use sha2::{Digest, Sha256};

use crate::host::{directory, http_get, put, run, try_run};

/// The container of the server.
pub const SERVER: &str = "js-blackbox-server";

/// The user and group the image runs as, which own its secrets and its state directory.
const SERVER_USER: (u32, u32) = (65532, 65532);

/// The Linux pool's name, which the agent's flags name too.
pub const POOL: &str = "blackbox";

/// What the wrapper wrote for the server, and where it listens: what the scenarios start it
/// with.
pub struct Files {
    /// The directory mounted at `/run/secrets`.
    pub secrets: PathBuf,
    /// The directory mounted at `/etc/js-lease-server`.
    pub etc: PathBuf,
    /// The directory mounted at `/var/lib/js-lease-server`.
    pub state: PathBuf,
    /// The command line, one argument a line.
    pub args: PathBuf,
    /// The agents' port.
    pub agent_port: u16,
    /// The job service's port.
    pub job_port: u16,
    /// The readiness route's port.
    pub ready_port: u16,
    /// The client token the test calls the job service with (owned by root, 0600).
    pub client_token: PathBuf,
    /// The server's certificate, which the test and the agent trust.
    pub cert: PathBuf,
}

/// The image of the release `version` for this runner, fetched from the GitHub release named
/// by the workspace's repository and checked against the release's `SHA256SUMS`, then loaded:
/// the image's id.
pub fn load_image(dir: &Path, version: Version) -> String {
    let file = format!("js-lease-server-{version}-x86_64-unknown-linux-musl.oci.tar");
    let base = format!(
        "{}/releases/download/v{version}",
        env!("CARGO_PKG_REPOSITORY")
    );
    let tar = dir.join(&file);
    let sums = dir.join("SHA256SUMS");
    for (url, to) in [
        (format!("{base}/{file}"), &tar),
        (format!("{base}/SHA256SUMS"), &sums),
    ] {
        run(
            "curl",
            &[
                "--proto",
                "=https",
                "--fail",
                "--silent",
                "--show-error",
                "--location",
                "--retry",
                "3",
                "--output",
                to.to_str().expect("a UTF-8 path"),
                &url,
            ],
        );
    }
    let bytes = std::fs::read(&tar).unwrap_or_else(|e| panic!("read {}: {e}", tar.display()));
    let digest = hex::encode(Sha256::digest(bytes));
    let listed = std::fs::read_to_string(&sums).expect("read SHA256SUMS");
    assert!(
        listed
            .lines()
            .any(|l| l.split_whitespace().collect::<Vec<_>>() == [digest.as_str(), file.as_str()]),
        "{file} is not the file SHA256SUMS of v{version} lists (sha256 {digest})"
    );
    let printed = run(
        "docker",
        &["load", "--input", tar.to_str().expect("a UTF-8 path")],
    );
    loaded_image(&printed).unwrap_or_else(|| panic!("docker load named no image id: {printed}"))
}

/// Writes the server's files under `dir`: its secrets, its configuration (the pool files and
/// the CA the hosts' certificates chain to, `agent_ca`), its state directory and its command
/// line, with `cert` and `key` its own pair. The ports are picked here.
pub fn write_files(dir: &Path, agent_ca: &str, (cert, key): (&str, &str)) -> Files {
    let (agent_port, job_port, ready_port, metrics_port) = (
        crate::free_port(),
        crate::free_port(),
        crate::free_port(),
        crate::free_port(),
    );
    let secrets = dir.join("server-secrets");
    let etc = dir.join("server-etc");
    let state = dir.join("server-state");
    directory(&secrets, 0o755, (0, 0));
    directory(&etc, 0o755, (0, 0));
    directory(&state, 0o700, SERVER_USER);
    let token = crate::secret();
    for (name, contents) in [
        ("tls.key", key.to_owned()),
        ("token", token.clone()),
        ("join-tokens", format!("{}\n", crate::secret())),
        (
            "store.url",
            "sqlite:/var/lib/js-lease-server/store.db\n".to_owned(),
        ),
        ("kbf-client-token", crate::secret()),
    ] {
        put(&secrets.join(name), contents, 0o400, SERVER_USER);
    }
    let pool = |shape: &str, name: &str| {
        render(shape, &BTreeMap::from([("pool", name.to_owned())]))
            .unwrap_or_else(|e| panic!("a pool file: {e}"))
    };
    for (name, contents) in [
        ("tls.pem", cert.to_owned()),
        ("pool.toml", pool(FARM_POOL, POOL)),
        ("pool-mac-build.toml", pool(MAC_POOL, "blackbox-mac")),
        ("agent-client-ca.pem", agent_ca.to_owned()),
    ] {
        put(&etc.join(name), contents, 0o444, (0, 0));
    }
    let client_token = dir.join("client-token");
    put(&client_token, token, 0o600, (0, 0));
    let server_cert = dir.join("server-cert.pem");
    put(&server_cert, cert, 0o644, (0, 0));
    let args = render(
        SERVER_ARGS,
        &BTreeMap::from([
            ("agent_port", agent_port.to_string()),
            ("job_port", job_port.to_string()),
            ("ready_port", ready_port.to_string()),
            ("metrics_port", metrics_port.to_string()),
        ]),
    )
    .unwrap_or_else(|e| panic!("the server's arguments: {e}"));
    let args_file = dir.join("server.args");
    put(&args_file, arguments(&args).join("\n"), 0o644, (0, 0));
    Files {
        secrets,
        etc,
        state,
        args: args_file,
        agent_port,
        job_port,
        ready_port,
        client_token,
        cert: server_cert,
    }
}

/// Starts the server's container from `image`: its user, a read-only root, no capability, a
/// tmpfs at /tmp, the secrets, the configuration and the state mounted, Docker's host network,
/// and the command line of `args` (one argument a line).
pub fn start(image: &str, secrets: &Path, etc: &Path, state: &Path, args: &Path) {
    let mount = |from: &Path, to: &str, ro: bool| {
        format!(
            "type=bind,source={},target={to}{}",
            from.display(),
            if ro { ",readonly" } else { "" }
        )
    };
    let mounts = [
        mount(secrets, "/run/secrets", true),
        mount(etc, "/etc/js-lease-server", true),
        mount(state, "/var/lib/js-lease-server", false),
    ];
    let args =
        std::fs::read_to_string(args).unwrap_or_else(|e| panic!("read {}: {e}", args.display()));
    let _ = try_run("docker", &["rm", "--force", SERVER]);
    let mut command: Vec<&str> = vec![
        "run",
        "--detach",
        "--name",
        SERVER,
        "--network",
        "host",
        "--user",
        "65532:65532",
        "--read-only",
        "--cap-drop",
        "ALL",
        "--tmpfs",
        "/tmp",
        "--stop-signal",
        "SIGTERM",
        "--stop-timeout",
        "120",
    ];
    for m in &mounts {
        command.extend(["--mount", m.as_str()]);
    }
    command.push(image);
    command.extend(args.lines().filter(|l| !l.is_empty()));
    run("docker", &command);
}

/// The readiness route's answer, `Ok` only for 200.
pub fn ready(addr: SocketAddr) -> Result<(), String> {
    match http_get(addr, "/readyz")? {
        (200, _) => Ok(()),
        (status, body) => Err(format!("/readyz answered {status}: {body}")),
    }
}

/// Restarts the server's container: SIGTERM, the drain, the exit, and a new start of the same
/// command over the same store and state directory.
pub fn restart() {
    run("docker", &["restart", "--time", "120", SERVER]);
}

/// The last lines the server logged, for a failure message.
pub fn logs() -> String {
    try_run("docker", &["logs", "--tail", "40", SERVER]).unwrap_or_else(|e| e)
}
