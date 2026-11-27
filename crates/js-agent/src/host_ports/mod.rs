//! Host ports (`docs/design/job-rootfs.md`, 4.1, 4.5 and 4.7): the ports of the host's
//! loopback a job reaches at ports of its own loopback ([`HostPort`]), the other direction
//! from the ports a job declares (`ports`, forwarded from the host into the job).
//!
//! - The operator lists the host ports a job may name with `--host-port PORT` or `LO-HI`,
//!   repeatable, at most [`MAX_ENTRIES`] entries ([`allowed`], [`HostPortFlags`]); the `crun`
//!   runtime and the macOS build's native runtime take the flag. The job's own port is not
//!   bounded on Linux: it is in the job's network alone.
//! - The admission refuses a start that names a host port the runtime does not allow
//!   (`input-not-allowed`, [`admit`]), with no I/O, before anything is made.
//! - On Linux the `crun` runtime offers each one with a listener inside the job's network
//!   namespace and a connection out of the agent's own ([`Forwarder`], with the namespace step
//!   behind [`Listen`] and [`Kernel`] the one an agent runs). The job's init waits until each
//!   job port listens before it starts the job's program (`js-init --wait-listen`).
//! - On macOS a native job shares the host's loopback, so it reaches a host port at the host
//!   port's own number (a remapped one is refused `host-port-remap-unsupported-here`), and
//!   the run's Seatbelt profile allows a connect to each port it names
//!   (`js_mac_pool::seatbelt::Policy::with_host_ports`); nothing listens for it.

#[cfg(target_os = "linux")]
mod forward;
#[cfg(target_os = "linux")]
mod kernel;
#[cfg(all(test, target_os = "linux"))]
mod tests;

use js_agent_wire::{HostPort, PortRange, limits};

#[cfg(target_os = "linux")]
pub use forward::{Forwarder, Listen, OfferError, Offered, THREADS};
#[cfg(target_os = "linux")]
pub use kernel::Kernel;

/// The most `--host-port` entries: as many as a job may name host ports.
pub const MAX_ENTRIES: usize = limits::MAX_PORTS;

/// Why the `--host-port` entries were refused at start.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FlagError {
    /// An entry is not `PORT` or `LO-HI`, each from 1 to 65535, `LO` not above `HI`.
    #[error("--host-port: not <PORT> or <LO-HI>, each from 1 to 65535, LO not above HI: {0:?}")]
    Entry(String),
    /// More than [`MAX_ENTRIES`] entries.
    #[error("--host-port: at most {MAX_ENTRIES} entries")]
    Many,
}

/// The `--host-port` flag, which both builds take: a Linux agent with the crun runtime and a
/// macOS agent with the native runtime.
#[derive(Debug, Clone, Default, clap::Args)]
pub struct HostPortFlags {
    /// A port of the host's loopback a job may reach (`host_ports`), `PORT` or `LO-HI`, each
    /// from 1 to 65535; repeatable, at most 16. A job that names a host port not listed is
    /// refused `input-not-allowed`, as is every job that names one when none is given. On
    /// Linux a job reaches each host port it names at a port of its own loopback, whatever
    /// its network level, through a listener the agent holds in the job's network namespace;
    /// it needs the crun runtime. On macOS a native job reaches it at its own number on the
    /// host's loopback, which its profile opens to that port alone, whatever its network level.
    #[arg(long = "host-port", value_name = "PORT|LO-HI")]
    pub host_port: Vec<String>,
}

impl HostPortFlags {
    /// The flag with whether it was given: an agent that runs no job refuses it, and so does
    /// a Linux agent that runs the container engine.
    #[must_use]
    pub fn given(&self) -> [(&'static str, bool); 1] {
        [("--host-port", !self.host_port.is_empty())]
    }

    /// The host ports the entries allow ([`allowed`]).
    ///
    /// # Errors
    ///
    /// The [`FlagError`] of the entries.
    pub fn allowed(&self) -> Result<Vec<PortRange>, FlagError> {
        allowed(&self.host_port)
    }
}

/// The host ports the `--host-port` entries `given` allow, each `PORT` or `LO-HI`, sorted and
/// each once.
///
/// # Errors
///
/// More than [`MAX_ENTRIES`] entries ([`FlagError::Many`]), or the first entry that is not a
/// port or a range ([`FlagError::Entry`]).
pub fn allowed(given: &[String]) -> Result<Vec<PortRange>, FlagError> {
    if given.len() > MAX_ENTRIES {
        return Err(FlagError::Many);
    }
    let mut ports = given
        .iter()
        .map(|text| PortRange::parse(text).ok_or_else(|| FlagError::Entry(text.clone())))
        .collect::<Result<Vec<_>, _>>()?;
    ports.sort();
    ports.dedup();
    Ok(ports)
}

/// Whether a start that names the host ports `asked` may run where `allowed` are the host
/// ports a job may reach: every `host_port` in one of them. Two job ports of one host port
/// are one host port.
///
/// # Errors
///
/// The text of the refusal (`input-not-allowed`) for the first host port, in the order given,
/// that none of `allowed` holds; it names the ports the agent allows.
pub(crate) fn admit(allowed: &[PortRange], asked: &[HostPort]) -> Result<(), String> {
    let Some(refused) = asked
        .iter()
        .find(|port| !allowed.iter().any(|range| range.contains(port.host_port)))
    else {
        return Ok(());
    };
    let offered: Vec<String> = allowed.iter().map(PortRange::to_string).collect();
    let offered = match offered.is_empty() {
        true => "none".to_owned(),
        false => offered.join(","),
    };
    Err(format!(
        "host port {} is not one this agent offers (--host-port: {offered})",
        refused.host_port
    ))
}

#[cfg(test)]
mod flag_tests {
    use js_agent_wire::PortProtocol;

