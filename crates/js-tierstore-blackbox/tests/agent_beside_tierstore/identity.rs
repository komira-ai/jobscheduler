//! The host's identity as hostkeeper keeps it on a farm node (its facts and its host
//! certificate), and the lease server's own certificate.
//!
//! The host certificate is minted by a CA of the test's own in the shape hostkeeper issues (one
//! DNS SAN, the host id; `clientAuth`; hostkeeper's host-role URI), which the lease server trusts
//! for agents (`--agent-client-ca`). The certificates are valid from 2026-09-01 to 3000-01-01.

use std::path::Path;

use rcgen::string::Ia5String;
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DistinguishedName, DnType,
    ExtendedKeyUsagePurpose, IsCa, KeyPair, SanType, SerialNumber, date_time_ymd,
};

use crate::host::{make_dir, put};

/// The host id, which the facts name and the certificate's one DNS SAN carries.
pub const HOST_ID: &str = "blackbox-node-1";

/// hostkeeper's facts of the host, which the agent (`--host-id-from-facts`) and tierstore
/// (`server.proxy_id_from_facts`) read.
pub const FACTS_FILE: &str = "/var/lib/hostkeeper/facts.json";

/// hostkeeper's copy of the host identity.
pub const IDENTITY_DIR: &str = "/var/lib/hostkeeper/identity";

/// The URI SAN hostkeeper puts on a host's own certificate.
const HOST_ROLE_URI: &str = "urn:hostkeeper:host-role:host";

/// A certificate authority of the test's own: hostkeeper's CA as the lease server sees it.
pub struct Ca(CertifiedIssuer<'static, KeyPair>);

impl Ca {
    /// A new CA.
    pub fn new() -> Ca {
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("CA params");
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.distinguished_name = DistinguishedName::new();
        params
            .distinguished_name
            .push(DnType::CommonName, "test-only hostkeeper CA");
        let key = KeyPair::generate().expect("a CA key");
        Ca(CertifiedIssuer::self_signed(params, key).expect("the CA"))
    }

    /// The CA's certificate as PEM.
    pub fn pem(&self) -> String {
        self.0.pem()
    }

    /// The host certificate of `host` and its key, as PEM, in the shape hostkeeper issues.
    fn host(&self, host: &str) -> (String, String) {
        let key = KeyPair::generate().expect("a host key");
        let mut params = CertificateParams::new(vec![host.to_owned()]).expect("host params");
        let uri = Ia5String::try_from(HOST_ROLE_URI.to_owned()).expect("an IA5 URI");
        params.subject_alt_names.push(SanType::URI(uri));
        params.distinguished_name = DistinguishedName::new();
        params
            .distinguished_name
            .push(DnType::CommonName, format!("host:{host}"));
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        params.serial_number = Some(SerialNumber::from(1u64));
        params.not_before = date_time_ymd(2026, 9, 1);
        params.not_after = date_time_ymd(3000, 1, 1);
        let cert = params
            .signed_by(&key, &self.0)
            .expect("the host certificate");
        (cert.pem(), key.serialize_pem())
    }

    /// Writes hostkeeper's facts and host identity of [`HOST_ID`] at the paths hostkeeper keeps
    /// them, with its modes.
    pub fn write_host(&self) {
        put(
            Path::new(FACTS_FILE),
            &format!("{{\"schema\":1,\"host_id\":\"{HOST_ID}\",\"host_group\":\"all-in-one\"}}\n"),
            0o644,
        );
        let (cert, key) = self.host(HOST_ID);
        make_dir(Path::new(IDENTITY_DIR), 0o750);
        put(&Path::new(IDENTITY_DIR).join("host.crt"), &cert, 0o644);
        put(&Path::new(IDENTITY_DIR).join("host.key"), &key, 0o640);
    }
}

/// The lease server's own certificate and key, as PEM, for `localhost` and `127.0.0.1`.
pub fn server_cert() -> (String, String) {
    let names = vec!["localhost".to_owned(), "127.0.0.1".to_owned()];
    let made = rcgen::generate_simple_self_signed(names).expect("the server's certificate");
    (made.cert.pem(), made.signing_key.serialize_pem())
}
