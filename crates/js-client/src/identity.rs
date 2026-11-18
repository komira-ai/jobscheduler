//! The service identity: a client certificate and its key, kept in two files that another
//! program replaces while this one runs. A client that authenticates by it sends no bearer.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use rustls::crypto::ring::default_provider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::sign::CertifiedKey;
use ureq::Agent;
use ureq::tls::{Certificate, ClientCert, PrivateKey};

use crate::error::ClientError;
use crate::token::owner_only;

/// The name, in a state root, of the file holding the service identity's certificate in
/// PEM, the leaf first and the rest of its chain after it.
pub const SERVICE_CERT_FILE: &str = "service-identity.pem";

/// The name, in a state root, of the file holding the service identity's private key in
/// PEM (PKCS#8 or SEC1). It must be owner-only, as a token file must.
pub const SERVICE_KEY_FILE: &str = "service-identity.key";

/// The flag whose directory holds the identity files; errors name it.
const FLAG: &str = "--state-root";

/// The most a certificate file may hold: a chain of a few certificates.
const MAX_CERT_FILE: u64 = 64 * 1024;

/// The most a key file may hold.
const MAX_KEY_FILE: u64 = 16 * 1024;

/// The two files of a service identity: where they are, not what they hold. Reading them
/// is [`crate::Client::connect_with_identity`]'s job, and again before each request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceIdentity {
    cert: PathBuf,
    key: PathBuf,
}

impl ServiceIdentity {
    /// The identity files [`SERVICE_CERT_FILE`] and [`SERVICE_KEY_FILE`] of `root`, which
    /// may not exist.
    #[must_use]
    pub fn in_dir(root: &Path) -> Self {
        Self {
            cert: root.join(SERVICE_CERT_FILE),
            key: root.join(SERVICE_KEY_FILE),
        }
    }

    /// The identity of `root` when both of its files exist, `None` when neither does.
    ///
    /// # Errors
    ///
    /// [`ClientError::Flag`] naming `--state-root` and the file that is missing when only
    /// one of the two exists: half an identity is never taken for none.
    pub fn found_in(root: &Path) -> Result<Option<Self>, ClientError> {
        let identity = Self::in_dir(root);
        match (identity.cert.exists(), identity.key.exists()) {
            (true, true) => Ok(Some(identity)),
            (false, false) => Ok(None),
            (true, false) => Err(identity.missing(&identity.key)),
            (false, true) => Err(identity.missing(&identity.cert)),
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

    fn missing(&self, file: &Path) -> ClientError {
        ClientError::flag(
            FLAG,
            format!(
                "{} is missing: the service identity needs both {SERVICE_CERT_FILE} and {SERVICE_KEY_FILE}",
                file.display()
            ),
        )
    }

    fn refuse(&self, file: &Path, why: impl std::fmt::Display) -> ClientError {
        ClientError::flag(FLAG, format!("{} {why}", file.display()))
    }

    /// Reads both files and checks them: the certificate parses, the key is owner-only and
    /// parses, and the key is the certificate's. The errors name the file, never its bytes.
    fn read(&self) -> Result<Pair, ClientError> {
        let cert = read_public(&self.cert)
            .map_err(|why| self.refuse(&self.cert, format!("cannot be read: {why}")))?;
        let key = owner_only(&self.key, MAX_KEY_FILE).map_err(|why| self.refuse(&self.key, why))?;
        let chain = CertificateDer::pem_slice_iter(&cert)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| self.refuse(&self.cert, "does not hold PEM certificates"))?;
        if chain.is_empty() {
            return Err(self.refuse(&self.cert, "holds no certificate"));
        }
        let private = PrivateKey::from_pem(&key)
            .map_err(|_| self.refuse(&self.key, "does not hold a PEM private key"))?;
        let signer = PrivateKeyDer::try_from(private.der())
            .ok()
            .and_then(|der| {
                default_provider()
                    .key_provider
                    .load_private_key(der.clone_key())
                    .ok()
            })
            .ok_or_else(|| self.refuse(&self.key, "holds a key of a kind that is not supported"))?;
        let certified = CertifiedKey::new(chain.clone(), signer);
        if certified.keys_match().is_err() {
            return Err(self.refuse(
                &self.key,
                format!(
                    "does not go with the certificate in {}",
                    self.cert.display()
                ),
            ));
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

/// A whole file of at most [`MAX_CERT_FILE`] bytes.
fn read_public(path: &Path) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)?
        .take(MAX_CERT_FILE)
        .read_to_end(&mut bytes)?;
    Ok(bytes)
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

/// A service identity in use: it hands out the HTTP agent of the newest valid pair.
pub(crate) struct Live {
    identity: ServiceIdentity,
    build: Box<dyn Fn(ClientCert) -> Agent + Send + Sync>,
    current: Mutex<Current>,
}

impl Live {
    /// Reads the identity and builds the first agent with `build`.
    pub(crate) fn open(
        identity: ServiceIdentity,
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
        let mut current = self.current.lock().unwrap_or_else(PoisonError::into_inner);
        if let Ok(pair) = self.identity.read()
            && pair.bytes != current.bytes
        {
            current.agent = (self.build)(pair.client);
            current.bytes = pair.bytes;
        }
        current.agent.clone()
    }
}
