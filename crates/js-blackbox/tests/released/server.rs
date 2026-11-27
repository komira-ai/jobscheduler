//! The released js-server: the js-lease-server image of the release, fetched from the GitHub
//! release and checked against its `SHA256SUMS`, loaded into Docker and started as the control
//! host starts it (`SERVER_ARGS`), over a Postgres store in a container of its own.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use js_blackbox::{FARM_POOL, MAC_POOL, SERVER_ARGS, Version, arguments, loaded_image, render};
use sha2::{Digest, Sha256};

use crate::host::{directory, http_get, put, run, try_run};

/// The container of the server.
pub const SERVER: &str = "js-blackbox-server";

/// The container of the store.
const POSTGRES: &str = "js-blackbox-postgres";

/// The Postgres image of the store, pinned by digest: the one release.yml's unit tests run.
const POSTGRES_IMAGE: &str =
    "postgres@sha256:2a0d0fe14825b0939f78a8cad5cd4e6aa68bf94d0e5dd96e24b6d23af4315545";

/// The user and group the image runs as, which own its secrets and its state directory.
const SERVER_USER: (u32, u32) = (65532, 65532);

/// The Linux pool's name, which the agent's flags name too.
pub const POOL: &str = "blackbox";

/// The server's four listeners, on the loopback.
#[derive(Clone, Copy)]
pub struct Ports {
    /// The agents' routes (TLS).
    pub agent: u16,
    /// The job service (TLS).
    pub job: u16,
    /// The readiness route (plain HTTP).
    pub ready: u16,
    /// The metrics (plain HTTP).
    pub metrics: u16,
}

/// What the server is started with.
pub struct Server {
    /// Its listeners.
    pub ports: Ports,
    /// The client token the test calls the job service with (owned by root, 0600).
    pub client_token: PathBuf,
    /// The server's certificate, which the test and the agent trust.
    pub cert: PathBuf,
}

impl Server {
    /// The readiness route's answer, `Ok` only for 200.
    pub fn ready(&self) -> Result<(), String> {
        let addr = SocketAddr::from(([127, 0, 0, 1], self.ports.ready));
        match http_get(addr, "/readyz")? {
            (200, _) => Ok(()),
            (status, body) => Err(format!("/readyz answered {status}: {body}")),
        }
    }

    /// Restarts the server's container: SIGTERM, the drain, the exit, and a new start of the
    /// same command over the same store and state directory.
    pub fn restart(&self) {
        run("docker", &["restart", "--time", "120", SERVER]);
    }

    /// The last lines the server logged, for a failure message.
    pub fn logs() -> String {
        try_run("docker", &["logs", "--tail", "40", SERVER]).unwrap_or_else(|e| e)
    }
}

/// Starts the store and applies the state schema to it (what the installer does on the control
/// host): its URL.
pub fn start_postgres(watch: &mut js_blackbox::Watch<'_>) -> String {
    let _ = try_run("docker", &["rm", "--force", POSTGRES]);
    let password = crate::secret();
    run(
        "docker",
        &[
            "run",
            "--detach",
            "--name",
            POSTGRES,
            "--env",
            &format!("POSTGRES_PASSWORD={password}"),
            "--publish",
            "127.0.0.1::5432",
            POSTGRES_IMAGE,
        ],
    );
    let published = run("docker", &["port", POSTGRES, "5432/tcp"]);
    let addr = published
        .lines()
        .find(|l| l.starts_with("127.0.0.1:"))
        .unwrap_or_else(|| panic!("the store's port: {published}"))
        .trim()
        .to_owned();
    let url = format!("postgres://postgres:{password}@{addr}/postgres?sslmode=disable");
    let config = js_state::postgres::PostgresConfig::parse(&url).expect("the store's URL");
    let set = js_state::postgres::schema::embedded();
    let upto = js_state::postgres::schema::latest_of(&set);
    watch
        .until(
            js_blackbox::READY_WITHIN,
            &mut || match js_state::postgres::schema::apply(&config, &set, upto) {
                Ok(_) => js_blackbox::Seen::Yes,
                Err(e) => js_blackbox::Seen::No(e.to_string()),
            },
        )
        .within(
            "the store takes the state schema",
            js_blackbox::READY_WITHIN,
        )
        .unwrap_or_else(|e| panic!("{e}"));
    url
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

/// Writes the server's files under `dir` and starts its container from `image`: the files and
/// the ports. `store` is the store's URL, `agent_ca` the PEM of the CA the hosts' certificates
/// chain to, `cert` and `key` the server's own pair.
pub fn start_server(
    dir: &Path,
    image: &str,
    store: &str,
    agent_ca: &str,
    (cert, key): (&str, &str),
) -> Server {
    let ports = Ports {
        agent: crate::free_port(),
        job: crate::free_port(),
        ready: crate::free_port(),
        metrics: crate::free_port(),
    };
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
        ("store.url", format!("{store}\n")),
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
            ("agent_port", ports.agent.to_string()),
            ("job_port", ports.job.to_string()),
            ("ready_port", ports.ready.to_string()),
            ("metrics_port", ports.metrics.to_string()),
        ]),
    )
    .unwrap_or_else(|e| panic!("the server's arguments: {e}"));
    let mount = |from: &Path, to: &str, ro: bool| {
        format!(
            "type=bind,source={},target={to}{}",
            from.display(),
            if ro { ",readonly" } else { "" }
        )
    };
    let mounts = [
        mount(&secrets, "/run/secrets", true),
        mount(&etc, "/etc/js-lease-server", true),
        mount(&state, "/var/lib/js-lease-server", false),
    ];
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
    let args = arguments(&args);
    command.extend(args.iter().map(String::as_str));
    run("docker", &command);
    Server {
        ports,
        client_token,
        cert: server_cert,
    }
}
