//! TLS against a real server. The test server speaks no TLS, so each test puts a front in
//! front of it: a loopback listener (port 0) that answers the client's TLS request as the test
//! asks (yes with a certificate it is given, or no), then passes the bytes on to the test
//! server unchanged. What the store meets is what a TLS-terminating Postgres sends: the
//! answer to the request, the handshake, then the server's own protocol.

use std::io;
use std::sync::{Arc, mpsc};

use js_state::postgres::{
    ConfigError, ExtensionPool, OpenError, PostgresConfig, PostgresStore, TestDatabase, TlsError,
    TlsMode, Unreached, reach, schema, test_database,
};
use js_state::{LockRefused, Store, StoreError, WriterLock, WriterTarget};
use js_testkit::dir::TestDir;
use postgres::config::Host;
use rcgen::{
    CertificateParams, CertifiedIssuer, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair,
};
use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::io::{AsyncReadExt, AsyncWriteExt, copy_bidirectional};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;

use super::{put, url};

/// The client's TLS request: length 8, code 80877103.
const SSL_REQUEST: [u8; 8] = [0, 0, 0, 8, 0x04, 0xd2, 0x16, 0x2f];

/// How the front answers the TLS request.
#[derive(Clone)]
enum Offer {
    /// Yes, then the handshake with this configuration.
    Tls(Arc<ServerConfig>),
    /// No: the connection goes on plain.
    Plain,
    /// Yes, the handshake, and then, as the server, a request for SCRAM (with and without
    /// channel binding) in answer to the startup message; the client's first SASL message
    /// (its mechanism, and the start of its data) is sent on the channel, and the connection
    /// ends there.
    Scram(Arc<ServerConfig>, mpsc::Sender<(String, Vec<u8>)>),
}

/// A front to the test server on a loopback port of its own; it stops when dropped.
struct Front {
    port: u16,
    _runtime: tokio::runtime::Runtime,
}

impl Front {
    fn start(offer: Offer) -> Front {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_io()
            .build()
            .expect("a runtime");
        let backend = backend();
        let listener = runtime
            .block_on(TcpListener::bind("127.0.0.1:0"))
            .expect("a loopback port");
        let port = listener.local_addr().expect("the address").port();
        runtime.spawn(async move {
            while let Ok((client, _)) = listener.accept().await {
                let offer = offer.clone();
                let backend = backend.clone();
                // A connection the client gives up on (a refused certificate) ends here.
                tokio::spawn(async move {
                    let _ = relay(client, offer, backend).await;
                });
            }
        });
        Front {
            port,
            _runtime: runtime,
        }
    }
}

async fn relay(mut client: TcpStream, offer: Offer, backend: (String, u16)) -> io::Result<()> {
    let mut first = [0; 8];
    client.read_exact(&mut first).await?;
    let mut server = TcpStream::connect((backend.0.as_str(), backend.1)).await?;
    if first != SSL_REQUEST {
        server.write_all(&first).await?;
        copy_bidirectional(&mut client, &mut server).await?;
        return Ok(());
    }
    match offer {
        Offer::Plain => {
            client.write_all(b"N").await?;
            copy_bidirectional(&mut client, &mut server).await?;
        }
        Offer::Tls(config) => {
            client.write_all(b"S").await?;
            let mut tls = TlsAcceptor::from(config).accept(client).await?;
            copy_bidirectional(&mut tls, &mut server).await?;
        }
        Offer::Scram(config, seen) => {
            client.write_all(b"S").await?;
            let mut tls = TlsAcceptor::from(config).accept(client).await?;
            let startup = tls.read_u32().await? as usize;
            let mut body = vec![0; startup - 4];
            tls.read_exact(&mut body).await?;
            // AuthenticationSASL (code 10): the mechanisms, each NUL-ended, then a NUL.
            let mechanisms = b"SCRAM-SHA-256-PLUS\0SCRAM-SHA-256\0\0";
            let length = u32::try_from(4 + 4 + mechanisms.len()).expect("short");
            let mut ask = vec![b'R'];
            ask.extend_from_slice(&length.to_be_bytes());
            ask.extend_from_slice(&10_u32.to_be_bytes());
            ask.extend_from_slice(mechanisms);
            tls.write_all(&ask).await?;
            // SASLInitialResponse: 'p', a length, the mechanism (NUL-ended), the data.
            let _tag = tls.read_u8().await?;
            let length = tls.read_u32().await? as usize;
            let mut message = vec![0; length - 4];
            tls.read_exact(&mut message).await?;
            let end = message.iter().position(|b| *b == 0).expect("a mechanism");
            let mechanism = String::from_utf8_lossy(&message[..end]).into_owned();
            let data = message[end + 1 + 4..].to_vec();
            let _ = seen.send((mechanism, data));
        }
    }
    Ok(())
}

