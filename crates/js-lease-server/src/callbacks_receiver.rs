//! A callback receiver for the delivery's tests: an HTTPS listener on loopback port 0 whose
//! certificate, for [`HOST`] and `127.0.0.1`, a test authority of its own signed. It answers
//! each connection with the next of the replies it was given, after running the test's hook,
//! and sends the head of each request it read to the test. With it, a test's sender admits
//! loopback ([`AddressRule::admit_loopback`]) and resolves [`HOST`] by [`Loopback`], so nothing
//! leaves the machine and no name is looked up.

use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, TcpListener};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use js_callback::{AddressRule, Resolve, ResolveError, Sender, TlsRoots};
use rcgen::{CertificateParams, CertifiedIssuer, IsCa, KeyPair};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ServerConfig, ServerConnection, StreamOwned};

/// The name the receiver's certificate is for, which [`Loopback`] resolves.
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

    /// A sender with a minute's limit that trusts this authority alone, admits loopback
    /// besides what `rule` admits, and resolves every name as `resolver` does.
    pub(crate) fn sender(
        &self,
        rule: AddressRule,
        resolver: impl Resolve + Send + Sync + 'static,
    ) -> Sender {
        let roots = TlsRoots::from_der(&[self.0.der().to_vec()]);
        let rule = rule.admit_loopback();
        Sender::new(Duration::from_secs(60), rule, Box::new(resolver), roots)
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

/// A resolver that answers every name with one address.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Loopback(pub(crate) IpAddr);

impl Loopback {
    /// The resolver that answers `127.0.0.1`.
    pub(crate) fn new() -> Loopback {
        Loopback(IpAddr::V4(Ipv4Addr::LOCALHOST))
    }
}

impl Resolve for Loopback {
    fn resolve(&self, _: &str, _: Duration) -> Result<Vec<IpAddr>, ResolveError> {
        Ok(vec![self.0])
    }
}

/// What a receiver runs after it read a request and before it answers.
pub(crate) type Hook = Box<dyn FnMut() + Send>;

/// The answer `status` with no body.
pub(crate) fn status(code: u16) -> String {
    format!("HTTP/1.1 {code} X\r\ncontent-length: 0\r\n\r\n")
}

/// A running receiver.
pub(crate) struct Receiver {
    /// The port it listens on, on `127.0.0.1`.
    pub(crate) port: u16,
    /// The head of each request it read, in order: the request line and the headers, up to and
    /// including the blank line.
    heads: mpsc::Receiver<String>,
}

impl Receiver {
    /// A receiver whose certificate `authority` signed, answering its first connection with
    /// `replies[0]`, its second with `replies[1]`, and every later one with the last reply.
    pub(crate) fn start(authority: &Authority, replies: &[String]) -> Receiver {
        Receiver::hooked(authority, replies, Box::new(|| {}))
    }

    /// [`Receiver::start`], running `hook` after each request is read, before its answer.
    pub(crate) fn hooked(authority: &Authority, replies: &[String], mut hook: Hook) -> Receiver {
        let listener = TcpListener::bind("127.0.0.1:0").expect("binds loopback");
        let port = listener.local_addr().expect("an address").port();
        let config = authority.server();
        let replies = replies.to_vec();
        let (head_to, heads) = mpsc::channel();
        thread::spawn(move || {
            for index in 0.. {
                let (stream, _) = listener.accept().expect("accepts");
                let session = ServerConnection::new(config.clone()).expect("a TLS session");
                let mut tls = StreamOwned::new(session, stream);
                let head = read_head(&mut tls).expect("a request");
                // The head goes out before the reply, so it is there when the sender returns.
                head_to.send(head).ok();
                hook();
                let reply: &String = &replies[index.min(replies.len() - 1)];
                tls.write_all(reply.as_bytes()).expect("answers");
                tls.flush().expect("answers");
            }
        });
        Receiver { port, heads }
    }

    /// The heads of the requests read since the last call. Every request a send made is here
    /// once that send has returned with an answer.
    pub(crate) fn heads(&self) -> Vec<String> {
        self.heads.try_iter().collect()
    }

    /// Waits for the next request's head.
    pub(crate) fn next_head(&self) -> String {
        self.heads.recv().expect("a request")
    }

    /// The callback URL of `path` (which starts with `/`) on this receiver, by [`HOST`].
    pub(crate) fn url(&self, path: &str) -> String {
        format!("https://{HOST}:{}{path}", self.port)
    }
}

/// The request head `stream` sends: every byte up to and including the first blank line.
fn read_head(stream: &mut impl Read) -> io::Result<String> {
    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte)?;
        head.push(byte[0]);
    }
    Ok(String::from_utf8(head).expect("an ASCII head"))
}
