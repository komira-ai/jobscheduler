//! The host identity: the host's client certificate and its key, kept in two files that
//! another program (hostkeeper) replaces in place while this one runs. A client that
//! authenticates by it sends no bearer.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use rustix::process::geteuid;
use rustls::crypto::ring::default_provider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::sign::CertifiedKey;
use ureq::Agent;
use ureq::tls::{Certificate, ClientCert, PrivateKey};

use crate::error::ClientError;
use crate::token::{Probe, probe};

/// The flag naming the certificate file; errors about the file name it.
const CERT_FLAG: &str = "--host-cert";

/// The flag naming the key file; errors about the file name it.
const KEY_FLAG: &str = "--host-key";

/// The most a certificate file may hold: a chain of a few certificates.
const MAX_CERT_FILE: u64 = 64 * 1024;

/// The most a key file may hold.
const MAX_KEY_FILE: u64 = 16 * 1024;

/// Why a key file with these facts is not one a program running as `euid` may use as the
/// host's key, or `None` when it is. After following links it must be a regular file owned
/// by root or by `euid`, with no permission for others and not writable by its group: mode
/// 0600, 0400, 0640 and 0440 pass; 0644, 0660 and 0604 do not. `mode` is the permission
/// bits. The text follows the path in a message and never holds the file's content.
#[must_use]
pub fn host_key_problem(regular: bool, mode: u32, owner: u32, euid: u32) -> Option<String> {
    let mode = mode & 0o7777;
    if !regular {
        return Some("is not a regular file".to_owned());
    }
    if owner != 0 && owner != euid {
        return Some(format!(
            "must be owned by root or by uid {euid}, not by uid {owner}"
        ));
    }
    if mode & 0o007 != 0 {
        return Some(format!(
            "must give others no access (mode 0600 or 0640), not mode {mode:04o}"
        ));
    }
    (mode & 0o020 != 0).then(|| {
        format!("must not be writable by its group (mode 0600 or 0640), not mode {mode:04o}")
    })
}

/// The two files of the host's identity: where they are, not what they hold. They are the
/// certificate and key that hostkeeper keeps for the host and renews in place. Reading them
/// is [`crate::Client::connect_with_identity`]'s job, and again before each request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostIdentity {
    cert: PathBuf,
    key: PathBuf,
}

impl HostIdentity {
    /// The identity in the certificate file `cert` (PEM, the leaf first and the rest of its
    /// chain after it) and the key file `key` (PEM, PKCS#8 or SEC1; see
    /// [`host_key_problem`] for the mode and owner it must have). Neither need exist yet.
    #[must_use]
    pub fn new(cert: impl Into<PathBuf>, key: impl Into<PathBuf>) -> Self {
        Self {
            cert: cert.into(),
            key: key.into(),
        }
    }

    /// The path of the certificate file.
    #[must_use]
    pub fn cert_path(&self) -> &Path {
        &self.cert
    }

    /// The path of the key file.
    #[must_use]
    pub fn key_path(&self) -> &Path {
        &self.key
    }

    fn refuse_cert(&self, why: impl std::fmt::Display) -> ClientError {
        ClientError::flag(CERT_FLAG, format!("{} {why}", self.cert.display()))
    }

    fn refuse_key(&self, why: impl std::fmt::Display) -> ClientError {
        ClientError::flag(KEY_FLAG, format!("{} {why}", self.key.display()))
    }

    /// The key file's bytes, when the file passes [`host_key_problem`].
    fn read_key(&self) -> Result<Vec<u8>, ClientError> {
        let euid = geteuid().as_raw();
        let allowed = |probe: &Probe| {
            host_key_problem(probe.is_file, probe.mode, probe.owner, euid).is_none()
        };
        let read = probe(&self.key, MAX_KEY_FILE, &allowed)
            .map_err(|why| self.refuse_key(format!("cannot be read: {why}")))?;
        match host_key_problem(read.is_file, read.mode, read.owner, euid) {
            Some(problem) => Err(self.refuse_key(problem)),
            None => Ok(read.bytes),
        }
    }

