//! The typed policy of one run, and the only ways to change it.

use std::collections::BTreeSet;

use js_bundle::ReadGrantKind;

use super::allow::MacosMajor;
use super::grant::{PinnedTree, ReadGrant};
use super::net::Net;
use super::path::AbsPath;
use super::selftest::Assurance;
use crate::xcode::HostXcode;

/// The most entries any one list of a policy holds. Each is a rule and a `-D` parameter
/// on the `sandbox-exec` command line.
pub const MAX_LISTED: usize = 64;

/// Which reads the profile permits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadModel {
    /// Reads are open (toolchains read widely) except the listed denies. Writes and the
    /// network are denied first and granted back narrowly.
    Open,
    /// Reads are an allow-list: a base set of the macOS version, the run's own places and
    /// the grants of [`Policy::allowing`]. A policy of this model has no profile until
    /// it has its grants, and those need the [`Assurance`] of a passing start-up
    /// self-test on this host ([`SelfTest`](super::SelfTest)).
    Allow,
}

/// Why a policy is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PolicyError {
    /// A list is longer than [`MAX_LISTED`]; the argument names it.
    #[error("too many entries in {0}")]
    TooMany(&'static str),
    /// Grants were given to a policy whose read model is [`ReadModel::Open`].
    #[error("read grants need the allow read model")]
    NotAllowModel,
    /// The policy has its grants already; they are given once.
    #[error("the policy already has its read grants")]
    AlreadyGranted,
    /// More than one Xcode was granted.
    #[error("more than one Xcode granted")]
    TwoXcodes,
}

/// The places the slot layout produced for one run. Nothing else is ever writable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotLayout {
    /// The run directory: writable.
    pub run: AbsPath,
    /// The user's own `/var/folders/<xx>/<hash>` entry (spelled under `/private`): writable.
    /// `None` for a layout that does not know it, which grants nothing there.
    pub uid_entry: Option<AbsPath>,
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
}

/// Everything a run may do that a caller can influence. A [`Policy`] is built once from
/// the slot layout and the agent's caps; after that a caller can only narrow it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    run: AbsPath,
    uid_entry: Option<AbsPath>,
    inputs: BTreeSet<AbsPath>,
    read_denies: BTreeSet<AbsPath>,
    net: Net,
    read_model: ReadModel,
    allow: Option<AllowGrants>,
}

/// The grants of the allow model: what the base set of one macOS major version is
/// extended with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AllowGrants {
    major: MacosMajor,
    trees: BTreeSet<PinnedTree>,
    xcode: Option<HostXcode>,
    kinds: Vec<ReadGrantKind>,
}

impl AllowGrants {
    fn new(major: MacosMajor, grants: Vec<ReadGrant>) -> Result<Self, PolicyError> {
        let mut allow = Self {
            major,
            trees: BTreeSet::new(),
            xcode: None,
            kinds: Vec::new(),
        };
        for grant in grants {
            match grant {
                ReadGrant::Tree(tree) => {
                    allow.trees.insert(tree);
                }
                ReadGrant::Xcode(xcode) => {
                    if allow.xcode.replace(xcode).is_some() {
                        return Err(PolicyError::TwoXcodes);
                    }
                }
                ReadGrant::Kind(kind) => {
                    if !allow.kinds.contains(&kind) {
                        allow.kinds.push(kind);
                    }
                }
            }
        }
        bounded(allow.trees.len(), "trees")?;
        Ok(allow)
    }

    pub(super) fn major(&self) -> MacosMajor {
        self.major
    }

    pub(super) fn trees(&self) -> &BTreeSet<PinnedTree> {
        &self.trees
    }

    pub(super) fn xcode(&self) -> Option<&HostXcode> {
        self.xcode.as_ref()
    }

    pub(super) fn kinds(&self) -> &[ReadGrantKind] {
        &self.kinds
    }
}

impl Policy {
    /// The widest policy for `layout` that `caps` allow.
    ///
    /// # Errors
    /// [`PolicyError::TooMany`] for a list over [`MAX_LISTED`].
    pub fn base(layout: SlotLayout, caps: Caps) -> Result<Self, PolicyError> {
        let policy = Self {
            run: layout.run,
            uid_entry: layout.uid_entry,
            inputs: layout.inputs.into_iter().collect(),
            read_denies: layout.read_denies.into_iter().collect(),
            net: caps.net,
            read_model: caps.read_model,
            allow: None,
        };
        policy.check()?;
        Ok(policy)
    }

    /// This policy with its read grants, for the allow model. `assurance` is the proof
    /// that the start-up self-test passed on this host, for the macOS version whose base
    /// set the profile gets. Grants are made of proof objects ([`ReadGrant`]), so a caller
    /// cannot name a path; they can still only be given once, and [`Narrowing`] can take
    /// nothing back from them but add denies after them.
    ///
    /// # Errors
    /// [`PolicyError::NotAllowModel`] for an open policy, [`PolicyError::AlreadyGranted`]
    /// for a second call, [`PolicyError::TwoXcodes`], and [`PolicyError::TooMany`] for a
    /// list over [`MAX_LISTED`].
    pub fn allowing(
        self,
        assurance: &Assurance,
        grants: Vec<ReadGrant>,
    ) -> Result<Self, PolicyError> {
        self.granting(assurance.major(), grants)
    }

    /// The same, for the self-test's own probes, which run before there is an assurance.
    pub(super) fn granting(
        mut self,
        major: MacosMajor,
        grants: Vec<ReadGrant>,
    ) -> Result<Self, PolicyError> {
        if self.read_model != ReadModel::Allow {
            return Err(PolicyError::NotAllowModel);
        }
        if self.allow.is_some() {
            return Err(PolicyError::AlreadyGranted);
        }
        self.allow = Some(AllowGrants::new(major, grants)?);
        Ok(self)
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
        bounded(self.read_denies.len(), "read denies")
    }

    pub(super) fn allow(&self) -> Option<&AllowGrants> {
        self.allow.as_ref()
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
}

fn bounded(len: usize, what: &'static str) -> Result<(), PolicyError> {
    if len > MAX_LISTED {
        return Err(PolicyError::TooMany(what));
    }
    Ok(())
}

/// A request to narrow a policy. Every field can only take something away or add a
/// deny: a level is met with the policy's level (the lower wins), the user-folder grant
/// can be dropped, and paths can be added to the denies. There is no field
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
}