/// The host and port of the test server, which the TLS tests need on TCP.
fn backend() -> (String, u16) {
    let pg: postgres::Config = url().parse().expect("the test server URL parses");
    let host = match pg.get_hosts().first() {
        Some(Host::Tcp(host)) => host.clone(),
        other => panic!("the TLS tests need the test server on TCP, not {other:?}"),
    };
    (host, pg.get_ports().first().copied().unwrap_or(5432))
}

/// `url` with its host and port the front's (127.0.0.1, so no name is resolved), and `query`
/// added to its query.
fn through(url: &str, port: u16, query: &str) -> String {
    let (scheme, rest) = url.split_once("://").expect("a URL");
    let (user, rest) = rest
        .split_once('@')
        .map_or(("", rest), |(user, rest)| (user, rest));
    let at = if user.is_empty() { "" } else { "@" };
    let path = rest.find(['/', '?']).map_or("", |start| &rest[start..]);
    let join = if path.contains('?') { '&' } else { '?' };
    format!("{scheme}://{user}{at}127.0.0.1:{port}{path}{join}{query}")
}

/// A certificate authority of its own, named `name`.
fn named_authority(name: &str) -> CertifiedIssuer<'static, KeyPair> {
    let mut params = CertificateParams::new(Vec::<String>::new()).expect("CA params");
    params.is_ca = IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    params.distinguished_name = DistinguishedName::new();
    params.distinguished_name.push(DnType::CommonName, name);
    CertifiedIssuer::self_signed(params, KeyPair::generate().expect("a key")).expect("a CA")
}

/// A certificate authority of its own, with the name every test's root has.
fn authority() -> CertifiedIssuer<'static, KeyPair> {
    named_authority("js-state test root")
}

/// A server configuration with a certificate for `name` signed by `ca`, for `purpose`,
/// speaking TLS 1.2 only when `tls12` (else every version rustls offers).
fn server(
    ca: &CertifiedIssuer<'static, KeyPair>,
    name: &str,
    purpose: ExtendedKeyUsagePurpose,
    tls12: bool,
) -> Arc<ServerConfig> {
    let key = KeyPair::generate().expect("a key");
    let mut params = CertificateParams::new(vec![name.to_owned()]).expect("params");
    params.extended_key_usages = vec![purpose];
    let cert = params.signed_by(&key, ca).expect("signs");
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let versions: &[&rustls::SupportedProtocolVersion] = if tls12 {
        &[&rustls::version::TLS12]
    } else {
        rustls::DEFAULT_VERSIONS
    };
    let chain: Vec<CertificateDer<'static>> = vec![cert.der().clone()];
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der()));
    Arc::new(
        ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(versions)
            .expect("versions")
            .with_no_client_auth()
            .with_single_cert(chain, key)
            .expect("a server configuration"),
    )
}

/// A TLS front for a server certificate of `name` signed by `ca`.
fn tls_front(ca: &CertifiedIssuer<'static, KeyPair>, name: &str, tls12: bool) -> Front {
    Front::start(Offer::Tls(server(
        ca,
        name,
        ExtendedKeyUsagePurpose::ServerAuth,
        tls12,
    )))
}

/// The PEM file of `ca`'s certificate, in a directory of the test's own.
fn root_file(dir: &TestDir, ca: &CertifiedIssuer<'static, KeyPair>) -> String {
    let path = dir.path().join("root.pem");
    std::fs::write(&path, ca.pem()).expect("writes the root certificate");
    path.display().to_string()
}

/// The store configuration for `database` through `front`, with `query` (sslmode...).
fn config(database: &TestDatabase, front: &Front, query: &str) -> PostgresConfig {
    PostgresConfig::parse(&through(&database.url(), front.port, query)).expect("parses")
}

fn open(config: &PostgresConfig) -> Result<PostgresStore, OpenError> {
    PostgresStore::try_open(config)
}

fn scratch(name: &str) -> TestDir {
    TestDir::under(env!("CARGO_TARGET_TMPDIR"), &format!("js-state-tls-{name}"))
}