    /// The certificate file's bytes, when it is a regular file of at most [`MAX_CERT_FILE`]
    /// bytes. It is opened without blocking, as the key is, so a named pipe is refused.
    fn read_cert(&self) -> Result<Vec<u8>, ClientError> {
        let read = probe(&self.cert, MAX_CERT_FILE + 1, &|probe| probe.is_file)
            .map_err(|why| self.refuse_cert(format!("cannot be read: {why}")))?;
        if !read.is_file {
            return Err(self.refuse_cert("is not a regular file"));
        }
        if read.bytes.len() as u64 > MAX_CERT_FILE {
            return Err(self.refuse_cert(format!("is larger than {MAX_CERT_FILE} bytes")));
        }
        Ok(read.bytes)
    }

    /// Reads both files and checks them: the certificate parses, the key passes
    /// [`host_key_problem`] and parses, and the key is the certificate's. The errors name
    /// the file, never its bytes.
    fn read(&self) -> Result<Pair, ClientError> {
        let cert = self.read_cert()?;
        let key = self.read_key()?;
        let chain = CertificateDer::pem_slice_iter(&cert)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| self.refuse_cert("does not hold PEM certificates"))?;
        if chain.is_empty() {
            return Err(self.refuse_cert("holds no certificate"));
        }
        let private = PrivateKey::from_pem(&key)
            .map_err(|_| self.refuse_key("does not hold a PEM private key"))?;
        let signer = PrivateKeyDer::try_from(private.der())
            .ok()
            .and_then(|der| {
                default_provider()
                    .key_provider
                    .load_private_key(der.clone_key())
                    .ok()
            })
            .ok_or_else(|| self.refuse_key("holds a key of a kind that is not supported"))?;
        let certified = CertifiedKey::new(chain.clone(), signer);
        if certified.keys_match().is_err() {
            return Err(self.refuse_key(format!(
                "does not go with the certificate in {}",
                self.cert.display()
            )));
        }
        let owned: Vec<Certificate<'static>> = chain
            .iter()
            .map(|der| Certificate::from_der(der.as_ref()).to_owned())
            .collect();
        Ok(Pair {
            bytes: (cert, key),
            client: ClientCert::new_with_certs(&owned, private),
        })
    }
}

/// A certificate and key read together, as the bytes they came from and as what the
/// TLS layer takes.
struct Pair {
    bytes: (Vec<u8>, Vec<u8>),
    client: ClientCert,
}

/// The agent a client sends its requests with, and the files it was built from.
struct Current {
    agent: Agent,
    bytes: (Vec<u8>, Vec<u8>),
}

/// A host identity in use: it hands out the HTTP agent of the newest valid pair.
pub(crate) struct Live {
    identity: HostIdentity,
    build: Box<dyn Fn(ClientCert) -> Agent + Send + Sync>,
    current: Mutex<Current>,
}

impl Live {
    /// Reads the identity and builds the first agent with `build`.
    pub(crate) fn open(
        identity: HostIdentity,
        build: impl Fn(ClientCert) -> Agent + Send + Sync + 'static,
    ) -> Result<Arc<Self>, ClientError> {
        let pair = identity.read()?;
        let agent = build(pair.client);
        Ok(Arc::new(Self {
            identity,
            build: Box::new(build),
            current: Mutex::new(Current {
                agent,
                bytes: pair.bytes,
            }),
        }))
    }

    /// The agent to send the next request with. The files are read again first: when their
    /// bytes changed and the pair they now hold is valid, the agent is rebuilt with it; a
    /// pair that cannot be read or does not hold together (a renewal caught between its two
    /// renames) leaves the agent the client had.
    pub(crate) fn agent(&self) -> Agent {
        // The files are read before the lock is taken: it is held only to compare and swap.
        let read = self.identity.read();
        let mut current = self.current.lock().unwrap_or_else(PoisonError::into_inner);
        if let Ok(pair) = read
            && pair.bytes != current.bytes
        {
            current.agent = (self.build)(pair.client);
            current.bytes = pair.bytes;
        }
        current.agent.clone()
    }
}
