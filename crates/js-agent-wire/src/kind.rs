//! What a job start runs in: [`StartKind`], the `kind` of a [`crate::JobStartSpec`].

use serde::{Deserialize, Serialize};

/// What a job start runs in. An absent `kind` is [`StartKind::Action`], so a start of a
/// server that predates the field reads as it did; an unknown value fails to decode (inside a
/// reply, that one start reads as [`crate::Command::Unknown`]), so an agent never runs a start
/// in something it cannot name.
///
/// The spellings are the lease kinds' (`js_types::sched::LeaseKind::name`). An agent that
/// predates the field ignores it and would run a VM start as an action, so a server sends
/// `vm` only to a host whose hello reports the capability `vm.slots`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StartKind {
    /// The agent's own runtime: a container, or a native program on a Mac. The default.
    #[default]
    Action,
    /// A fresh macOS VM of its own, cloned for the start and destroyed when it ends.
    Vm,
}

impl StartKind {
    /// Whether this is [`StartKind::Action`], which a start leaves out.
    #[must_use]
    pub fn is_action(&self) -> bool {
        *self == Self::Action
    }
}
