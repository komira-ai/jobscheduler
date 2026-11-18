//! The service identity: a client certificate presented in the TLS handshake, in place of
//! a bearer, and reloaded from its two files before each request.

use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use js_client::{Client, ClientError, SERVICE_CERT_FILE, SERVICE_KEY_FILE, ServiceIdentity};
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, ExtendedKeyUsagePurpose, IsCa, KeyPair,
};
use rustls::pki_types::CertificateDer;
use rustls::{ServerConfig, ServerConnection, StreamOwned};
use serde_json::json;

use crate::support::{Cert, Scratch, json_reply};

const LIMIT: Duration = Duration::from_secs(20);

/// A stand-in for the authority that issues service certificates.
struct Authority(CertifiedIssuer<'static, KeyPair>);

/// A certificate and key issued by an [`Authority`].
struct Minted {
    cert_pem: String,
    key_pem: String,
    der: Vec<u8>,
}

impl Authority {
    fn new() -> Self {
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        Self(CertifiedIssuer::self_signed(params, KeyPair::generate().unwrap()).unwrap())
    }

    fn der(&self) -> CertificateDer<'static> {
        self.0.der().clone()
    }

    /// A client certificate for `host` with a key of its own.
    fn issue(&self, host: &str) -> Minted {
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(vec![host.to_owned()]).unwrap();
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        let cert = params.signed_by(&key, &self.0).unwrap();
        Minted {
            cert_pem: cert.pem(),
            key_pem: key.serialize_pem(),
            der: cert.der().as_ref().to_vec(),
        }
    }
}

/// What the server saw of one connection.
#[derive(Debug)]
struct Seen {
    /// The DER of each certificate the client presented.
    presented: Vec<Vec<u8>>,
    /// The request head, empty when the handshake failed.
    head: String,
}

/// A TLS server on 127.0.0.1 that requires a client certificate and answers `connections`
/// connections with an empty JSON object.
struct Mutual {
    addr: SocketAddr,
    served: JoinHandle<Vec<Seen>>,
}

impl Mutual {
    fn start(config: Arc<ServerConfig>, connections: usize) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let served = thread::spawn(move || {
            (0..connections)
                .map(|_| {
                    let (tcp, _) = listener.accept().unwrap();
                    tcp.set_read_timeout(Some(LIMIT)).unwrap();
                    let conn = ServerConnection::new(Arc::clone(&config)).unwrap();
                    let mut stream = StreamOwned::new(conn, tcp);
                    let mut head = Vec::new();
                    let mut byte = [0_u8; 1];
                    while !head.ends_with(b"\r\n\r\n") {
                        match stream.read(&mut byte) {
                            Ok(1) => head.push(byte[0]),
                            _ => break,
                        }
                    }
                    let presented = stream
                        .conn
                        .peer_certificates()
                        .map(|certs| certs.iter().map(|der| der.as_ref().to_vec()).collect())
                        .unwrap_or_default();
                    let reply = json_reply(200, "{}");
                    let _ = stream.write_all(&reply);
                    let _ = stream.flush();
                    Seen {
                        presented,
                        head: String::from_utf8_lossy(&head).into_owned(),
                    }
                })
                .collect()
        });
        Self { addr, served }
    }

    fn url(&self) -> String {
        format!("https://{}", self.addr)
    }

    fn seen(self) -> Vec<Seen> {
        self.served.join().unwrap()
    }
}

/// The files of a state root holding `minted`, with `mode` on the key.
fn install(dir: &Scratch, minted: &Minted, mode: u32) -> ServiceIdentity {
    dir.write(SERVICE_CERT_FILE, minted.cert_pem.as_bytes(), 0o644);
    dir.write(SERVICE_KEY_FILE, minted.key_pem.as_bytes(), mode);
    ServiceIdentity::in_dir(&dir.path(""))
}

/// Replaces both files with `minted`'s the way a renewal does: each written aside and
/// renamed over the old one.
fn renew(dir: &Scratch, minted: &Minted) {
    replace(dir, SERVICE_CERT_FILE, minted.cert_pem.as_bytes(), 0o644);
    replace(dir, SERVICE_KEY_FILE, minted.key_pem.as_bytes(), 0o600);
}

