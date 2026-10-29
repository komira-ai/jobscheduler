//! The typed policy of one run, and the only ways to change it.

use std::collections::BTreeSet;
use std::net::Ipv4Addr;

use super::net::Net;
use super::path::AbsPath;

/// The most entries any one list of a policy holds. Each is a rule and a `-D` parameter
/// on the `sandbox-exec` command line.
pub const MAX_LISTED: usize = 64;

/// The cloud instance metadata address. Under [`Net::On`] it is always denied.
pub const METADATA_ADDR: Ipv4Addr = Ipv4Addr::new(169, 254, 169, 254);

/// Which reads the profile permits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadModel {
    /// Reads are open (toolchains read widely) except the listed denies. Writes and the
    /// network are denied first and granted back narrowly.
    Open,
    /// Reads are an allow-list. Its builder is not written: it needs a trace on the
    /// farm's macOS version, so [`Profile::for_policy`](super::Profile::for_policy)
    /// refuses it.
    Allow,
}

/// Why a policy is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PolicyError {
    /// A list is longer than [`MAX_LISTED`]; the argument names it.
    #[error("too many entries in {0}")]
    TooMany(&'static str),
}

/// The places the slot layout produced for one run. Nothing else is ever writable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotLayout {
    /// The run directory: writable.
    pub run: AbsPath,
    /// The user's own `/var/folders/<xx>/<hash>` entry (spelled under `/private`): writable.
    pub uid_entry: AbsPath,
    /// Read-only inputs: write-denied after the grants, so they stay unwritable inside
    /// the run directory.
    pub inputs: Vec<AbsPath>,
    /// Paths a run may not read (the agent's state, the store, credentials).
    pub read_denies: Vec<AbsPath>,
}

/// What the agent allows at most, from its flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caps {
    /// The highest network level any run gets.
    pub net: Net,
    /// The read model of the pool.
    pub read_model: ReadModel,
    /// Addresses denied under [`Net::On`] besides [`METADATA_ADDR`] (the scheduler
    /// endpoint, the node's own addresses). Literal IPv4 addresses only: a host name
    /// cannot be matched, so this is best effort and a packet filter is the real control.
    pub deny_egress: Vec<Ipv4Addr>,
}

/// Everything a run may do that a caller can influence. A [`Policy`] is built once from
/// the slot layout and the agent's caps; after that a caller can only narrow it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    run: AbsPath,
    uid_entry: Option<AbsPath>,
    inputs: BTreeSet<AbsPath>,
    read_denies: BTreeSet<AbsPath>,
    egress: BTreeSet<Ipv4Addr>,
    net: Net,
    read_model: ReadModel,
}

impl Policy {
    /// The widest policy for `layout` that `caps` allow.
    ///
    /// # Errors
    /// [`PolicyError::TooMany`] for a list over [`MAX_LISTED`].
    pub fn base(layout: SlotLayout, caps: Caps) -> Result<Self, PolicyError> {
        let policy = Self {
            run: layout.run,
            uid_entry: Some(layout.uid_entry),
            inputs: layout.inputs.into_iter().collect(),
            read_denies: layout.read_denies.into_iter().collect(),
            egress: caps.deny_egress.into_iter().collect(),
            net: caps.net,
            read_model: caps.read_model,
        };
        policy.check()?;
        Ok(policy)
    }

    /// This policy with `narrowing` applied. No narrowing widens anything.
    ///
    /// # Errors
    /// [`PolicyError::TooMany`] for a list that grows over [`MAX_LISTED`].
    pub fn narrowed(mut self, narrowing: Narrowing) -> Result<Self, PolicyError> {
        if let Some(net) = narrowing.net {
            self.net = self.net.narrow(net);
        }
        if narrowing.drop_uid_entry {
            self.uid_entry = None;
        }
        self.inputs.extend(narrowing.protect);
        self.read_denies.extend(narrowing.deny_read);
        self.egress.extend(narrowing.deny_egress);
        self.check()?;
        Ok(self)
    }

    /// The network level.
    #[must_use]
    pub fn net(&self) -> &Net {
        &self.net
    }

    /// The read model.
    #[must_use]
    pub fn read_model(&self) -> ReadModel {
        self.read_model
    }

    fn check(&self) -> Result<(), PolicyError> {
        bounded(self.inputs.len(), "inputs")?;
        bounded(self.read_denies.len(), "read denies")?;
        bounded(self.egress.len(), "denied egress")
    }

    pub(super) fn run(&self) -> &AbsPath {
        &self.run
    }

    pub(super) fn uid_entry(&self) -> Option<&AbsPath> {
        self.uid_entry.as_ref()
    }

    pub(super) fn inputs(&self) -> &BTreeSet<AbsPath> {
        &self.inputs
    }

    pub(super) fn read_denies(&self) -> &BTreeSet<AbsPath> {
        &self.read_denies
    }

    pub(super) fn egress(&self) -> &BTreeSet<Ipv4Addr> {
        &self.egress
    }
}

fn bounded(len: usize, what: &'static str) -> Result<(), PolicyError> {
    if len > MAX_LISTED {
        return Err(PolicyError::TooMany(what));
    }
    Ok(())
}

/// A request to narrow a policy. Every field can only take something away or add a
/// deny: a level is met with the policy's level (the lower wins), the user-folder grant
/// can be dropped, and paths and addresses can be added to the denies. There is no field
/// that adds a grant or raises a level.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Narrowing {
    /// A level to meet with the policy's.
    pub net: Option<Net>,
    /// Drop the write grant of the user's `/var/folders` entry.
    pub drop_uid_entry: bool,
    /// More paths to write-protect after the grants.
    pub protect: Vec<AbsPath>,
    /// More paths to read-deny.
    pub deny_read: Vec<AbsPath>,
    /// More addresses to deny under [`Net::On`].
    pub deny_egress: Vec<Ipv4Addr>,
}
