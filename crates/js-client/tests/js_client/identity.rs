//! The host identity: the host's client certificate presented in the TLS handshake, in
//! place of a bearer, and reloaded from its two files before each request.

use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use js_client::{Client, ClientError, HostIdentity, host_key_problem};
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, ExtendedKeyUsagePurpose, IsCa, KeyPair,
};
use rustls::pki_types::CertificateDer;
use rustls::{ServerConfig, ServerConnection, StreamOwned};
use serde_json::json;

use crate::support::{Cert, Scratch, json_reply};

const LIMIT: Duration = Duration::from_secs(20);

/// The certificate file of the identities these tests install.
const CERT_FILE: &str = "host.pem";

/// The key file of the identities these tests install.
const KEY_FILE: &str = "host.key";

/// A stand-in for the authority that issues host certificates.
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

/// The identity files of `dir` holding `minted`, with `mode` on the key.
fn install(dir: &Scratch, minted: &Minted, mode: u32) -> HostIdentity {
    dir.write(CERT_FILE, minted.cert_pem.as_bytes(), 0o644);
    dir.write(KEY_FILE, minted.key_pem.as_bytes(), mode);
    HostIdentity::new(dir.path(CERT_FILE), dir.path(KEY_FILE))
}

/// Replaces both files with `minted`'s the way a renewal does: each written aside and
/// renamed over the old one.
fn renew(dir: &Scratch, minted: &Minted) {
    replace(dir, CERT_FILE, minted.cert_pem.as_bytes(), 0o644);
    replace(dir, KEY_FILE, minted.key_pem.as_bytes(), 0o640);
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

    replace(&dir, CERT_FILE, second.cert_pem.as_bytes(), 0o644);
    client.get("/new-cert-old-key").unwrap();
    fs::remove_file(dir.path(KEY_FILE)).unwrap();
    client.get("/no-key").unwrap();
    replace(&dir, KEY_FILE, b"not a key\n", 0o600);
    client.get("/garbage-key").unwrap();
    replace(&dir, KEY_FILE, second.key_pem.as_bytes(), 0o600);
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

/// Asserts that a renewed key that others can read is not taken up: the client keeps the pair it had.
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
    replace(&dir, CERT_FILE, second.cert_pem.as_bytes(), 0o644);
    replace(&dir, KEY_FILE, second.key_pem.as_bytes(), 0o644);
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

/// Asserts that the identity files are refused at build, naming `--host-cert` or `--host-key`
/// and the file: a missing certificate, a missing key, a certificate file with no
/// certificate in it, a key file with no key, a key that is not a usable key, and a key that
/// is not the certificate's. None of the messages holds the content of a file.
#[test]
fn unusable_identity_files_are_refused_naming_the_file_and_never_its_content() {
    let dir = Scratch::new("identity_refusals");
    let authority = Authority::new();
    let minted = authority.issue("host-1");
    let other = authority.issue("host-1");
    let identity = HostIdentity::new(dir.path(CERT_FILE), dir.path(KEY_FILE));
    let cert = dir.path(CERT_FILE);
    let key = dir.path(KEY_FILE);
    let connect = || Client::connect_with_identity("https://localhost:1", &identity, None, LIMIT);

    let said = refused(connect());
    assert!(
        said.starts_with(&format!("--host-cert: {} cannot be read: ", cert.display())),
        "{said}"
    );
    dir.write(CERT_FILE, minted.cert_pem.as_bytes(), 0o644);
    let said = refused(connect());
    assert!(
        said.starts_with(&format!("--host-key: {} cannot be read: ", key.display())),
        "{said}"
    );

    dir.write(KEY_FILE, minted.key_pem.as_bytes(), 0o600);
    dir.write(CERT_FILE, b"sekrit-bytes\n", 0o644);
    assert_eq!(
        refused(connect()),
        format!("--host-cert: {} holds no certificate", cert.display())
    );
    dir.write(
        CERT_FILE,
        b"-----BEGIN CERTIFICATE-----\n!!!sekrit!!!\n-----END CERTIFICATE-----\n",
        0o644,
    );
    assert_eq!(
        refused(connect()),
        format!(
            "--host-cert: {} does not hold PEM certificates",
            cert.display()
        )
    );
    dir.write(CERT_FILE, minted.cert_pem.as_bytes(), 0o644);

    dir.write(KEY_FILE, b"sekrit-bytes\n", 0o600);
    assert_eq!(
        refused(connect()),
        format!(
            "--host-key: {} does not hold a PEM private key",
            key.display()
        )
    );
    let unsupported = format!(
        "--host-key: {} holds a key of a kind that is not supported",
        key.display()
    );
    dir.write(
        KEY_FILE,
        b"-----BEGIN PRIVATE KEY-----\nc2Vrcml0LWJ5dGVz\n-----END PRIVATE KEY-----\n",
        0o600,
    );
    assert_eq!(refused(connect()), unsupported);
    dir.write(
        KEY_FILE,
        b"-----BEGIN PRIVATE KEY-----\nMAMCAQA=\n-----END PRIVATE KEY-----\n",
        0o600,
    );
    assert_eq!(refused(connect()), unsupported);

    let mismatch = format!(
        "--host-key: {} does not go with the certificate in {}",
        key.display(),
        cert.display()
    );
    dir.write(KEY_FILE, other.key_pem.as_bytes(), 0o600);
    assert_eq!(refused(connect()), mismatch);
    dir.write(
        CERT_FILE,
        b"-----BEGIN CERTIFICATE-----\nc2Vrcml0LWJ5dGVz\n-----END CERTIFICATE-----\n",
        0o644,
    );
    dir.write(KEY_FILE, minted.key_pem.as_bytes(), 0o600);
    assert_eq!(refused(connect()), mismatch);
}

/// Asserts that a key file the group can read (0640, 0440) is accepted beside the owner-only
/// modes (0600, 0400): the client connects and presents the certificate.
#[test]
fn a_key_readable_by_its_group_is_accepted() {
    let authority = Authority::new();
    for mode in [0o600, 0o400, 0o640, 0o440] {
        let dir = Scratch::new(&format!("identity_mode_{mode:o}"));
        let minted = authority.issue("host-1");
        let identity = install(&dir, &minted, mode);
        let server = Cert::new();
        let ca = ca_file(&dir, &server);
        let fake = Mutual::start(server.requiring_clients_of(authority.der()), 1);
        let client =
            Client::connect_with_identity(&fake.url(), &identity, Some(&ca), LIMIT).unwrap();
        client.get("/a").unwrap();
        assert_eq!(
            fake.seen()[0].presented,
            vec![minted.der.clone()],
            "{mode:o}"
        );
    }
}

/// Asserts that a key file that others can read or that its group can write (0644, 0660,
/// 0604, 0666) is refused at build with the full message naming `--host-key` and the file,
/// and that a key that is a directory is refused as not a regular file.
#[test]
fn a_key_that_others_can_use_or_its_group_can_write_is_refused() {
    let dir = Scratch::new("identity_loose_modes");
    let minted = Authority::new().issue("host-1");
    let identity = HostIdentity::new(dir.path(CERT_FILE), dir.path(KEY_FILE));
    let key = dir.path(KEY_FILE);
    dir.write(CERT_FILE, minted.cert_pem.as_bytes(), 0o644);
    let connect = || Client::connect_with_identity("https://localhost:1", &identity, None, LIMIT);

    for mode in [0o644, 0o604, 0o666] {
        dir.write(KEY_FILE, minted.key_pem.as_bytes(), mode);
        assert_eq!(
            refused(connect()),
            format!(
                "--host-key: {} must give others no access (mode 0600 or 0640), not mode {mode:04o}",
                key.display()
            )
        );
    }
    dir.write(KEY_FILE, minted.key_pem.as_bytes(), 0o660);
    assert_eq!(
        refused(connect()),
        format!(
            "--host-key: {} must not be writable by its group (mode 0600 or 0640), not mode 0660",
            key.display()
        )
    );
    fs::remove_file(&key).unwrap();
    fs::create_dir(&key).unwrap();
    assert_eq!(
        refused(connect()),
        format!("--host-key: {} is not a regular file", key.display())
    );
}

/// Asserts the owner rule of the key: a regular file owned by root or by the user running the
/// program passes with mode 0600 or 0640, one owned by another user is refused whatever its
/// mode, and a file that is not regular is refused first.
#[test]
fn a_key_owned_by_another_user_is_refused() {
    let me = 1000;
    assert_eq!(host_key_problem(true, 0o640, 0, me), None);
    assert_eq!(host_key_problem(true, 0o600, me, me), None);
    assert_eq!(
        host_key_problem(true, 0o600, 1001, me).as_deref(),
        Some("must be owned by root or by uid 1000, not by uid 1001")
    );
    assert_eq!(
        host_key_problem(false, 0o600, 1001, me).as_deref(),
        Some("is not a regular file")
    );
}

/// Asserts that the pair is followed through symbolic links, the way hostkeeper renews it:
/// both names are links to a directory of one generation, and after the links are repointed
/// to the next generation the next request of the same client presents the new certificate.
#[test]
fn a_renewal_that_repoints_symbolic_links_is_used_by_the_next_request() {
    let dir = Scratch::new("identity_symlinks");
    let authority = Authority::new();
    let first = authority.issue("host-1");
    let second = authority.issue("host-1");
    for (generation, minted) in [("gen1", &first), ("gen2", &second)] {
        fs::create_dir(dir.path(generation)).unwrap();
        dir.write(
            &format!("{generation}/cert.pem"),
            minted.cert_pem.as_bytes(),
            0o644,
        );
        dir.write(
            &format!("{generation}/key.pem"),
            minted.key_pem.as_bytes(),
            0o640,
        );
    }
    let point = |generation: &str| {
        for (name, target) in [(CERT_FILE, "cert.pem"), (KEY_FILE, "key.pem")] {
            let aside = dir.path(&format!("{name}.link"));
            let _ = fs::remove_file(&aside);
            std::os::unix::fs::symlink(dir.path(generation).join(target), &aside).unwrap();
            fs::rename(&aside, dir.path(name)).unwrap();
        }
    };
    point("gen1");
    let identity = HostIdentity::new(dir.path(CERT_FILE), dir.path(KEY_FILE));
    let server = Cert::new();
    let ca = ca_file(&dir, &server);
    let fake = Mutual::start(server.requiring_clients_of(authority.der()), 2);
    let client = Client::connect_with_identity(&fake.url(), &identity, Some(&ca), LIMIT).unwrap();
    client.get("/a").unwrap();
    point("gen2");
    client.get("/b").unwrap();
    let presented: Vec<_> = fake.seen().into_iter().map(|seen| seen.presented).collect();
    assert_eq!(
        presented,
        vec![vec![first.der.clone()], vec![second.der.clone()]]
    );
}

/// Asserts that the server flag and the `--ca-cert` flag are checked before the identity
/// and named as for a token client, and that a certificate file that is a directory is
/// refused.
#[test]
fn the_other_flags_are_checked_for_an_identity_client_too() {
    let dir = Scratch::new("identity_flags");
    let identity = HostIdentity::new(dir.path(CERT_FILE), dir.path(KEY_FILE));
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

    fs::create_dir(dir.path(CERT_FILE)).unwrap();
    let said = refused(Client::connect_with_identity(
        "https://localhost:1",
        &identity,
        None,
        LIMIT,
    ));
    assert_eq!(
        said,
        format!(
            "--host-cert: {} is not a regular file",
            dir.path(CERT_FILE).display()
        )
    );
}

/// Asserts that an identity client is refused a server that is plain `http://`, even a
/// loopback one that a token client may use, naming `--server` and saying the certificate is
/// presented over `https://` only; the message is the same whatever the files hold.
#[test]
fn an_identity_client_is_refused_a_plain_http_server() {
    let dir = Scratch::new("identity_plain_http");
    let identity = HostIdentity::new(dir.path(CERT_FILE), dir.path(KEY_FILE));
    let said = refused(Client::connect_with_identity(
        "http://127.0.0.1:1",
        &identity,
        None,
        LIMIT,
    ));
    assert_eq!(
        said,
        "--server: the host identity is a client certificate, presented only over https://"
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
        "--token-file: the client authenticates by its host identity and takes no bearer"
    );
    client.get("/a").unwrap();
    let seen = fake.seen();
    assert_eq!(seen[0].presented, vec![minted.der.clone()]);
    assert!(!seen[0].head.to_ascii_lowercase().contains("authorization"));
}

/// A named pipe at `path`, which a blocking open would wait on forever. `mkfifo`, not rustix's
/// `mknodat`: rustix has no `mknodat` (nor `mkfifoat`) on macOS, where these tests also run.
fn make_fifo(path: &std::path::Path) {
    nix::unistd::mkfifo(path, nix::sys::stat::Mode::from_bits_truncate(0o600)).unwrap();
}

/// Asserts that a certificate file that is a named pipe is refused at connect, without
/// waiting for a writer, with the full message naming `--host-cert` and the file.
#[test]
fn a_certificate_that_is_a_named_pipe_is_refused_without_blocking() {
    let dir = Scratch::new("identity_cert_fifo");
    let minted = Authority::new().issue("host-1");
    let identity = HostIdentity::new(dir.path(CERT_FILE), dir.path(KEY_FILE));
    dir.write(KEY_FILE, minted.key_pem.as_bytes(), 0o600);
    make_fifo(&dir.path(CERT_FILE));
    assert_eq!(
        refused(Client::connect_with_identity(
            "https://localhost:1",
            &identity,
            None,
            LIMIT
        )),
        format!(
            "--host-cert: {} is not a regular file",
            dir.path(CERT_FILE).display()
        )
    );
}

/// Asserts that a certificate file larger than 65536 bytes is refused at connect with the
/// full message naming `--host-cert`, and one of exactly that size is read (and then
/// refused only for what it holds).
#[test]
fn a_certificate_file_larger_than_the_limit_is_refused() {
    let dir = Scratch::new("identity_cert_oversize");
    let minted = Authority::new().issue("host-1");
    let identity = HostIdentity::new(dir.path(CERT_FILE), dir.path(KEY_FILE));
    dir.write(KEY_FILE, minted.key_pem.as_bytes(), 0o600);
    let connect = || Client::connect_with_identity("https://localhost:1", &identity, None, LIMIT);
    dir.write(CERT_FILE, &vec![b'#'; 65 * 1024], 0o644);
    assert_eq!(
        refused(connect()),
        format!(
            "--host-cert: {} is larger than 65536 bytes",
            dir.path(CERT_FILE).display()
        )
    );
    dir.write(CERT_FILE, &vec![b'#'; 64 * 1024], 0o644);
    assert_eq!(
        refused(connect()),
        format!(
            "--host-cert: {} holds no certificate",
            dir.path(CERT_FILE).display()
        )
    );
}

/// Asserts that a renewal that leaves the certificate path a named pipe keeps the pair the
/// client had, and the request does not wait for a writer.
#[test]
fn a_renewal_whose_certificate_becomes_a_named_pipe_keeps_the_old_pair() {
    let dir = Scratch::new("identity_cert_fifo_renewal");
    let authority = Authority::new();
    let first = authority.issue("host-1");
    let second = authority.issue("host-1");
    let identity = install(&dir, &first, 0o600);
    let server = Cert::new();
    let ca = ca_file(&dir, &server);
    let fake = Mutual::start(server.requiring_clients_of(authority.der()), 1);
    let client = Client::connect_with_identity(&fake.url(), &identity, Some(&ca), LIMIT).unwrap();
    fs::remove_file(dir.path(CERT_FILE)).unwrap();
    make_fifo(&dir.path(CERT_FILE));
    replace(&dir, KEY_FILE, second.key_pem.as_bytes(), 0o600);
    client.get("/a").unwrap();
    assert_eq!(fake.seen()[0].presented, vec![first.der.clone()]);
}
