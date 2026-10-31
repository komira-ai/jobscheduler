//! The server's TLS configuration, from the files of `--tls-cert` and `--tls-key`, and
//! the reload of them while the server runs.
//!
//! The certificate and key are one `CertifiedKey` behind a lock that the handshake reads
//! once, so a handshake sees the old pair or the new pair and never one of each. A
//! connection that is already open keeps what it negotiated.

use std::sync::{Arc, PoisonError, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use rustls::ServerConfig;
use rustls::crypto::ring::default_provider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
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
        let fresh = load(&self.files)?;
        check_valid_now(&fresh, &self.files)?;
        *self
            .current
            .0
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Arc::new(fresh);
        Ok(())
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
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_secs()).unwrap_or(i64::MAX)
        });
    let refuse =
        |why: &str| StartupError::new("--tls-cert", format!("{}: {why}", files.cert.display()));
    let Some((from, to)) = validity(pair.end_entity_cert().map_or(&[][..], |der| der.as_ref()))
    else {
        return Err(refuse("the validity of the certificate cannot be read"));
    };
    if now < from {
        return Err(refuse("the certificate is not valid yet"));
    }
    if now > to {
        return Err(refuse("the certificate has expired"));
    }
    Ok(())
}

/// The not-before and not-after of a DER certificate, in seconds since the Unix epoch.
fn validity(cert: &[u8]) -> Option<(i64, i64)> {
    // Certificate ::= SEQUENCE { tbsCertificate SEQUENCE { [0] version OPTIONAL, serial,
    // signature, issuer, validity SEQUENCE { notBefore, notAfter }, ... }, ... }
    let (_, cert, _) = tlv(cert)?;
    let (_, tbs, _) = tlv(cert)?;
    let mut rest = tbs;
    if *rest.first()? == 0xa0 {
        rest = tlv(rest)?.2;
    }
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
    let (len, rest) = if first < 0x80 {
        (usize::from(first), rest)
    } else {
        let count = usize::from(first & 0x7f);
        if count == 0 || count > 4 || rest.len() < count {
            return None;
        }
        let (len, rest) = rest.split_at(count);
        (
            len.iter().fold(0usize, |all, &b| all << 8 | usize::from(b)),
            rest,
        )
    };
    (rest.len() >= len).then(|| (tag, &rest[..len], &rest[len..]))
}

/// A UTCTime (tag 0x17, `YYMMDDHHMMSSZ`) or GeneralizedTime (0x18, `YYYYMMDDHHMMSSZ`) as
/// seconds since the Unix epoch.
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
    let (year, at) = match (tag, digits.len()) {
        (0x17, 12) => (
            number(0, 2).map(|yy| if yy >= 50 { 1900 + yy } else { 2000 + yy })?,
            2,
        ),
        (0x18, 14) => (number(0, 4)?, 4),
        _ => return None,
    };
    let (month, day) = (number(at, 2)?, number(at + 2, 2)?);
    let (hour, minute, second) = (number(at + 4, 2)?, number(at + 6, 2)?, number(at + 8, 2)?);
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second)
}

/// Days from 1970-01-01 to the given proleptic Gregorian date.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let shifted = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * shifted + 2) / 5 + day - 1;
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
        assert_eq!(tlv(&[0x30, 0x80]), None);
        assert_eq!(tlv(&[0x30, 0x85, 0, 0, 0, 0, 0]), None);
        assert_eq!(tlv(&[0x30]), None);
        assert_eq!(tlv(&[]), None);
    }

    #[test]
    fn a_certificate_that_is_not_one_has_no_validity() {
        assert_eq!(validity(&[]), None);
        assert_eq!(validity(&[0x30, 0x03, 0x30, 0x01, 0x00]), None);
        assert_eq!(validity(b"not a certificate"), None);
    }
}
