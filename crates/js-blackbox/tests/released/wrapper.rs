//! The wrapper (`released rig --version X.Y.Z`): installs the release and wires the test, then
//! runs the test, `released scenarios`, with flags that name what it installed and where each
//! fake listens. It is the replaceable part: a runner that installs the package another way
//! (an image of its own, say) runs `released scenarios` with the same flags.
//!
//! What it does, in order, on a disposable host as root:
//!
//! - mints the certificates: a CA standing in for hostkeeper's, the host's certificate in
//!   hostkeeper's shape, and the server's own;
//! - starts the fake registry (`released fake-registry`, as a transient unit of its own), which
//!   serves the job image on the loopback;
//! - fetches the released js-server image, checks it against the release's `SHA256SUMS`, loads
//!   it, and writes the server's secrets, configuration and command line;
//! - adds the project's apt repository with the suite `testing`, installs the released js-agent,
//!   downloads its package and the previous release's for the update scenario, and makes the host
//!   ready as a farm host is, with the flags file of the farm's shape;
//! - starts the fake mount provider (`released fake-provider`, a transient unit of its own) on
//!   the socket the flags name;
//! - runs `released scenarios` and exits as it does, after stopping the fakes.
//!
//! It installs no other service's package: the server is this repository's own release, and the
//! store, the registry and the mount provider are fakes.

use std::path::Path;
use std::process::{Command, ExitCode};

use js_blackbox::{READY_WITHIN, Seen, Version};
use js_testkit::dir::TestDir;
use rcgen::string::Ia5String;
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DistinguishedName, DnType,
    ExtendedKeyUsagePurpose, IsCa, KeyPair, SanType,
};

use crate::host::{run, try_run};
use crate::{agent, server, watch};

/// The URI SAN hostkeeper puts in a host certificate beside the host's DNS name.
const HOST_ROLE_URI: &str = "urn:hostkeeper:host-role:host";

/// The transient units of the fakes.
const FAKE_REGISTRY: &str = "js-blackbox-fake-registry";
const FAKE_PROVIDER: &str = "js-blackbox-fake-provider";

/// The socket the farm's flags name for tierstore's mount provider, where the fake serves.
const PROVIDER_SOCKET: &str = "/var/lib/tierstore/provider.sock";

/// Where the fake provider makes its trees: under the farm's tierstore mount roots.
const PROVIDER_TREES: &str = "/run/tierstore/fs/trees";

/// The flags of `released rig`.
#[derive(Debug, clap::Args)]
pub struct Flags {
    /// The release to test, MAJOR.MINOR.PATCH: published, and in the apt suite `testing`.
    #[arg(long)]
    pub version: String,
}

