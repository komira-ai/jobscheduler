//! The scaler's records: one per machine it has launched or found, and the cooldowns of
//! its pools. They are the scaler's memory across a restart and the only thing about a
//! machine that outlives the process, so they are written before the call they describe.

use std::collections::BTreeMap;

use js_elastic::{AcquireId, GpuClass, Offer, PriceUnit};
use js_types::ExecMode;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::time::Millis;

/// Where one machine is in its life (section 6.2 of the design).
///
/// ```text
/// Planned -> Launching -> Booting -> Registered <-> Draining -> Releasing -> Gone
///               |  \-> Refused           \-> Releasing (boot_timeout, lost)
///               \-> Unresolved -> Booting | Failed -> Releasing (listed late)
/// Orphan -> Gone (two lists, `unknown_hold` apart, no longer show it)
/// ```
///
/// The design's "Failed (Refused)" is [`Phase::Refused`] here: no machine exists, nothing
/// is counted, and the record is dropped at the end of the step that made it. [`Phase::Failed`] is an acquire that was never seen; it stays
/// counted until two clean lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// A launch was decided and the provider said to wait; nothing was sent.
    Planned,
    /// The record is written and the acquire is out or about to go.
    Launching,
    /// The acquire got no answer, so a machine may exist.
    Unresolved,
    /// The provider has the machine; the agent has not said hello.
    Booting,
    /// The agent said hello and the host is the registry's.
    Registered,
    /// The host is marked and holds no work; it is released or taken back.
    Draining,
    /// The machine is being terminated; the answer is read back until it is gone.
    Releasing,
    /// The provider refused the acquire. No machine exists; the record goes at the end of
    /// the step.
    Refused,
    /// The acquire was never seen after `unknown_hold`. Counted until two clean lists.
    Failed,
    /// The machine is gone, confirmed by a read by id (or, for an orphan, by two clean
    /// lists). Kept until its billed time has left the day window.
    Gone,
    /// The provider lists a machine of ours that no record knows, or a launch answer named
    /// a machine it could not confirm released. Counted and reported, never released by
    /// default.
    Orphan,
}

/// Every transition of the state machine. One not here is refused and counted.
const TRANSITIONS: &[(Phase, Phase)] = &[
    (Phase::Planned, Phase::Launching),
    (Phase::Launching, Phase::Planned),
    (Phase::Launching, Phase::Booting),
    (Phase::Launching, Phase::Refused),
    (Phase::Launching, Phase::Unresolved),
    (Phase::Launching, Phase::Failed),
    (Phase::Unresolved, Phase::Booting),
    (Phase::Unresolved, Phase::Failed),
    (Phase::Booting, Phase::Registered),
    (Phase::Booting, Phase::Releasing),
    (Phase::Registered, Phase::Draining),
    (Phase::Registered, Phase::Releasing),
    (Phase::Draining, Phase::Registered),
    (Phase::Draining, Phase::Releasing),
    (Phase::Releasing, Phase::Gone),
    (Phase::Failed, Phase::Releasing),
    (Phase::Orphan, Phase::Gone),
];

impl Phase {
    /// Whether the machine may move from this phase to `to`.
    #[must_use]
    pub fn may_become(self, to: Self) -> bool {
        TRANSITIONS.contains(&(self, to))
    }
}

/// The mode a machine runs jobs in, as the records file spells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StoredExec {
    /// In a container.
    Container,
    /// Natively under a `sandbox-exec` profile.
    SandboxExec,
}

/// The shape of the offer a machine was launched from, kept so the machine can be matched
/// to demand and its hello compared with it after the catalog has changed. The price is
/// kept on the [`Record`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredOffer {
    /// The provider's id for the offer.
    pub id: String,
    /// The zone.
    pub zone: Option<String>,
    /// CPU in thousandths of a core.
    pub cpu_millis: u64,
    /// Memory in bytes.
    pub memory_bytes: u64,
    /// Disk in bytes.
    pub disk_bytes: u64,
    /// GPUs.
    pub gpus: u32,
    /// The GPU class, as the agent spells it.
    pub gpu_class: Option<String>,
    /// The operating system.
    pub os: String,
    /// The CPU architecture.
    pub arch: String,
    /// The execution mode.
    pub exec: StoredExec,
}

impl StoredOffer {
    /// The shape of `offer`.
    #[must_use]
    pub fn from_offer(offer: &Offer) -> Self {
        Self {
            id: offer.id.clone(),
            zone: offer.zone.clone(),
            cpu_millis: offer.cpu_millis,
            memory_bytes: offer.memory_bytes,
            disk_bytes: offer.disk_bytes,
            gpus: offer.gpus,
            gpu_class: offer.gpu_class.as_ref().map(|c| c.as_str().to_owned()),
            os: offer.os.clone(),
            arch: offer.arch.clone(),
            exec: match offer.exec {
                ExecMode::Container => StoredExec::Container,
                ExecMode::SandboxExec => StoredExec::SandboxExec,
            },
        }
    }

