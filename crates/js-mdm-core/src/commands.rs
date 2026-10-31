//! The arguments the host layer passes to the tools, as pure functions of the settings.

use crate::config::MdmConfig;

/// The arguments of `scepserver ca -init` that create the SCEP CA in
/// `<MDM_STATE>/scep/depot`, after the binary's own path. The subject is the issuer of
/// every Mac's MDM identity certificate; changing the CA later re-enrolls every Mac.
pub fn scep_ca_init_args(c: &MdmConfig) -> Vec<String> {
    let _ = c;
    todo!("red run: not implemented yet")
}

/// The path (with its query) NanoDEP's API answers with the DEP token certificate:
/// `GET /v1/tokenpki/<MDM_DEP_NAME>?cn=<MDM_DEP_CERT_CN>&validity_days=<MDM_DEP_CERT_DAYS>`.
pub fn dep_cert_request_path(c: &MdmConfig) -> String {
    let _ = c;
    todo!("red run: not implemented yet")
}
