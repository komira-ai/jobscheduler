//! The host certificates the tests present to a lease server over TLS: a certificate
//! authority of the test process that the servers of the tests trust for agents, the server's
//! own TLS pair, and the flags that give an agent its certificate, its key and the server's
//! certificate to trust. A host is identified by its certificate alone, so every agent of a test
//! that runs against a real server gets one.
//!
//! Every certificate and key is made in the test; nothing here is a secret.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use js_client::HostIdentity;
use rcgen::string::Ia5String;
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DistinguishedName, DnType,
    ExtendedKeyUsagePurpose, IsCa, KeyPair, SanType, SerialNumber, date_time_ymd,
};

use crate::support::Scratch;

/// The URI SAN hostkeeper puts on a host's own certificate.
const HOST_ROLE_URI: &str = "urn:hostkeeper:host-role:host";

/// A certificate (leaf first) and its key, as PEM text.
pub struct Identity {
    /// The certificate.
    pub cert_pem: String,
    /// The key.
    pub key_pem: String,
}

/// A certificate authority of the test process.
pub struct Ca(CertifiedIssuer<'static, KeyPair>);

/// The authority every server of the tests trusts for host certificates, made once per test
/// process.
pub fn shared_ca() -> &'static Ca {
    static CA: OnceLock<Ca> = OnceLock::new();
    CA.get_or_init(Ca::new)
}

impl Ca {
    fn new() -> Self {
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.distinguished_name = DistinguishedName::new();
        params
            .distinguished_name
            .push(DnType::CommonName, "test-only hostkeeper CA");
        Self(CertifiedIssuer::self_signed(params, KeyPair::generate().unwrap()).unwrap())
    }

    /// The authority's certificate as PEM.
    pub fn pem(&self) -> String {
        self.0.pem()
    }

    /// The host certificate of `host`, in the shape hostkeeper issues: the host id as the one
    /// DNS name, the host role URI, client authentication, and a new key.
    pub fn issue_host(&self, host: &str) -> Identity {
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(vec![host.to_owned()]).unwrap();
        let uri = Ia5String::try_from(HOST_ROLE_URI.to_owned()).unwrap();
        params.subject_alt_names.push(SanType::URI(uri));
        params.distinguished_name = DistinguishedName::new();
        params
            .distinguished_name
            .push(DnType::CommonName, format!("host:{host}"));
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        params.serial_number = Some(SerialNumber::from(1u64));
        params.not_before = date_time_ymd(2020, 1, 1);
        params.not_after = date_time_ymd(3000, 1, 1);
        let cert = params.signed_by(&key, &self.0).unwrap();
        Identity {
            cert_pem: cert.pem(),
            key_pem: key.serialize_pem(),
        }
    }
}

/// A self-signed certificate of `host` and its key, as files of `dir` (the key mode 0600): a
/// host identity that parses and that no server trusts.
pub fn self_signed(dir: &Scratch, host: &str) -> HostIdentity {
    let made = rcgen::generate_simple_self_signed(vec![host.to_owned()]).unwrap();
    let cert = dir.write(&format!("{host}-self.crt"), &made.cert.pem(), 0o644);
    let key = dir.write(
        &format!("{host}-self.key"),
        &made.signing_key.serialize_pem(),
        0o600,
    );
    HostIdentity::new(cert, key)
}

/// The TLS pair of a test's server, as files of `scratch`, made the first time it is asked
/// for and kept for a restart over the same files: the certificate and the key.
pub fn server_pair(scratch: &Scratch) -> (PathBuf, PathBuf) {
    let (cert, key) = (
        scratch.path("server-cert.pem"),
        scratch.path("server-key.pem"),
    );
    if !cert.exists() {
        let names = vec!["127.0.0.1".to_owned(), "localhost".to_owned()];
        let made = rcgen::generate_simple_self_signed(names).unwrap();
        scratch.write("server-cert.pem", &made.cert.pem(), 0o644);
        scratch.write("server-key.pem", &made.signing_key.serialize_pem(), 0o600);
    }
    (cert, key)
}

/// The flags that make a server of the tests serve TLS with a pair of its own
/// ([`server_pair`]) and trust the authority of the process for the agents' host certificates
/// (`--agent-client-ca`), whose certificate is written to `scratch`.
pub fn server_flags(scratch: &Scratch) -> Vec<String> {
    let ca = scratch.write("hostkeeper-ca.pem", &shared_ca().pem(), 0o644);
    let (cert, key) = server_pair(scratch);
    let text = |path: &Path| path.display().to_string();
    vec![
        "--tls-cert".to_owned(),
        text(&cert),
        "--tls-key".to_owned(),
        text(&key),
        "--agent-client-ca".to_owned(),
        text(&ca),
    ]
}

/// Where the agents of a test find their host certificates, and the server certificate they
/// trust: the flags that give an agent of a host its identity.
#[derive(Clone, Debug)]
pub struct Identities {
    dir: PathBuf,
    server_cert: PathBuf,
}

impl Identities {
    /// The identities of the agents of the server whose files are in `scratch`.
    pub fn new(scratch: &Scratch) -> Self {
        let dir = scratch.path("identities");
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        Self {
            dir,
            server_cert: server_pair(scratch).0,
        }
    }

    /// The server certificate an agent trusts (`--ca-cert`).
    pub fn server_cert(&self) -> &Path {
        &self.server_cert
    }

    /// The certificate and key of `host`, issued by the process's authority the first time they
    /// are asked for and kept after: an agent of the host reads the same pair again.
    pub fn of(&self, host: &str) -> HostIdentity {
        let (cert, key) = (
            self.dir.join(format!("{host}.crt")),
            self.dir.join(format!("{host}.key")),
        );
        if !cert.exists() {
            let made = shared_ca().issue_host(host);
            fs::write(&key, made.key_pem).unwrap();
            fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).unwrap();
            fs::write(&cert, made.cert_pem).unwrap();
        }
        HostIdentity::new(cert, key)
    }

    /// The flags that make an agent the host `host`: `--host-cert`, `--host-key` and
    /// `--ca-cert`.
    pub fn flags(&self, host: &str) -> Vec<String> {
        let identity = self.of(host);
        let text = |path: &Path| path.display().to_string();
        vec![
            "--host-cert".to_owned(),
            text(identity.cert_path()),
            "--host-key".to_owned(),
            text(identity.key_path()),
            "--ca-cert".to_owned(),
            text(&self.server_cert),
        ]
    }
}