    /// A shape that offers nothing, for a machine found with no offer to read it from.
    #[must_use]
    pub fn nothing(id: &str, zone: Option<String>) -> Self {
        Self {
            id: id.to_owned(),
            zone,
            cpu_millis: 0,
            memory_bytes: 0,
            disk_bytes: 0,
            gpus: 0,
            gpu_class: None,
            os: String::new(),
            arch: String::new(),
            exec: StoredExec::Container,
        }
    }

    /// The offer this shape was taken from, priced at `price`, available.
    #[must_use]
    pub fn to_offer(&self, price: u64) -> Offer {
        Offer {
            id: self.id.clone(),
            zone: self.zone.clone(),
            cpu_millis: self.cpu_millis,
            memory_bytes: self.memory_bytes,
            disk_bytes: self.disk_bytes,
            gpus: self.gpus,
            gpu_class: self
                .gpu_class
                .as_deref()
                .and_then(|class| GpuClass::parse(class).ok()),
            os: self.os.clone(),
            arch: self.arch.clone(),
            exec: match self.exec {
                StoredExec::Container => ExecMode::Container,
                StoredExec::SandboxExec => ExecMode::SandboxExec,
            },
            price_cents_per_hour: Some(price),
            price_unit: PriceUnit::UsCents,
            available: true,
        }
    }
}

/// The acquire id is kept as text in the file and checked when the file is read.
mod acquire_text {
    use super::{AcquireId, Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(id: &Option<AcquireId>, out: S) -> Result<S::Ok, S::Error> {
        id.as_ref().map(AcquireId::as_str).serialize(out)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(input: D) -> Result<Option<AcquireId>, D::Error> {
        Option::<String>::deserialize(input)?
            .map(|text| AcquireId::new(&text).map_err(serde::de::Error::custom))
            .transpose()
    }
}

/// One machine the scaler has launched or found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    /// The pool it belongs to.
    pub pool: String,
    /// The id of the acquire that launched it. `None` for an orphan whose marks carry no
    /// usable one.
    #[serde(with = "acquire_text")]
    pub acquire: Option<AcquireId>,
    /// The name it was given, which it registers under. Empty for an orphan with no name.
    pub name: String,
    /// The shape of its offer.
    pub offer: StoredOffer,
    /// The price per hour when it was launched. A later catalog change does not move it.
    pub price_cents_per_hour: u64,
    /// Where it is in its life.
    pub phase: Phase,
    /// When it entered `phase`.
    pub since: Millis,
    /// When it was launched (found, for an orphan). The day window and the billed time
    /// count from here.
    pub launched_at: Millis,
    /// When the join token minted for it expires. It holds no secret.
    pub token_expires: Option<Millis>,
    /// The provider's id for it, once known.
    pub cloud_id: Option<String>,
    /// For `Planned`: when the provider allows the launch.
    pub retry_at: Option<Millis>,
    /// How many times the launch has been retried after a `Deferred` answer. The call for
    /// each attempt carries its number, so a replayed answer for an earlier one is known.
    pub attempt: u32,
    /// When it was confirmed gone.
    pub gone_at: Option<Millis>,
    /// Whether its agent said hello.
    pub registered: bool,
    /// Whether its hello was below its offer: it is drained and released as soon as it is
    /// quiet and is never taken back for a request.
    pub condemned: bool,
    /// For `Failed` and `Orphan`: how many clean lists have shown no instance of it. A list
    /// that shows an orphan starts the count over.
    pub clean_lists: u8,
    /// For `Failed` and `Orphan`: when the last of them was taken.
    pub last_clean: Option<Millis>,
}

impl Record {
    /// Whether the machine counts toward `max_instances` and `max_spend_per_hour`: it
    /// exists or may exist. A refused acquire, a gone machine and a failed acquire that
    /// two clean lists have settled do not.
    #[must_use]
    pub fn counts(&self) -> bool {
        match self.phase {
            Phase::Gone | Phase::Refused => false,
            Phase::Failed => self.clean_lists < 2,
            _ => true,
        }
    }

    /// Moves to `to` and says so, or does nothing and answers `false` when the state
    /// machine does not allow it.
    pub(crate) fn go(&mut self, to: Phase, now: Millis) -> bool {
        let allowed = self.phase.may_become(to);
        if allowed {
            self.phase = to;
            self.since = now;
        }
        allowed
    }
}

/// Why a pool launches nothing for a while.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CooldownReason {
    /// A boot failed.
    BootFailed,
    /// The provider's quota was reached.
    Quota,
    /// The provider refused the request.
    Denied,
    /// The provider said the request fits nothing it has.
    Shape,
}

/// A pool that launches nothing until `until`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cooldown {
    /// The first instant the pool may launch again.
    pub until: Millis,
    /// Why.
    pub reason: CooldownReason,
}

/// The version of the records file this writer writes. Version 2 added the silent and
/// missing times and the time of the last write.
pub const VERSION: u32 = 2;

/// The first version that has the silent and missing times and the time of the last write.
/// A file of an older version is read with them empty.
const TIMES_FROM: u32 = 2;