// Catches `verify-full` that does not check the name in the certificate (a server with a
// certificate for another host, from the same authority, would be taken for the store's),
// a name mismatch reported as anything but its own typed error, and `verify-full` that cannot
// connect at all: with the matching name the store opens, writes and reads, and the
// installer's runner, the writer lock and the login check connect the same way. Before this
// change every TLS sslmode was refused when the URL was read.
#[test]
fn verify_full_refuses_a_certificate_for_another_name_and_takes_the_matching_one() {
    let database = test_database();
    let dir = scratch("verify-full");
    let ca = authority();
    let root = root_file(&dir, &ca);
    let query = format!("sslmode=verify-full&sslrootcert={root}");

    let other = tls_front(&ca, "db.other.invalid", false);
    let refused = open(&config(&database, &other, &query)).err();
    assert_eq!(refused, Some(OpenError::Tls(TlsError::NameMismatch)));

    let matching = tls_front(&ca, "127.0.0.1", false);
    let config = config(&database, &matching, &query);
    assert_eq!(config.tls_mode(), TlsMode::VerifyFull);
    let store = open(&config).expect("opens over TLS");
    store.txn(put("k")).expect("writes").expect("commits");
    assert!(store.get("k").expect("reads").is_some());
    assert_eq!(reach(&config), Ok(()));
    assert_eq!(schema::current(&config), Ok(schema::latest()));
    schema::apply(&config, &schema::embedded(), schema::latest()).expect("the runner connects");
    let lock = WriterLock::acquire(&WriterTarget::Postgres(Box::new(config)));
    assert!(lock.is_ok(), "{lock:?}");
}

// Catches `verify-ca` that checks the name (it would refuse a server whose certificate names
// another host, which is what verify-ca allows) or does not check the chain: a certificate
// from another authority is refused, and so is one from an impostor that took the root's
// name, both as `Untrusted`. The server speaks TLS 1.2 only, so the handshake signature of
// that version is checked too.
#[test]
fn verify_ca_takes_another_name_and_refuses_another_authority() {
    let database = test_database();
    let dir = scratch("verify-ca");
    let ca = authority();
    let query = format!("sslmode=verify-ca&sslrootcert={}", root_file(&dir, &ca));

    let named_otherwise = tls_front(&ca, "db.other.invalid", true);
    let store = open(&config(&database, &named_otherwise, &query)).expect("opens");
    store.txn(put("k")).expect("writes").expect("commits");

    for impostor in [named_authority("another root"), authority()] {
        let front = tls_front(&impostor, "127.0.0.1", true);
        let refused = open(&config(&database, &front, &query)).err();
        assert_eq!(refused, Some(OpenError::Tls(TlsError::Untrusted)));
    }
}

// Catches `require` downgraded to a plain connection against a server that answers "no" to
// TLS (the password and every row would cross the network in the clear), the refusal
// reported as anything but its own typed error by the store, the login check, the
// installer's runner and the writer lock, and `prefer` that does not fall back to plain.
#[test]
fn require_refuses_a_server_without_tls_and_prefer_connects_plain() {
    let database = test_database();
    let plain = Front::start(Offer::Plain);
    let required = config(&database, &plain, "sslmode=require");
    assert_eq!(
        open(&required).err(),
        Some(OpenError::Tls(TlsError::NotOffered))
    );
    assert_eq!(reach(&required), Err(Unreached::Tls(TlsError::NotOffered)));
    assert_eq!(
        schema::current(&required),
        Err(schema::SchemaError::Database(
            TlsError::NotOffered.to_string()
        ))
    );
    assert_eq!(
        WriterLock::acquire(&WriterTarget::Postgres(Box::new(required.clone()))).err(),
        Some(LockRefused::Failed(TlsError::NotOffered.to_string()))
    );
    assert_eq!(
        ExtensionPool::open(&required).err(),
        Some(StoreError::Failed(TlsError::NotOffered.to_string()))
    );
    assert_eq!(
        OpenError::Tls(TlsError::NotOffered).into_store(),
        StoreError::Failed(
            "the server does not offer TLS, and the state store's sslmode requires it".to_owned()
        )
    );

    let prefer = config(&database, &plain, "sslmode=prefer");
    let store = open(&prefer).expect("prefer connects plain");
    store.txn(put("k")).expect("writes").expect("commits");
}

// Catches `require` (and `prefer`) that check the certificate: a server with a certificate
// from an authority the store does not know is taken, with or without a root certificate
// named, and a root certificate read by a mode that does not check one (a file that is not
// there would refuse the configuration).
#[test]
fn require_takes_any_certificate() {
    let database = test_database();
    let dir = scratch("require");
    let front = tls_front(&authority(), "db.other.invalid", false);
    let unrelated = dir.path().join("absent.pem").display().to_string();
    for query in [
        "sslmode=require".to_owned(),
        format!("sslmode=require&sslrootcert={unrelated}"),
        "sslmode=prefer".to_owned(),
    ] {
        let store = open(&config(&database, &front, &query)).expect("opens");
        store.txn(put("k")).expect("writes").expect("commits");
    }
}