/// Replaces one file with `bytes` by writing a new file aside and renaming it over.
fn replace(dir: &Scratch, name: &str, bytes: &[u8], mode: u32) {
    let aside = dir.write(&format!("{name}.new"), bytes, mode);
    fs::rename(aside, dir.path(name)).unwrap();
}

fn ca_file(dir: &Scratch, server: &Cert) -> std::path::PathBuf {
    dir.write("ca.pem", server.pem.as_bytes(), 0o644)
}

fn refused(connected: Result<Client, ClientError>) -> String {
    connected.err().expect("connect was refused").to_string()
}

/// Asserts that a client built on the identity files presents that certificate to a
/// server that requires one, on every call (get, post, post_within, delete, delete_with),
/// and sends no Authorization header. A cert-only server that sees a bearer on the
/// request would take it for the credential.
#[test]
fn a_client_presents_the_identity_and_sends_no_bearer() {
    let dir = Scratch::new("identity_presents");
    let authority = Authority::new();
    let minted = authority.issue("host-1");
    let identity = install(&dir, &minted, 0o600);
    let server = Cert::new();
    let ca = ca_file(&dir, &server);
    let fake = Mutual::start(server.requiring_clients_of(authority.der()), 5);
    let client = Client::connect_with_identity(&fake.url(), &identity, Some(&ca), LIMIT).unwrap();

    assert_eq!(client.get("/a").unwrap().status, 200);
    assert_eq!(client.post("/b", &json!({})).unwrap().status, 200);
    let within = client.post_within("/c", &json!({}), Duration::from_secs(5));
    assert_eq!(within.unwrap().status, 200);
    assert_eq!(client.delete("/d").unwrap().status, 200);
    let with_body = client.delete_with("/e", &json!({ "id": 1 }));
    assert_eq!(with_body.unwrap().status, 200);

    let seen = fake.seen();
    assert_eq!(seen.len(), 5);
    for connection in &seen {
        assert_eq!(connection.presented, vec![minted.der.clone()]);
        let head = connection.head.to_ascii_lowercase();
        assert!(!head.contains("authorization"), "{head}");
    }
}

/// Asserts that the rest of the chain in the certificate file is presented after the leaf.
#[test]
fn the_chain_in_the_certificate_file_is_presented() {
    let dir = Scratch::new("identity_chain");
    let authority = Authority::new();
    let mut minted = authority.issue("host-1");
    let extra = Authority::new();
    minted.cert_pem.push_str(&extra.0.pem());
    let identity = install(&dir, &minted, 0o400);
    let server = Cert::new();
    let ca = ca_file(&dir, &server);
    let fake = Mutual::start(server.requiring_clients_of(authority.der()), 1);
    let client = Client::connect_with_identity(&fake.url(), &identity, Some(&ca), LIMIT).unwrap();
    client.get("/a").unwrap();
    let seen = fake.seen();
    assert_eq!(
        seen[0].presented,
        vec![minted.der.clone(), extra.der().as_ref().to_vec()]
    );
}

/// Asserts that after both files are replaced by a renewed pair (a new key and
/// certificate), the next request of the same client presents the new certificate, and that
/// requests while the files are unchanged keep presenting the same one.
#[test]
fn a_renewed_pair_is_used_by_the_next_request_of_the_same_client() {
    let dir = Scratch::new("identity_renewal");
    let authority = Authority::new();
    let first = authority.issue("host-1");
    let second = authority.issue("host-1");
    let identity = install(&dir, &first, 0o600);
    let server = Cert::new();
    let ca = ca_file(&dir, &server);
    let fake = Mutual::start(server.requiring_clients_of(authority.der()), 3);
    let client = Client::connect_with_identity(&fake.url(), &identity, Some(&ca), LIMIT).unwrap();

    client.get("/a").unwrap();
    client.get("/b").unwrap();
    renew(&dir, &second);
    client.get("/c").unwrap();

    let presented: Vec<_> = fake.seen().into_iter().map(|seen| seen.presented).collect();
    assert_eq!(
        presented,
        vec![
            vec![first.der.clone()],
            vec![first.der.clone()],
            vec![second.der.clone()],
        ]
    );
}