    use super::*;

    fn given(entries: &[&str]) -> Vec<String> {
        entries.iter().map(|entry| (*entry).to_owned()).collect()
    }

    fn port(host_port: u16, container_port: Option<u16>) -> HostPort {
        HostPort {
            host_port,
            container_port,
            protocol: PortProtocol::Tcp,
        }
    }

    /// Proves the entries are read as ports and ranges, sorted and each once, that 16 are
    /// accepted and 17 refused, and that a port of 0, of 65536, a reversed range, a sign, a
    /// space and an empty entry are each refused naming the entry. Catches a flag that takes
    /// an entry the admission could not hold a start to, or more entries than a job may name
    /// host ports.
    #[test]
    fn the_entries_are_ports_and_ranges_of_at_most_sixteen() {
        let ranges = allowed(&given(&["9000-9010", "80", "80", "5432"])).unwrap();
        let text: Vec<String> = ranges.iter().map(PortRange::to_string).collect();
        assert_eq!(text, ["80", "5432", "9000-9010"]);
        assert_eq!(allowed(&[]).unwrap(), []);
        assert_eq!(allowed(&given(&["1", "65535"])).unwrap().len(), 2);
        let sixteen: Vec<String> = (1..=16).map(|port| port.to_string()).collect();
        assert_eq!(allowed(&sixteen).unwrap().len(), 16);
        let seventeen: Vec<String> = (1..=17).map(|port| port.to_string()).collect();
        assert_eq!(allowed(&seventeen), Err(FlagError::Many));
        assert_eq!(
            FlagError::Many.to_string(),
            "--host-port: at most 16 entries"
        );
        for wrong in ["0", "65536", "90-80", "+80", " 80", "", "80-", "x"] {
            assert_eq!(
                allowed(&given(&["80", wrong])),
                Err(FlagError::Entry(wrong.to_owned())),
                "{wrong:?}"
            );
        }
        assert_eq!(
            FlagError::Entry("0".to_owned()).to_string(),
            "--host-port: not <PORT> or <LO-HI>, each from 1 to 65535, LO not above HI: \"0\""
        );
    }

    /// Proves the flag says it was given only with an entry, and reads its entries as
    /// [`allowed`] does, its refusal included. Catches a `--no-jobs` agent or a container
    /// engine that takes the flag unseen, and a flag read apart from its checks.
    #[test]
    fn the_flag_is_given_with_an_entry_and_read_as_its_entries() {
        let none = HostPortFlags::default();
        assert_eq!(none.given(), [("--host-port", false)]);
        assert_eq!(none.allowed(), Ok(Vec::new()));
        let two = HostPortFlags {
            host_port: given(&["9000-9010", "80"]),
        };
        assert_eq!(two.given(), [("--host-port", true)]);
        assert_eq!(two.allowed(), allowed(&given(&["80", "9000-9010"])));
        let wrong = HostPortFlags {
            host_port: given(&["0"]),
        };
        assert_eq!(wrong.allowed(), Err(FlagError::Entry("0".to_owned())));
    }

    /// Proves a start is admitted when every host port it names is in one of the ranges
    /// (the job port is not looked at, and two job ports of one host port pass), and refused
    /// naming the first host port none holds, and the ranges allowed or `none`. Catches an
    /// admission that bounds the job's own port, or that lets a host port through past the
    /// first refused one.
    #[test]
    fn a_host_port_outside_every_range_is_refused_by_number() {
        let ranges = allowed(&given(&["80", "9000-9010"])).unwrap();
        assert_eq!(admit(&ranges, &[]), Ok(()));
        assert_eq!(admit(&[], &[]), Ok(()));
        let inside = [port(80, Some(1)), port(9000, None), port(9010, Some(9010))];
        assert_eq!(admit(&ranges, &inside), Ok(()));
        assert_eq!(
            admit(&ranges, &[port(80, Some(8)), port(80, Some(9))]),
            Ok(())
        );
        assert_eq!(
            admit(
                &ranges,
                &[port(80, None), port(9011, Some(80)), port(81, None)]
            ),
            Err(
                "host port 9011 is not one this agent offers (--host-port: 80,9000-9010)"
                    .to_owned()
            )
        );
        assert_eq!(
            admit(&[], &[port(5432, None)]),
            Err("host port 5432 is not one this agent offers (--host-port: none)".to_owned())
        );
    }
}
