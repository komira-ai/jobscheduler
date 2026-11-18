//! Host certificates minted for the unit tests: a CA of the test's own and leaves of any
//! shape, so a test can break one rule at a time.

use rcgen::string::Ia5String;
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DistinguishedName, DnType,
    ExtendedKeyUsagePurpose, IsCa, KeyPair, SanType, SerialNumber, date_time_ymd,
};
use rustls::pki_types::CertificateDer;

/// The URI SAN hostkeeper puts on a host's own certificate.
pub(crate) const HOST_ROLE_URI: &str = "urn:hostkeeper:host-role:host";

/// The URI SAN hostkeeper puts on a certificate of the agent role.
pub(crate) const AGENT_ROLE_URI: &str = "urn:hostkeeper:role:agent";

/// What a minted leaf has; [`Shape::host`] is a valid host certificate, shaped as hostkeeper issues it.
pub(crate) struct Shape {
    pub(crate) dns: Vec<String>,
    pub(crate) uris: Vec<String>,
    /// The subject common name, which the shape rules do not read.
    pub(crate) cn: Option<String>,
    pub(crate) usages: Vec<ExtendedKeyUsagePurpose>,
    /// Also an IP address SAN, which the shape rules do not read.
    pub(crate) ip: bool,
    /// A CA's basic constraints.
    pub(crate) ca: bool,
    /// The years the certificate is valid from and until.
    pub(crate) years: (i32, i32),
    pub(crate) serial: u64,
}

impl Shape {
    /// The host certificate of `host`, with the URI SAN and CN hostkeeper gives it.
    pub(crate) fn host(host: &str) -> Self {
        Self {
            dns: vec![host.to_owned()],
            uris: vec![HOST_ROLE_URI.to_owned()],
            cn: Some(format!("host:{host}")),
            usages: vec![ExtendedKeyUsagePurpose::ClientAuth],
            ip: true,
            ca: false,
            years: (2020, 3000),
            serial: 1,
        }
    }
}

/// A leaf certificate and its key.
pub(crate) struct Leaf {
    pub(crate) der: CertificateDer<'static>,
}

/// The certificate authority of a test.
pub(crate) struct Authority(CertifiedIssuer<'static, KeyPair>);

impl Authority {
    /// A new CA with a key of its own.
    pub(crate) fn new() -> Self {
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.distinguished_name = DistinguishedName::new();
        params
            .distinguished_name
            .push(DnType::CommonName, "test-only CA");
        Self(CertifiedIssuer::self_signed(params, KeyPair::generate().unwrap()).unwrap())
    }

    /// The CA's certificate as PEM text.
    pub(crate) fn pem(&self) -> String {
        self.0.pem()
    }

    /// A leaf of `shape` signed by this CA.
    pub(crate) fn issue(&self, shape: &Shape) -> Leaf {
        let key = KeyPair::generate().unwrap();
        let cert = params(shape).signed_by(&key, &self.0).unwrap();
        Leaf {
            der: cert.der().clone(),
        }
    }
}

/// A leaf of `shape` that signed itself.
pub(crate) fn self_signed(shape: &Shape) -> Leaf {
    let key = KeyPair::generate().unwrap();
    let cert = params(shape).self_signed(&key).unwrap();
    Leaf {
        der: cert.der().clone(),
    }
}

fn params(shape: &Shape) -> CertificateParams {
    let mut params = CertificateParams::new(shape.dns.clone()).unwrap();
    for uri in &shape.uris {
        let uri = Ia5String::try_from(uri.clone()).unwrap();
        params.subject_alt_names.push(SanType::URI(uri));
    }
    if shape.ip {
        let ip = std::net::IpAddr::from([127, 0, 0, 1]);
        params.subject_alt_names.push(SanType::IpAddress(ip));
    }
    params.distinguished_name = DistinguishedName::new();
    if let Some(cn) = &shape.cn {
        params
            .distinguished_name
            .push(DnType::CommonName, cn.clone());
    }
    params.distinguished_name = DistinguishedName::new();
    if let Some(cn) = &shape.cn {
        params
            .distinguished_name
            .push(DnType::CommonName, cn.clone());
    }
    params.extended_key_usages = shape.usages.clone();
    params.serial_number = Some(SerialNumber::from(shape.serial));
    params.not_before = date_time_ymd(shape.years.0, 1, 1);
    params.not_after = date_time_ymd(shape.years.1, 1, 1);
    if shape.ca {
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    }
    params
}
