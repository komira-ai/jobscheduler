//! A callback receiver for the sender's tests: an HTTPS listener on loopback port 0 whose
//! certificate, for [`HOST`] and `127.0.0.1`, a test authority of its own signed. It answers
//! each connection with the next of the replies it was given, written byte for byte, and keeps
//! every connection open until it is dropped, so a reply that promises a body it never sends,
//! or no reply at all, holds the sender until it gives up.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, mpsc};
use std::thread;

use rcgen::{CertificateParams, CertifiedIssuer, IsCa, KeyPair};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ServerConfig, ServerConnection, StreamOwned};

use crate::{CallbackUrl, TlsRoots, check_url};

/// The name the receiver's certificate is for, which the tests' fake resolver answers.
pub(crate) const HOST: &str = "hooks.test";

/// A certificate authority of a test's own.
pub(crate) struct Authority(CertifiedIssuer<'static, KeyPair>);

impl Authority {
    /// A new authority.
    pub(crate) fn new() -> Authority {
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("CA params");
        params.is_ca = IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let key = KeyPair::generate().expect("a key");
        Authority(CertifiedIssuer::self_signed(params, key).expect("a CA"))
    }

    /// The roots a sender that trusts this authority alone takes.
    pub(crate) fn roots(&self) -> TlsRoots {
        TlsRoots::from_der(&[self.0.der().to_vec()])
    }

    /// A server configuration with a certificate for [`HOST`] and `127.0.0.1`.
    fn server(&self) -> Arc<ServerConfig> {
        let key = KeyPair::generate().expect("a key");
        let names = vec![HOST.to_owned(), "127.0.0.1".to_owned()];
        let params = CertificateParams::new(names).expect("params");
        let cert = params.signed_by(&key, &self.0).expect("signs");
        let chain: Vec<CertificateDer<'static>> = vec![cert.der().clone()];
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der()));
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("versions")
            .with_no_client_auth()
            .with_single_cert(chain, key)
            .expect("a server configuration");
        Arc::new(config)
    }
}

/// One connection the receiver holds open.
type Held = StreamOwned<ServerConnection, std::net::TcpStream>;

/// A running receiver.
pub(crate) struct Receiver {
    /// The port it listens on, on `127.0.0.1`.
    pub(crate) port: u16,
    /// The head of each request it read, in order: the request line and the headers, up to and
    /// including the blank line.
    heads: mpsc::Receiver<String>,
    /// Every connection it answered, held open while the receiver lives.
    _held: mpsc::Receiver<Held>,
}

impl Receiver {
    /// A receiver whose certificate `authority` signed, answering its first connection with
    /// `replies[0]`, its second with `replies[1]`, and every later one with the last reply.
    pub(crate) fn start(authority: &Authority, replies: &[&str]) -> Receiver {
        let listener = TcpListener::bind("127.0.0.1:0").expect("binds loopback");
        let port = listener.local_addr().expect("an address").port();
        let config = authority.server();
        let replies: Vec<String> = replies.iter().map(|reply| (*reply).to_owned()).collect();
        let (head_to, heads) = mpsc::channel();
        let (held_to, held) = mpsc::channel();
        thread::spawn(move || {
            for (index, stream) in listener.incoming().enumerate() {
                let session = ServerConnection::new(config.clone()).expect("a TLS session");
                let mut tls = StreamOwned::new(session, stream.expect("accepts"));
                // The head goes out before the reply, so it is there when the sender returns.
                head_to.send(read_head(&mut tls)).ok();
                let reply = &replies[index.min(replies.len() - 1)];
                tls.write_all(reply.as_bytes()).expect("answers");
                tls.flush().expect("answers");
                held_to.send(tls).ok();
            }
        });
        Receiver {
            port,
            heads,
            _held: held,
        }
    }

    /// The heads of the requests read since the last call. Every request a send made is here
    /// once that send has returned with an answer.
    pub(crate) fn heads(&self) -> Vec<String> {
        self.heads.try_iter().collect()
    }

    /// The callback URL of `path` (which starts with `/`) on this receiver, by [`HOST`].
    pub(crate) fn url(&self, path: &str) -> CallbackUrl {
        url_of(HOST, self.port, path)
    }
}

/// The callback URL `https://host:port` followed by `path`.
pub(crate) fn url_of(host: &str, port: u16, path: &str) -> CallbackUrl {
    check_url(&format!("https://{host}:{port}{path}")).expect("a callback URL")
}

/// The request head `stream` sends: every byte up to and including the first blank line.
fn read_head(stream: &mut impl Read) -> String {
    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).expect("reads the request");
        head.push(byte[0]);
    }
    String::from_utf8(head).expect("an ASCII head")
}