/// The oldest version a reader still reads. A file of an older version is read as if it
/// had been written by this one with the fields it lacks empty, and the next save writes
/// it as [`VERSION`].
pub const OLDEST: u32 = 1;

/// Everything the scaler remembers. The caller writes all of it, as one file, whenever a
/// step returns [`Action::Save`](crate::Action::Save), before it runs any other action of
/// the step: nothing the scaler needs after a restart lives outside it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Records {
    /// The machines, oldest first.
    pub instances: Vec<Record>,
    /// The pools that are waiting out a failure, by name.
    pub cooldowns: BTreeMap<String, Cooldown>,
    /// Since when each registered machine whose host the registry shows but does not hear
    /// from has been silent, as the scaler first saw it. Written so that a scaler that
    /// restarts more often than `unknown_hold` still reaches the hold. The time the scaler
    /// was down is not part of it: see [`Records::last_step`].
    pub silent_since: BTreeMap<String, Millis>,
    /// Since when each registered machine the registry does not show has been missing from
    /// complete views, unbroken, for the same reason. A restart of the scaler does not
    /// reset it, and nor does an incomplete view; a step that shows the host does.
    pub missing_since: BTreeMap<String, Millis>,
    /// The time of the step that last wrote these records. A scaler that starts from them
    /// was down from then until its first step, and the hosts could not say hello meanwhile,
    /// so that span says nothing about them: the first step moves every silent and missing
    /// time forward by it, and only time the scaler observed counts toward the holds. It is
    /// written with every save and by no step on its own, so it is a lower bound on the
    /// last step the old process ran, which errs toward counting less. `None` in a file
    /// that has none: the whole span back to each time is taken as downtime.
    pub last_step: Option<Millis>,
}

#[derive(Serialize)]
struct WireOut<'a> {
    version: u32,
    instances: &'a [Record],
    cooldowns: &'a BTreeMap<String, Cooldown>,
    silent_since: &'a BTreeMap<String, Millis>,
    missing_since: &'a BTreeMap<String, Millis>,
    last_step: Option<Millis>,
}

/// Version 2.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireIn {
    #[serde(rename = "version")]
    _version: u32,
    instances: Vec<Record>,
    cooldowns: BTreeMap<String, Cooldown>,
    silent_since: BTreeMap<String, Millis>,
    missing_since: BTreeMap<String, Millis>,
    #[serde(default)]
    last_step: Option<Millis>,
}

/// Version 1: the same without the two times.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireV1 {
    #[serde(rename = "version")]
    _version: u32,
    instances: Vec<Record>,
    cooldowns: BTreeMap<String, Cooldown>,
}

/// The records file could not be read or written.
#[derive(Debug, thiserror::Error)]
pub enum RecordsError {
    /// The text is not records of this version's shape.
    #[error("records: {0}")]
    Format(#[from] serde_json::Error),
    /// The file is of a version this reader does not read (or names none), so it is not
    /// read.
    #[error(
        "records: version {found:?}, this reader reads {} to {}",
        OLDEST,
        VERSION
    )]
    Version {
        /// The version the file names.
        found: Option<u64>,
    },
}

impl Records {
    /// The file's text: JSON, versioned, with fields in a fixed order.
    ///
    /// # Errors
    ///
    /// [`RecordsError::Format`] if serialisation fails, which these types do not do.
    pub fn encode(&self) -> Result<String, RecordsError> {
        let wire = WireOut {
            version: VERSION,
            instances: &self.instances,
            cooldowns: &self.cooldowns,
            silent_since: &self.silent_since,
            missing_since: &self.missing_since,
            last_step: self.last_step,
        };
        Ok(serde_json::to_string_pretty(&wire)?)
    }

    /// The records in `text`. A file of version 1 is read with no silent or missing times.
    ///
    /// # Errors
    ///
    /// [`RecordsError::Version`] for a file of another version, which is not guessed at,
    /// and [`RecordsError::Format`] for text that is not records.
    pub fn decode(text: &str) -> Result<Self, RecordsError> {
        let value: serde_json::Value = serde_json::from_str(text)?;
        let found = value.get("version").and_then(serde_json::Value::as_u64);
        // The versions from `OLDEST` to `VERSION` are read; any other is refused.
        let version = found
            .and_then(|found| u32::try_from(found).ok())
            .filter(|version| (OLDEST..=VERSION).contains(version))
            .ok_or(RecordsError::Version { found })?;
        if version < TIMES_FROM {
            let wire: WireV1 = serde_json::from_value(value)?;
            Ok(Self {
                instances: wire.instances,
                cooldowns: wire.cooldowns,
                ..Self::default()
            })
        } else {
            let wire: WireIn = serde_json::from_value(value)?;
            Ok(Self {
                instances: wire.instances,
                cooldowns: wire.cooldowns,
                silent_since: wire.silent_since,
                missing_since: wire.missing_since,
                last_step: wire.last_step,
            })
        }
    }
}
