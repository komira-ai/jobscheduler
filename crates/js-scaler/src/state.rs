//! What a step returns: the actions the caller runs, and the state to give the next step.

use std::collections::{BTreeMap, BTreeSet};
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
    /// The lifetime the cloud is to enforce on the machine ([`Settings::lifetime`](crate::Settings::lifetime)), if the
    /// pool has one. A provider with a native limit sets it at launch; one without ignores
    /// it. It is computed from the pool's settings at each call, so a retry carries the
    /// settings of its time; nothing about it is stored.
    pub lifetime: Option<Duration>,
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
    /// `Registry::retire` the host: drain it whether or not it is busy, because it has
    /// reached `max_age`. The mark stops new leases and cuts none; the release that follows
    /// is asked only when the host is quiet. Sent again whenever the registry does not show
    /// the mark, so a crash between the step and the call loses nothing.
    Retire(String),
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
    /// to say hello again) has been silent, as the scaler first saw it. A silent host is
    /// held and counted until it books nothing and has been silent for `unknown_hold`, so
    /// this is the age a status page shows. Kept in [`Records::silent_since`] too, so a
    /// restart does not start it again; the time the scaler was down does not count, see
    /// [`Records::last_step`].
    pub silent: BTreeMap<String, Millis>,
    /// The machines released this step because their host was silent for `unknown_hold`
    /// and booked nothing (its leases given up, or marked releasing). The shell logs each:
    /// a paid machine was released on silence.
    pub silent_released: Vec<String>,
    /// Since when each registered machine has been missing from complete registry views,
    /// unbroken. Kept in [`Records::missing_since`] too.
    pub(crate) missing: BTreeMap<String, Millis>,
    /// Since when each booting machine past `boot_timeout` has been unheard from on
    /// complete registry views, unbroken. Not written to disk, on purpose: a booting
    /// machine is given up on only after a hold in this process, so a restart gives it the
    /// whole hold again, which is when its agent is about to say hello to the new registry.
    pub(crate) unheard: BTreeMap<String, Millis>,
    /// Whether this state was read from records and has not yet taken a step: its first
    /// step moves the silent and missing times past the time the scaler was down.
    pub(crate) restored: bool,
    /// The restored registered machines the registry has not yet heard from since this
    /// process started, each with the instant of the first step, which is the
    /// restart. Until its host says hello, up to `boot_timeout` from that instant, such a
    /// machine covers a request its stored offer fits, so a request it would serve does not
    /// make a second machine (see `Pass::cover`). Not written to disk, on purpose: a restart
    /// gives every machine the whole window again, which is when its agent is about to say
    /// hello to the new registry.
    ///
    /// "Restart" is the first step after the process starts; each restart re-stamps every
    /// window, so a server that keeps crashing within `boot_timeout` can hold a launch for
    /// as long as it keeps crashing, which is bounded per restart and accepted. A restored
    /// machine that is dead holds a request its offer fits for up to `boot_timeout`: a
    /// stated trade-off for not launching twice for a machine that is only slow to say
    /// hello. A draining machine is not awaited: the step had already decided to take it
    /// out, and the registry places no new lease on it, so it would hold a request for the
    /// whole window to no purpose (the request launches anyway at its end).
    pub(crate) awaiting: BTreeMap<String, Millis>,
    /// Since when each registered or draining machine's host has been heard from at every
    /// step, unbroken. Not written to disk, on purpose: a restart starts the time again, which
    /// only delays the end of the brake's count of failures.
    pub(crate) heard: BTreeMap<String, Millis>,
    /// The machines whose host has been heard from for `boot_fail_reset_after` and so already
    /// ended the count of failures once. Not written to disk: after a restart a machine can
    /// end the count once more.
    pub(crate) credited: BTreeSet<String>,
}

impl State {
    /// The state of a scaler that starts from `records`.
    #[must_use]
    pub fn new(records: Records) -> Self {
        Self {
            silent: records.silent_since.clone(),
            missing: records.missing_since.clone(),
            records,
            restored: true,
            ..Self::default()
        }
    }
}