/// Asserts that a half-replaced pair (the new certificate beside the old key, as between
/// the two renames of a renewal) leaves the client on the pair it had, that a pair whose
/// key file is gone does the same, and that once the second file lands the client switches.
#[test]
fn a_pair_that_does_not_hold_together_keeps_the_old_pair() {
    let dir = Scratch::new("identity_half");
    let authority = Authority::new();
    let first = authority.issue("host-1");
    let second = authority.issue("host-1");
    let identity = install(&dir, &first, 0o600);
    let server = Cert::new();
    let ca = ca_file(&dir, &server);
    let fake = Mutual::start(server.requiring_clients_of(authority.der()), 4);
    let client = Client::connect_with_identity(&fake.url(), &identity, Some(&ca), LIMIT).unwrap();

    replace(&dir, SERVICE_CERT_FILE, second.cert_pem.as_bytes(), 0o644);
    client.get("/new-cert-old-key").unwrap();
    fs::remove_file(dir.path(SERVICE_KEY_FILE)).unwrap();
    client.get("/no-key").unwrap();
    replace(&dir, SERVICE_KEY_FILE, b"not a key\n", 0o600);
    client.get("/garbage-key").unwrap();
    replace(&dir, SERVICE_KEY_FILE, second.key_pem.as_bytes(), 0o600);
    client.get("/both").unwrap();

    let presented: Vec<_> = fake.seen().into_iter().map(|seen| seen.presented).collect();
    assert_eq!(
        presented,
        vec![
            vec![first.der.clone()],
            vec![first.der.clone()],
            vec![first.der.clone()],
            vec![second.der.clone()],
        ]
    );
}

/// Asserts that a key file that a later renewal leaves with a loose mode is not taken up.
#[test]
fn a_renewed_key_with_a_loose_mode_keeps_the_old_pair() {
    let dir = Scratch::new("identity_loose_renewal");
    let authority = Authority::new();
    let first = authority.issue("host-1");
    let second = authority.issue("host-1");
    let identity = install(&dir, &first, 0o600);
    let server = Cert::new();
    let ca = ca_file(&dir, &server);
    let fake = Mutual::start(server.requiring_clients_of(authority.der()), 1);
    let client = Client::connect_with_identity(&fake.url(), &identity, Some(&ca), LIMIT).unwrap();
    replace(&dir, SERVICE_CERT_FILE, second.cert_pem.as_bytes(), 0o644);
    replace(&dir, SERVICE_KEY_FILE, second.key_pem.as_bytes(), 0o644);
    client.get("/a").unwrap();
    assert_eq!(fake.seen()[0].presented, vec![first.der.clone()]);
}

/// Asserts that a server that requires a certificate refuses a client with a certificate of
/// another authority, and that the client reports it as a server failure.
#[test]
fn a_certificate_of_another_authority_fails_the_handshake() {
    let dir = Scratch::new("identity_other_ca");
    let ours = Authority::new();
    let theirs = Authority::new();
    let identity = install(&dir, &theirs.issue("host-1"), 0o600);
    let server = Cert::new();
    let ca = ca_file(&dir, &server);
    let fake = Mutual::start(server.requiring_clients_of(ours.der()), 1);
    let client = Client::connect_with_identity(&fake.url(), &identity, Some(&ca), LIMIT).unwrap();
    let err = client.get("/a").err().unwrap();
    assert!(matches!(err, ClientError::Server(_)), "{err:?}");
    assert_eq!(fake.seen()[0].head, "");
}

