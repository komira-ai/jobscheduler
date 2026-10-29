//! The default mode: the host uses the network it already has.

use std::fs::File;
use std::path::PathBuf;

use hyper::Uri;
use js_elastic::{BootStep, HostName};

use super::{Contribution, LaunchCtx, NetworkError, NetworkMode};

/// The mode that adds nothing to a launch: no boot step and no secret. The host reaches
/// the service over the same LAN, a VPC, or a VPN its image joins on its own.
#[derive(Debug)]
pub struct DefaultMode {
    advertise_url: String,
    tls_files: Vec<PathBuf>,
}

impl DefaultMode {
    /// The mode for a service that tells hosts to dial `advertise_url`. `tls_files` are
    /// the certificate files the service itself needs when that URL is `https`.
    pub fn new(advertise_url: impl Into<String>, tls_files: Vec<PathBuf>) -> Self {
        Self {
            advertise_url: advertise_url.into(),
            tls_files,
        }
    }
}

impl NetworkMode for DefaultMode {
    fn prepare(&self, _launch: &LaunchCtx<'_>) -> Result<Contribution, NetworkError> {
        Ok(Contribution::empty())
    }

    fn describe(&self, _launch: &LaunchCtx<'_>) -> Vec<BootStep> {
        Vec::new()
    }

    /// Confirms that the advertised URL is `http` or `https` with a host, and, for
    /// `https`, that the certificate files can be opened.
    fn check(&self) -> Result<(), NetworkError> {
        let unusable = || {
            NetworkError::Misconfigured(format!(
                "advertise_url {:?} must be an http or https URL with a host",
                self.advertise_url
            ))
        };
        let uri: Uri = self.advertise_url.parse().map_err(|_| unusable())?;
        let https = match (uri.scheme_str(), uri.authority()) {
            (Some("https"), Some(_)) => true,
            (Some("http"), Some(_)) => false,
            _ => return Err(unusable()),
        };
        if https {
            for path in &self.tls_files {
                File::open(path).map_err(|err| {
                    NetworkError::Misconfigured(format!("{}: {err}", path.display()))
                })?;
            }
        }
        Ok(())
    }

    fn forget(&self, _host: &HostName) -> Result<(), NetworkError> {
        Ok(())
    }
}
