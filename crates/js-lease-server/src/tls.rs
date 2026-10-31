//! The server's TLS configuration, from the files of `--tls-cert` and `--tls-key`, and
//! the reload of them while the server runs.
//!
//! The certificate and key are one `CertifiedKey` behind a lock that the handshake reads
//! once, so a handshake sees the old pair or the new pair and never one of each. A
//! connection that is already open keeps what it negotiated.

#![allow(dead_code)]

use std::sync::{Arc, PoisonError, RwLock};

use rustls::ServerConfig;
use rustls::crypto::ring::default_provider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, UnixTime};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use tokio_rustls::TlsAcceptor;

use crate::config::{TlsFiles, read_private};
use crate::error::StartupError;

/// The pair the handshakes use, replaced as a whole.
#[derive(Debug)]
struct Current(RwLock<Arc<CertifiedKey>>);

impl ResolvesServerCert for Current {
    fn resolve(&self, _hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(
            self.0
                .read()
                .unwrap_or_else(PoisonError::into_inner)
                .clone(),
        )
    }
}

/// The server's TLS: the acceptor for the listeners and the means to reload the pair.
pub(crate) struct Tls {
    files: TlsFiles,
    current: Arc<Current>,
    acceptor: TlsAcceptor,
}

impl Tls {
    /// TLS serving the certificate chain and key in `files`, speaking HTTP/1.1 only. The
    /// key file must be owner-only like the token file (see `read_private`).
    pub(crate) fn open(files: &TlsFiles) -> Result<Self, StartupError> {
        let current = Arc::new(Current(RwLock::new(Arc::new(load(files)?))));
        let builder = ServerConfig::builder_with_provider(Arc::new(default_provider()))
            .with_safe_default_protocol_versions()
            .map_err(StartupError::because("--tls-cert"))?;
        let mut config = builder
            .with_no_client_auth()
            .with_cert_resolver(current.clone());
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(Self {
            files: files.clone(),
            current,
            acceptor: TlsAcceptor::from(Arc::new(config)),
        })
    }

    /// The acceptor of the listeners; it follows every reload.
    pub(crate) fn acceptor(&self) -> TlsAcceptor {
        self.acceptor.clone()
    }

    /// Reads the files again and, when the pair is whole (the key owner-only and the key
    /// of the certificate) and the leaf certificate is valid now, serves it from the next
    /// handshake on. Otherwise the pair being served stays and the error says why.
    pub(crate) fn reload(&self) -> Result<(), StartupError> {
        let _ = (&self.files, &self.current);
        Err(StartupError::new("--tls-cert", "red stub"))
    }
}

/// The pair in `files`, with the key checked to be the certificate's.
fn load(files: &TlsFiles) -> Result<CertifiedKey, StartupError> {
    let certs = CertificateDer::pem_file_iter(&files.cert)
        .and_then(|found| found.collect::<Result<Vec<_>, _>>())
        .map_err(|err| {
            StartupError::new("--tls-cert", format!("{}: {err}", files.cert.display()))
        })?;
    if certs.is_empty() {
        let reason = format!("{}: no certificate found", files.cert.display());
        return Err(StartupError::new("--tls-cert", reason));
    }
    let key_bytes = read_private(&files.key, "--tls-key")?;
    let key = PrivateKeyDer::from_pem_slice(&key_bytes)
        .map_err(|err| StartupError::new("--tls-key", format!("{}: {err}", files.key.display())))?;
    let signer = default_provider()
        .key_provider
        .load_private_key(key)
        .map_err(StartupError::because("--tls-key"))?;
    let pair = CertifiedKey::new(certs, signer);
    pair.keys_match()
        .map_err(|err| StartupError::new("--tls-key", format!("{}: {err}", files.key.display())))?;
    Ok(pair)
}

/// Refuses a pair whose leaf certificate is not valid at this moment.
fn check_valid_now(pair: &CertifiedKey, files: &TlsFiles) -> Result<(), StartupError> {
    let now = i64::try_from(UnixTime::now().as_secs()).unwrap_or(i64::MAX);
    let refuse =
        |why: &str| StartupError::new("--tls-cert", format!("{}: {why}", files.cert.display()));
    let unreadable = refuse("the validity of the certificate cannot be read");
    let range = pair.cert.first().and_then(|der| validity(der.as_ref()));
    let (from, to) = range.ok_or(unreadable)?;
    if now < from {
        return Err(refuse("the certificate is not valid yet"));
    }
    if now > to {
        return Err(refuse("the certificate has expired"));
    }
    Ok(())
}