/// Asserts that the identity files are refused at build, naming `--state-root` and the file:
/// a missing certificate, a missing key, a key with a loose mode, a certificate file with no
/// certificate in it, a key file with no key, a key that is not a usable key, and a key that
/// is not the certificate's. None of the messages holds the content of a file.
#[test]
fn unusable_identity_files_are_refused_naming_the_file_and_never_its_content() {
    let dir = Scratch::new("identity_refusals");
    let authority = Authority::new();
    let minted = authority.issue("host-1");
    let other = authority.issue("host-1");
    let identity = ServiceIdentity::in_dir(&dir.path(""));
    let cert = dir.path(SERVICE_CERT_FILE);
    let key = dir.path(SERVICE_KEY_FILE);
    let connect = || Client::connect_with_identity("https://localhost:1", &identity, None, LIMIT);

    let said = refused(connect());
    assert!(
        said.starts_with(&format!(
            "--state-root: {} cannot be read: ",
            cert.display()
        )),
        "{said}"
    );
    dir.write(SERVICE_CERT_FILE, minted.cert_pem.as_bytes(), 0o644);
    let said = refused(connect());
    assert!(
        said.starts_with(&format!("--state-root: {} cannot be read: ", key.display())),
        "{said}"
    );

    for mode in [0o640, 0o604, 0o700, 0o666] {
        dir.write(SERVICE_KEY_FILE, minted.key_pem.as_bytes(), mode);
        let said = refused(connect());
        let want = format!(
            "--state-root: {} must be a regular file owned by you and readable only by you (mode 0600 or 0400), not mode {mode:04o} owned by uid ",
            key.display()
        );
        assert!(said.starts_with(&want), "{said}");
        assert!(!said.contains("PRIVATE"), "{said}");
    }

    dir.write(SERVICE_KEY_FILE, minted.key_pem.as_bytes(), 0o600);
    dir.write(SERVICE_CERT_FILE, b"sekrit-bytes\n", 0o644);
    assert_eq!(
        refused(connect()),
        format!("--state-root: {} holds no certificate", cert.display())
    );
    dir.write(
        SERVICE_CERT_FILE,
        b"-----BEGIN CERTIFICATE-----\n!!!sekrit!!!\n-----END CERTIFICATE-----\n",
        0o644,
    );
    assert_eq!(
        refused(connect()),
        format!(
            "--state-root: {} does not hold PEM certificates",
            cert.display()
        )
    );
    dir.write(SERVICE_CERT_FILE, minted.cert_pem.as_bytes(), 0o644);

    dir.write(SERVICE_KEY_FILE, b"sekrit-bytes\n", 0o600);
    assert_eq!(
        refused(connect()),
        format!(
            "--state-root: {} does not hold a PEM private key",
            key.display()
        )
    );
    dir.write(
        SERVICE_KEY_FILE,
        b"-----BEGIN PRIVATE KEY-----\nc2Vrcml0LWJ5dGVz\n-----END PRIVATE KEY-----\n",
        0o600,
    );
    assert_eq!(
        refused(connect()),
        format!(
            "--state-root: {} holds a key of a kind that is not supported",
            key.display()
        )
    );
    dir.write(
        SERVICE_KEY_FILE,
        b"-----BEGIN PRIVATE KEY-----\nMAMCAQA=\n-----END PRIVATE KEY-----\n",
        0o600,
    );
    assert_eq!(
        refused(connect()),
        format!(
            "--state-root: {} holds a key of a kind that is not supported",
            key.display()
        )
    );

    dir.write(SERVICE_KEY_FILE, other.key_pem.as_bytes(), 0o600);
    assert_eq!(
        refused(connect()),
        format!(
            "--state-root: {} does not go with the certificate in {}",
            key.display(),
            cert.display()
        )
    );
    dir.write(
        SERVICE_CERT_FILE,
        b"-----BEGIN CERTIFICATE-----\nc2Vrcml0LWJ5dGVz\n-----END CERTIFICATE-----\n",
        0o644,
    );
    dir.write(SERVICE_KEY_FILE, minted.key_pem.as_bytes(), 0o600);
    assert_eq!(
        refused(connect()),
        format!(
            "--state-root: {} does not go with the certificate in {}",
            key.display(),
            cert.display()
        )
    );
}