/// Installs, wires, and runs the test.
pub fn run_rig(flags: &Flags) -> ExitCode {
    assert!(
        rustix::process::geteuid().is_root(),
        "the black-box test installs a package, mounts a disk and starts containers: run it as root on a disposable runner"
    );
    let version = Version::parse(&flags.version)
        .unwrap_or_else(|| panic!("--version {:?} is not MAJOR.MINOR.PATCH", flags.version));
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "released");
    let me = std::env::current_exe().expect("this program's path");

    let ca = ca();
    let identity = host_identity(&ca);
    let made = rcgen::generate_simple_self_signed(vec![
        agent::SERVER_NAME.to_owned(),
        "127.0.0.1".to_owned(),
    ])
    .expect("the server's certificate");
    let (server_cert, server_key) = (made.cert.pem(), made.signing_key.serialize_pem());

    let registry_file = dir.join("registry");
    start_fake(
        FAKE_REGISTRY,
        &me,
        &["fake-registry", "--out", path(&registry_file)],
    );
    let registry = wait_for_file(&registry_file);
    let mut lines = registry.lines();
    let registry_port: u16 = lines
        .next()
        .and_then(|p| p.parse().ok())
        .unwrap_or_else(|| panic!("the fake registry's port: {registry:?}"));
    let image = lines
        .next()
        .unwrap_or_else(|| panic!("the fake registry's image: {registry:?}"))
        .to_owned();

    let server_image = server::load_image(&dir, version);
    let files = server::write_files(&dir, &ca.pem(), (&server_cert, &server_key));

    agent::add_repository("amd64");
    agent::install(version);
    let previous = agent::previous(version);
    let released_deb = agent::download(version, &dir);
    let previous_deb = agent::download(previous, &dir);
    agent::prepare_host(
        &dir,
        &server_cert,
        (&identity.0, &identity.1),
        files.agent_port,
        registry_port,
    );

    start_fake(
        FAKE_PROVIDER,
        &me,
        &[
            "fake-provider",
            "--socket",
            PROVIDER_SOCKET,
            "--trees",
            PROVIDER_TREES,
            "--answer",
            "success",
        ],
    );
    wait_for_file(Path::new(PROVIDER_SOCKET));

    let ports = (
        files.agent_port.to_string(),
        format!("https://127.0.0.1:{}", files.job_port),
        format!("127.0.0.1:{}", files.ready_port),
    );
    let status = Command::new(&me)
        .args([
            "scenarios",
            "--server-image",
            &server_image,
            "--server-secrets",
            path(&files.secrets),
            "--server-etc",
            path(&files.etc),
            "--server-state",
            path(&files.state),
            "--server-args",
            path(&files.args),
            "--server-ready",
            &ports.2,
            "--server-agent-port",
            &ports.0,
            "--server-name",
            agent::SERVER_NAME,
            "--job-service",
            &ports.1,
            "--client-token",
            path(&files.client_token),
            "--ca-cert",
            path(&files.cert),
            "--agent-unit",
            agent::UNIT,
            "--agent-health",
            agent::HEALTH,
            "--host-id",
            agent::HOST_ID,
            "--released",
            &version.to_string(),
            "--previous",
            &previous.to_string(),
            "--previous-deb",
            path(&previous_deb),
            "--released-deb",
            path(&released_deb),
            "--image",
            &image,
            "--provider",
            "tierstore",
        ])
        .status()
        .expect("run the scenarios");
    for unit in [FAKE_PROVIDER, FAKE_REGISTRY] {
        let _ = try_run("systemctl", &["stop", unit]);
    }
    if status.success() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn path(p: &Path) -> &str {
    p.to_str().expect("a UTF-8 path")
}

/// Starts `program` with `args` as the transient unit `unit`, a process of its own.
fn start_fake(unit: &str, program: &Path, args: &[&str]) {
    let _ = try_run("systemctl", &["stop", unit]);
    let mut command = vec!["--unit", unit, "--collect", "--quiet", path(program)];
    command.extend(args);
    run("systemd-run", &command);
}

/// Waits for `file` to exist (a fake writes it once it serves): its text, empty for a socket.
fn wait_for_file(file: &Path) -> String {
    watch(|w| {
        w.until(READY_WITHIN, &mut || {
            if file.exists() {
                Seen::Yes
            } else {
                Seen::No(format!("no {}", file.display()))
            }
        })
    })
    .within("a fake serves", READY_WITHIN)
    .unwrap_or_else(|e| panic!("{e}"));
    std::fs::read_to_string(file).unwrap_or_default()
}

/// The CA of the hosts' certificates: hostkeeper's CA as the server sees it.
fn ca() -> CertifiedIssuer<'static, KeyPair> {
    let mut params = CertificateParams::new(Vec::<String>::new()).expect("CA params");
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.distinguished_name = DistinguishedName::new();
    params
        .distinguished_name
        .push(DnType::CommonName, "test-only hostkeeper CA");
    let key = KeyPair::generate().expect("a CA key");
    CertifiedIssuer::self_signed(params, key).expect("the CA")
}

/// The host's certificate and key, as PEM, in the shape hostkeeper issues: one DNS name, the
/// host id, and client authentication alone.
fn host_identity(ca: &CertifiedIssuer<'static, KeyPair>) -> (String, String) {
    let key = KeyPair::generate().expect("a host key");
    let mut params = CertificateParams::new(vec![agent::HOST_ID.to_owned()]).expect("host params");
    let uri = Ia5String::try_from(HOST_ROLE_URI.to_owned()).expect("an IA5 URI");
    params.subject_alt_names.push(SanType::URI(uri));
    params.distinguished_name = DistinguishedName::new();
    params
        .distinguished_name
        .push(DnType::CommonName, format!("host:{}", agent::HOST_ID));
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    let cert = params.signed_by(&key, ca).expect("the host certificate");
    (cert.pem(), key.serialize_pem())
}
