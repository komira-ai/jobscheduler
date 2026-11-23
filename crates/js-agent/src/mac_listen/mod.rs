//! The Mac's own services on loopback, which no native job may reach.
//!
//! A Mac has one loopback, shared by every user: a job whose profile allows loopback can
//! connect to any listener bound to `127.0.0.1`, `::1` or a wildcard address, whoever holds
//! it. The job's profile therefore denies, by port, every listener of the host that a
//! connect to loopback reaches and that no job account holds ([`denied_ports`]): the ports
//! the agent is configured with (`--native-host-port`, the fixed ports of the host's
//! services, denied whether or not the service is up, so a job cannot take one while its
//! service restarts), and the port of every such listener the listing finds when the run
//! starts (so a service no flag names is denied too, whatever its port).
//!
//! Only TCP listeners are listed: a host service that answers on UDP alone is denied only when
//! its port is a `--native-host-port` (whose deny covers every protocol).
//!
//! A listener of a job account (a uid of the pool's slots, effective or real) is not denied:
//! two jobs of one Mac can reach each other's loopback listeners, which two Linux jobs, each
//! in its own network namespace, cannot.
//!
//! The decisions are here, on every platform, and are tested over a fake table; the macOS
//! table (`libproc`: every process's TCP sockets in the listening state) is `MacListeners`.

#[cfg(target_os = "macos")]
mod macos;
#[cfg(test)]
mod tests;

use std::collections::BTreeSet;
use std::io;
use std::net::IpAddr;

use crate::mac_accounts::Slots;

#[cfg(target_os = "macos")]
pub use macos::MacListeners;

/// One TCP socket in the listening state, and the process that holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listener {
    /// The local address it is bound to.
    pub addr: IpAddr,
    /// The local port.
    pub port: u16,
    /// The process that holds it.
    pub pid: i32,
    /// The process's effective uid.
    pub uid: u32,
    /// The process's real uid.
    pub ruid: u32,
    /// The process's name, for the log.
    pub name: String,
}

/// The TCP listeners of the machine: the operating system's on a Mac, a fake in the tests.
pub trait ListenerTable: Send + Sync {
    /// Every TCP socket in the listening state, with the process that holds it.
    ///
    /// # Errors
    ///
    /// When a process's sockets cannot be read: a listener could be missed, so the caller
    /// starts no run on the listing.
    fn listeners(&self) -> io::Result<Vec<Listener>>;
}

/// Whether a connect to loopback reaches a listener bound to `addr`: a loopback address
/// (`127.0.0.0/8`, `::1`), a wildcard (`0.0.0.0`, `::`), or either spelled as an IPv4-mapped
/// IPv6 address.
#[must_use]
pub fn on_loopback(addr: IpAddr) -> bool {
    match addr {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_unspecified(),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => on_loopback(IpAddr::V4(v4)),
            None => v6.is_loopback() || v6.is_unspecified(),
        },
    }
}

/// The listeners of `listeners` that are the host's and that a job reaches on loopback:
/// bound to an address [`on_loopback`] takes, and held by a process neither of whose uids
/// (effective, real) is a slot of `jobs`.
#[must_use]
pub fn host_listeners(listeners: Vec<Listener>, jobs: Slots) -> Vec<Listener> {
    let range = jobs.range();
    listeners
        .into_iter()
        .filter(|found| on_loopback(found.addr))
        .filter(|found| {
            ![found.uid, found.ruid]
                .iter()
                .any(|uid| range.contains(uid))
        })
        .collect()
}

/// The ports a run's profile denies: every port of `configured` and the port of every host
/// listener of `table` ([`host_listeners`]), each once, lowest first.
///
/// # Errors
///
/// The table's, when it cannot be read.
pub fn denied_ports(
    table: &dyn ListenerTable,
    configured: &BTreeSet<u16>,
    jobs: Slots,
) -> io::Result<Vec<u16>> {
    let found = host_listeners(table.listeners()?, jobs);
    Ok(ports(configured, &found))
}

/// Every port of `configured` and of the host listeners `found`, each once, lowest first.
#[must_use]
pub fn ports(configured: &BTreeSet<u16>, found: &[Listener]) -> Vec<u16> {
    let all: BTreeSet<u16> = configured
        .iter()
        .copied()
        .chain(found.iter().map(|listener| listener.port))
        .collect();
    all.into_iter().collect()
}

/// What the log says of a host listener: `name (pid P, uid U) on ADDR:PORT`, with the
/// address of an IPv6 listener in brackets.
#[must_use]
pub fn describe(listener: &Listener) -> String {
    let at = std::net::SocketAddr::new(listener.addr, listener.port);
    format!(
        "{} (pid {}, uid {}) on {at}",
        listener.name, listener.pid, listener.uid
    )
}