/// Asserts that the server flag and the `--ca-cert` flag are checked before the identity
/// and named as for a token client, and that a state root's file that is a directory is
/// refused.
#[test]
fn the_other_flags_are_checked_for_an_identity_client_too() {
    let dir = Scratch::new("identity_flags");
    let identity = ServiceIdentity::in_dir(&dir.path(""));
    assert!(
        refused(Client::connect_with_identity(
            "http://example.com",
            &identity,
            None,
            LIMIT
        ))
        .starts_with("--server: ")
    );
    let missing = dir.path("missing.pem");
    let said = refused(Client::connect_with_identity(
        "https://localhost:1",
        &identity,
        Some(&missing),
        LIMIT,
    ));
    assert!(said.starts_with("--ca-cert: "), "{said}");

    fs::create_dir(dir.path(SERVICE_CERT_FILE)).unwrap();
    let said = refused(Client::connect_with_identity(
        "https://localhost:1",
        &identity,
        None,
        LIMIT,
    ));
    assert!(
        said.starts_with(&format!(
            "--state-root: {} cannot be read",
            dir.path(SERVICE_CERT_FILE).display()
        )),
        "{said}"
    );
}

/// Asserts that a client on the identity takes no bearer: `set_bearer` is refused by name
/// and the client still presents the identity.
#[test]
fn an_identity_client_takes_no_bearer() {
    let dir = Scratch::new("identity_no_bearer");
    let authority = Authority::new();
    let minted = authority.issue("host-1");
    let identity = install(&dir, &minted, 0o600);
    let server = Cert::new();
    let ca = ca_file(&dir, &server);
    let fake = Mutual::start(server.requiring_clients_of(authority.der()), 1);
    let mut client =
        Client::connect_with_identity(&fake.url(), &identity, Some(&ca), LIMIT).unwrap();
    assert_eq!(
        client.set_bearer("host-cred").unwrap_err().to_string(),
        "--token-file: the client authenticates by its service identity and takes no bearer"
    );
    client.get("/a").unwrap();
    let seen = fake.seen();
    assert_eq!(seen[0].presented, vec![minted.der.clone()]);
    assert!(!seen[0].head.to_ascii_lowercase().contains("authorization"));
}

/// Asserts that a state root with both files has an identity, with the paths of the two
/// files, and that one with neither has none.
#[test]
fn a_state_root_with_both_files_holds_the_identity() {
    let dir = Scratch::new("identity_found");
    assert_eq!(ServiceIdentity::found_in(&dir.path("")).unwrap(), None);
    let minted = Authority::new().issue("host-1");
    install(&dir, &minted, 0o600);
    let found = ServiceIdentity::found_in(&dir.path("")).unwrap().unwrap();
    assert_eq!(found.cert_path(), dir.path(SERVICE_CERT_FILE));
    assert_eq!(found.key_path(), dir.path(SERVICE_KEY_FILE));
}

/// Asserts that a state root with only one of the two files is an error naming the file
/// that is missing, in both directions, and not a silent fall back to none.
#[test]
fn a_state_root_with_one_file_names_the_missing_one() {
    let dir = Scratch::new("identity_half_present");
    let minted = Authority::new().issue("host-1");
    dir.write(SERVICE_CERT_FILE, minted.cert_pem.as_bytes(), 0o644);
    let said = ServiceIdentity::found_in(&dir.path(""))
        .unwrap_err()
        .to_string();
    assert_eq!(
        said,
        format!(
            "--state-root: {} is missing: the service identity needs both service-identity.pem and service-identity.key",
            dir.path(SERVICE_KEY_FILE).display()
        )
    );
    fs::remove_file(dir.path(SERVICE_CERT_FILE)).unwrap();
    dir.write(SERVICE_KEY_FILE, minted.key_pem.as_bytes(), 0o600);
    let said = ServiceIdentity::found_in(&dir.path(""))
        .unwrap_err()
        .to_string();
    assert!(
        said.starts_with(&format!(
            "--state-root: {} is missing: ",
            dir.path(SERVICE_CERT_FILE).display()
        )),
        "{said}"
    );
    assert!(!said.contains("PRIVATE"), "{said}");
}