// Catches a certificate refusal other than the name or the authority (here a certificate
// that is for clients, not servers) that is lost or reported as a name mismatch.
#[test]
fn a_certificate_that_is_not_for_servers_is_a_handshake_error() {
    let database = test_database();
    let dir = scratch("purpose");
    let ca = authority();
    let query = format!("sslmode=verify-full&sslrootcert={}", root_file(&dir, &ca));
    let front = Front::start(Offer::Tls(server(
        &ca,
        "127.0.0.1",
        ExtendedKeyUsagePurpose::ClientAuth,
        false,
    )));
    let refused = open(&config(&database, &front, &query)).err();
    assert!(
        matches!(refused, Some(OpenError::Tls(TlsError::Handshake(_)))),
        "{refused:?}"
    );
}

// Catches a server's refusal after the handshake (here: no such database) taken for a TLS
// failure, and a connection that cannot be made at all (nothing listens) taken for a server
// without TLS: neither is a TLS error, and the second is a server that is not up, and to the
// login check no server.
#[test]
fn failures_that_are_not_the_handshakes_keep_their_kind() {
    let database = test_database();
    let front = tls_front(&authority(), "127.0.0.1", false);
    let url = through(&database.url(), front.port, "sslmode=require");
    let missing = js_testkit::database::with_database(&url, "js_state_tls_no_such_database");
    let refused = open(&PostgresConfig::parse(&missing).expect("parses")).err();
    assert!(
        matches!(refused, Some(OpenError::Refused(_))),
        "{refused:?}"
    );

    let closed = std::net::TcpListener::bind("127.0.0.1:0").expect("a loopback port");
    let port = closed.local_addr().expect("the address").port();
    drop(closed);
    let nobody = through(&database.url(), port, "sslmode=require");
    let refused = open(&PostgresConfig::parse(&nobody).expect("parses")).err();
    assert!(
        refused.as_ref().is_some_and(OpenError::is_not_up),
        "{refused:?}"
    );
    let nobody = PostgresConfig::parse(&nobody).expect("parses");
    assert_eq!(reach(&nobody), Err(Unreached::NoServer));
}

// Catches a root certificate file that cannot be used taken without one (a `verify` mode
// would then trust nothing, or everything), and an error that does not name the file.
#[test]
fn a_root_certificate_file_that_cannot_be_used_is_refused() {
    let dir = scratch("root-file");
    let missing = dir.path().join("absent.pem");
    let empty = dir.path().join("empty.pem");
    std::fs::write(&empty, "not a certificate\n").expect("writes");
    let parse = |path: &std::path::Path| {
        PostgresConfig::parse(&format!(
            "postgres://u@127.0.0.1/db?sslmode=verify-full&sslrootcert={}",
            path.display()
        ))
        .err()
    };
    let Some(ConfigError::RootCert(why)) = parse(&missing) else {
        panic!("a missing file is refused");
    };
    assert!(why.starts_with(&missing.display().to_string()), "{why}");
    assert_eq!(
        parse(&empty),
        Some(ConfigError::RootCert(format!(
            "{}: holds no certificate",
            empty.display()
        )))
    );
}

// Catches a TLS connection that claims channel binding it does not provide (a server that
// requires SCRAM-SHA-256-PLUS would be told the client binds to the TLS session when it
// cannot prove it): offered both, the client picks plain SCRAM-SHA-256 and says, in the
// first bytes of its message (`n,,`), that it does not bind.
#[test]
fn over_tls_the_client_offers_no_channel_binding() {
    let (seen, said) = mpsc::channel();
    let front = Front::start(Offer::Scram(
        server(
            &authority(),
            "127.0.0.1",
            ExtendedKeyUsagePurpose::ServerAuth,
            false,
        ),
        seen,
    ));
    let url = through(
        "postgres://someone:secret@127.0.0.1/db",
        front.port,
        "sslmode=require",
    );
    let ended = open(&PostgresConfig::parse(&url).expect("parses"));
    assert!(ended.is_err(), "the front ends the connection");
    let (mechanism, data) = said.recv().expect("the client's first SASL message");
    assert_eq!(mechanism, "SCRAM-SHA-256");
    assert_eq!(&data[..3], b"n,,");
}
