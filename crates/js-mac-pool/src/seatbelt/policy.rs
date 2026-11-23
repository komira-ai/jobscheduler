//! The typed policy of one run, and the only ways to change it.

use std::collections::BTreeSet;

use js_bundle::ReadGrantKind;

use super::allow::MacosMajor;
use super::grant::{PinnedTree, ReadGrant};
use super::net::Net;
use super::path::AbsPath;
use super::selftest::Assurance;
use super::trees::MountTrees;
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
    /// A mount grant is the trees' mount root, above it or below it: its read grant would
    /// reopen what the trees' rule denies.
    #[error("a mount grant overlaps the trees' mount root")]
    MountOverTrees,
    /// Trees were given to a policy of the allow read model, whose read grants are made of
    /// proof objects only ([`Policy::allowing`]).
    #[error("trees on a mount need the open read model")]
    TreesNeedOpenModel,
    /// A mount grant is a read-denied path or lies above one: its grant, emitted after the
    /// read denies, would reopen the denied path. (A grant below a denied path is the
    /// intended way to open part of it.)
    #[error("a mount grant holds a read-denied path")]
    MountOverReadDeny,
    /// A homes root is the run directory or lies below it: the run directory's read grant,
    /// emitted after the homes' denies, would reopen it.
    #[error("a homes root lies in the run directory")]
    HomesInRun,
}

/// One host directory a run may use, granted by its canonical path (Seatbelt matches the
/// resolved path, so a link in the run directory is covered by the grant of its target).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct MountGrant {
    /// The canonical host path.
    pub path: AbsPath,
    /// Whether the run may also write below `path`.
    pub write: bool,
    /// Whether the run may not execute or map as executable anything below `path`.
    pub no_exec: bool,
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
    /// Host directories granted to this run, after the read denies (so a mount under a
    /// denied mount root is readable again) and before the network rules.
    pub mounts: Vec<MountGrant>,
    /// The run's own trees on the store's shared mount, if the host has one: nothing else
    /// below the mount's root is readable. Rendered before the read denies, so a read deny
    /// also holds inside a tree.
    pub trees: Option<MountTrees>,
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
    homes: BTreeSet<AbsPath>,
    mounts: BTreeSet<MountGrant>,
    trees: Option<MountTrees>,
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
    /// [`PolicyError::TooMany`] for a list over [`MAX_LISTED`],
    /// [`PolicyError::MountOverReadDeny`] for a mount grant at or above a read-denied path,
    /// [`PolicyError::MountOverTrees`] for a mount grant that overlaps the trees' mount
    /// root, and [`PolicyError::TreesNeedOpenModel`] for trees under the allow read model.
    pub fn base(layout: SlotLayout, caps: Caps) -> Result<Self, PolicyError> {
        let policy = Self {
            run: layout.run,
            uid_entry: layout.uid_entry,
            inputs: layout.inputs.into_iter().collect(),
            read_denies: layout.read_denies.into_iter().collect(),
            homes: BTreeSet::new(),
            mounts: layout.mounts.into_iter().collect(),
            trees: layout.trees,
            net: caps.net,
            read_model: caps.read_model,
            allow: None,
        };
        policy.check()?;
        if let Some(trees) = &policy.trees {
            if policy.read_model != ReadModel::Open {
                return Err(PolicyError::TreesNeedOpenModel);
            }
            let root = trees.root();
            if policy
                .mounts
                .iter()
                .any(|mount| mount.path.starts_with(root) || root.starts_with(&mount.path))
            {
                return Err(PolicyError::MountOverTrees);
            }
        }
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
    /// [`PolicyError::TooMany`] for a list that grows over [`MAX_LISTED`],
    /// [`PolicyError::MountOverReadDeny`] for a read deny or a homes root added at or below
    /// a mount grant (the grant would reopen it), and [`PolicyError::HomesInRun`] for a
    /// homes root at or below the run directory.
    pub fn narrowed(mut self, narrowing: Narrowing) -> Result<Self, PolicyError> {
        if let Some(net) = narrowing.net {
            self.net = self.net.narrow(net);
        }
        if narrowing.drop_uid_entry {
            self.uid_entry = None;
        }
        self.inputs.extend(narrowing.protect);
        self.read_denies.extend(narrowing.deny_read);
        self.homes.extend(narrowing.deny_homes);
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
        bounded(self.homes.len(), "homes")?;
        bounded(self.mounts.len(), "mounts")?;
        let over = |mount: &MountGrant| {
            self.read_denies
                .iter()
                .chain(&self.homes)
                .any(|deny| deny.starts_with(&mount.path))
        };
        if self.mounts.iter().any(over) {
            return Err(PolicyError::MountOverReadDeny);
        }
        if self.homes.iter().any(|home| home.starts_with(&self.run)) {
            return Err(PolicyError::HomesInRun);
        }
        Ok(())
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

    pub(super) fn homes(&self) -> &BTreeSet<AbsPath> {
        &self.homes
    }

    pub(super) fn mounts(&self) -> &BTreeSet<MountGrant> {
        &self.mounts
    }

    pub(super) fn trees(&self) -> Option<&MountTrees> {
        self.trees.as_ref()
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
/// that adds a grant or raises a level (the run directory's read grant that follows the
/// homes' denies gives back only what they took).
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
    /// Roots of users' homes to read-deny, the run directory below one excepted: their
    /// denies come before every read grant of the profile, so the run directory (granted
    /// back right after them, with the names of its ancestors), the trees and the mount
    /// grants still open what they name, and the read denies still close what they name.
    /// A root at or below the run directory is refused ([`PolicyError::HomesInRun`]), and
    /// so is a mount grant at or above a root ([`PolicyError::MountOverReadDeny`]).
    pub deny_homes: Vec<AbsPath>,
}
