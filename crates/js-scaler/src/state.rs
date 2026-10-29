//! What a step returns: the actions the caller runs, and the state to give the next step.

use std::collections::BTreeMap;
use std::time::Duration;

use js_elastic::{AcquireId, Dimension, HostRef, Offer};
use js_types::RequestId;

use crate::record::Records;
use crate::settings::Blocked;
use crate::time::Millis;

/// One acquire to make: everything but the boot payload, which the caller renders from
/// its network mode and the join token it mints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcquirePlan {
    /// The acquire's id. Asking again with it launches nothing new.
    pub acquire: AcquireId,
    /// Which call this is for the acquire id: 0 for the first, one more for each retry of a
    /// launch the provider told to wait. The answer carries it back.
    pub attempt: u32,
    /// The pool the machine is for.
    pub pool: String,
    /// The name to give the machine, chosen before the launch.
    pub name: String,
    /// What to launch: the offer the machine's record holds, at the stored price.
    pub offer: Offer,
}

/// One thing the caller does. A step lists them in the order to run them: the
/// [`Action::Save`] first, so the records are durable before any call they describe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Write these records, whole, before running any later action of the step. Sent when
    /// the step changed them; it carries everything the scaler keeps (the machines and the
    /// cooldowns), so a caller cannot write part of it.
    Save(Box<Records>),
    /// Mint a join token for the host, valid for `valid_for`. Sent for a new acquire id
    /// and for the retry of a deferred one, never for a record found after a restart.
    Mint {
        /// The host's name.
        host: String,
        /// How long the token lives.
        valid_for: Duration,
    },
    /// Call `acquire`; its answer comes back in the next step's input.
    Acquire(Box<AcquirePlan>),
    /// `Registry::drain` the host.
    Drain(String),
    /// `Registry::undrain` the host.
    Undrain(String),
    /// `Registry::release_host` the host: it marks it only while it is still quiet.
    ReleaseHost(String),
    /// Revoke the host's join token. Sent once per machine.
    Revoke(String),
    /// Call `release` on the machine; its answer is read back in the next step's input.
    /// Sent every step until the answer is `Gone`.
    Release {
        /// The host's name.
        host: String,
        /// The provider's id for it.
        id: HostRef,
    },
    /// Forget the host's network device or key.
    ForgetNetwork(String),
    /// `Registry::remove` the host.
    RemoveHost(String),
}

/// A launch a pool in `Plan` mode would make.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanLine {
    /// The pool.
    pub pool: String,
    /// The request the machine would serve.
    pub request: RequestId,
    /// The offer it would launch.
    pub offer: String,
    /// Its zone.
    pub zone: Option<String>,
    /// Its price per hour.
    pub price_cents_per_hour: u64,
}

/// What the scaler keeps between steps: its [`Records`], which are the only part that is
/// written to disk, and what its last step found, for a status page.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct State {
    /// The machines and cooldowns.
    pub records: Records,
    /// Why each blocked pool launches nothing now.
    pub blocked: BTreeMap<String, Blocked>,
    /// What `Plan` pools would launch.
    pub plan: Vec<PlanLine>,
    /// Requests that no offer of any pool fits, with the [`Dimension`] the closest offer
    /// fails.
    pub no_offer: BTreeMap<RequestId, Dimension>,
    /// How many times an input asked for a transition the state machine does not have.
    /// Each was refused and changed nothing.
    pub refused: u64,
    /// Since when each registered host has been quiet. Not written to disk: after a
    /// restart the idle time counts again from the first step.
    pub(crate) quiet: BTreeMap<String, Millis>,
    /// Since when each registered host the registry does not hear from (lost, or waiting
    /// to say hello again) has been silent, as this process first saw it. A silent host is
    /// held and counted, never released on its silence, so this is the age a status page
    /// shows. Not written to disk: after a restart the age counts again from the first step.
    pub silent: BTreeMap<String, Millis>,
    /// The machines released this step because their host was silent, its leases given up,
    /// for `unknown_hold`. The shell logs each: a paid machine was released on silence.
    pub silent_released: Vec<String>,
    /// Since when each registered machine has been missing from a complete registry view,
    /// unbroken. Not written to disk: after a restart the count starts again.
    pub(crate) missing: BTreeMap<String, Millis>,
}

impl State {
    /// The state of a scaler that starts from `records`.
    #[must_use]
    pub fn new(records: Records) -> Self {
        Self {
            records,
            ..Self::default()
        }
    }
}