/// The not-before and not-after of a DER certificate, in seconds since the Unix epoch.
///
/// The readers below are written without a branch of their own (a `?` or a combinator
/// where a test would be) so that the paths a certificate in the wild can take are the
/// paths the tests of the server take.
fn validity(cert: &[u8]) -> Option<(i64, i64)> {
    // Certificate ::= SEQUENCE { tbsCertificate SEQUENCE { [0] version OPTIONAL, serial,
    // signature, issuer, validity SEQUENCE { notBefore, notAfter }, ... }, ... }
    let (_, cert, _) = tlv(cert)?;
    let (_, tbs, _) = tlv(cert)?;
    let version = tlv(tbs).filter(|(tag, _, _)| *tag == 0xa0);
    let mut rest = version.map_or(tbs, |(_, _, after)| after);
    // serial, signature, issuer
    for _ in 0..3 {
        rest = tlv(rest)?.2;
    }
    let (_, validity, _) = tlv(rest)?;
    let (from_tag, from, after) = tlv(validity)?;
    let (to_tag, to, _) = tlv(after)?;
    Some((time(from_tag, from)?, time(to_tag, to)?))
}

/// One DER element at the start of `bytes`: its tag, its content and what follows it.
fn tlv(bytes: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, rest) = bytes.split_first()?;
    let (&first, rest) = rest.split_first()?;
    // A length under 0x80 is itself; above it, the low bits count the bytes that hold it.
    let (len, rest) = match first.checked_sub(0x80) {
        None => (usize::from(first), rest),
        Some(count) => {
            let (digits, rest) = rest.split_at_checked(usize::from(count))?;
            let len = digits.iter().try_fold(0usize, |all, &digit| {
                all.checked_mul(256)?.checked_add(usize::from(digit))
            });
            (len?, rest)
        }
    };
    let (content, after) = rest.split_at_checked(len)?;
    Some((tag, content, after))
}

/// A UTCTime (`YYMMDDHHMMSSZ`) or, for tag 0x18, GeneralizedTime (`YYYYMMDDHHMMSSZ`) as
/// seconds since the Unix epoch. A two-digit year of 50 or more is 19yy.
fn time(tag: u8, text: &[u8]) -> Option<i64> {
    let digits = text.strip_suffix(b"Z")?;
    let number = |from: usize, len: usize| -> Option<i64> {
        digits
            .get(from..from + len)?
            .iter()
            .try_fold(0i64, |all, &b| {
                b.is_ascii_digit().then(|| all * 10 + i64::from(b - b'0'))
            })
    };
    let general = i64::from(tag == 0x18);
    let at = 2 + 2 * usize::from(tag == 0x18);
    let year = number(0, at)?;
    let year = year + (1 - general) * (1900 + 100 * i64::from(year < 50));
    let (month, day) = (number(at, 2)?, number(at + 2, 2)?);
    let (hour, minute, second) = (number(at + 4, 2)?, number(at + 6, 2)?, number(at + 8, 2)?);
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second)
}

/// Days from 1970-01-01 to the given proleptic Gregorian date.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let doy = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_count_from_the_epoch() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        assert_eq!(days_from_civil(1969, 12, 31), -1);
    }

    #[test]
    fn both_time_forms_are_read() {
        assert_eq!(time(0x17, b"700101000000Z"), Some(0));
        assert_eq!(time(0x18, b"19700101000001Z"), Some(1));
        assert_eq!(time(0x17, b"491231235959Z"), time(0x18, b"20491231235959Z"));
        assert_eq!(time(0x17, b"7001010000Z"), None);
        assert_eq!(time(0x17, b"70010100000xZ"), None);
    }

    #[test]
    fn elements_are_read_in_short_and_long_form() {
        assert_eq!(
            tlv(&[0x30, 0x01, 0xaa, 0xbb]),
            Some((0x30, &[0xaa][..], &[0xbb][..]))
        );
        let mut long = vec![0x30, 0x81, 0x80];
        long.extend([7u8; 0x80]);
        assert_eq!(
            tlv(&long).map(|(tag, body, rest)| (tag, body.len(), rest.len())),
            Some((0x30, 0x80, 0))
        );
        assert_eq!(tlv(&[0x30, 0x05, 1]), None);
        assert_eq!(tlv(&[0x30, 0x82, 0x01]), None);
        assert_eq!(tlv(&[0x30, 0x89, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0]), None);
        assert_eq!(tlv(&[0x30]), None);
        assert_eq!(tlv(&[]), None);
    }

    #[test]
    fn a_certificate_that_is_not_one_has_no_validity() {
        assert_eq!(validity(&[]), None);
        assert_eq!(validity(&[0x30, 0x03, 0x30, 0x01, 0x00]), None);
        assert_eq!(validity(b"not a certificate"), None);
    }

    #[test]
    fn a_pair_whose_certificate_cannot_be_read_is_refused() {
        let key = rcgen::KeyPair::generate().unwrap();
        let der = PrivateKeyDer::try_from(key.serialize_der()).unwrap();
        let signer = default_provider()
            .key_provider
            .load_private_key(der)
            .unwrap();
        let pair = CertifiedKey::new(vec![CertificateDer::from(vec![0x30, 0x00])], signer);
        let files = TlsFiles {
            cert: "cert.pem".into(),
            key: "key.pem".into(),
        };
        let err = check_valid_now(&pair, &files).unwrap_err();
        assert_eq!(err.flag(), "--tls-cert");
        assert!(err.to_string().contains("cannot be read"), "{err}");
    }
}
