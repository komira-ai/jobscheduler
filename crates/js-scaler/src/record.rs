//! The scaler's records: one per machine it has launched or found, and the cooldowns of
//! its pools. They are the scaler's memory across a restart and the only thing about a
//! machine that outlives the process, so they are written before the call they describe.

use std::collections::BTreeMap;

use js_elastic::{AcquireId, Cooldowns, Instant, Offer, OfferKey, PriceUnit};
use js_types::{Accelerator, ExecMode, Model, ProviderId, accelerator_of, normalise_arch};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::time::Millis;
use crate::verify::HelloDimension;

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
    /// The highest ISA level the offer stated, by its name (`x86-64-v3`), kept while the
    /// scaler runs so that a machine on its way covers the request it was launched for.
    ///
    /// It is read from the file when present and never written: the reader of the release
    /// before this one refuses an unknown key, so a file this release writes stays one that
    /// release reads (a rollback restarts from it). A record read back after a restart
    /// holds no level; the scaler then takes the level the catalog states for the same
    /// offer and zone when it judges whether the machine covers a request. A file without
    /// the key reads as stating none.
    #[serde(default, skip_serializing)]
    pub isa_level: Option<String>,
    /// The accelerator the offer stated, when [`StoredOffer::gpus`] and
    /// [`StoredOffer::gpu_class`] cannot rebuild all of it: a model the alias table does
    /// not know keeps the memory per device, the vendor, the interconnect and the form
    /// factor the offer measured or stated, none of which the class spelling holds. It is
    /// `None` when the spelling rebuilds the whole accelerator, so a record of an alias
    /// offer equals the same record read back.
    ///
    /// It is never written or read, so the records file is unchanged and the release before
    /// this one reads what this release writes. A record read back after a restart holds
    /// none; the scaler then takes the accelerator the catalog states for the same offer
    /// and zone, when it spells the same class and count, as it judges whether the machine
    /// covers a request. Equality includes it.
    #[serde(skip)]
    pub accelerator_facts: Option<Accelerator>,
    /// The execution mode.
    pub exec: StoredExec,
}

/// How an agent spells `accelerator`: its variant, else its model.
fn class_of(accelerator: &Accelerator) -> &str {
    accelerator
        .variant
        .as_ref()
        .unwrap_or(&accelerator.model)
        .as_str()
}

impl StoredOffer {
    /// The shape of `offer`.
    #[must_use]
    pub fn from_offer(offer: &Offer) -> Self {
        let mut stored = Self {
            id: offer.id.clone(),
            zone: offer.zone.clone(),
            cpu_millis: offer.cpu_millis,
            memory_bytes: offer.memory_bytes,
            disk_bytes: offer.disk_bytes,
            gpus: offer.accelerator.as_ref().map_or(0, |a| a.count),
            gpu_class: offer.accelerator.as_ref().map(|a| class_of(a).to_owned()),
            os: offer.os.clone(),
            arch: offer.arch.clone(),
            isa_level: offer.isa_level.map(|level| level.name().to_owned()),
            accelerator_facts: None,
            exec: match offer.exec {
                ExecMode::Container => StoredExec::Container,
                ExecMode::SandboxExec => StoredExec::SandboxExec,
            },
        };
        let rebuilt = stored.accelerator();
        stored.accelerator_facts = offer
            .accelerator
            .clone()
            .filter(|stated| rebuilt.as_ref() != Some(stated));
        stored
    }

    /// Whether this shape is an offer that was recorded, and not [`StoredOffer::nothing`]
    /// (the stand-in for a machine found with no offer to read it from). A hello is judged
    /// against a recorded offer only: a shape that offers nothing would read every hello as
    /// "more than the offer" and say nothing about the machine.
    #[must_use]
    pub fn is_recorded(&self) -> bool {
        *self != Self::nothing(&self.id, self.zone.clone())
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
            isa_level: None,
            accelerator_facts: None,
            exec: StoredExec::Container,
        }
    }

    /// The accelerator the stored count and class spell, with the memory the alias table
    /// states for the variant. A class that is not a model name (a file edited by hand)
    /// states none.
    pub(crate) fn accelerator(&self) -> Option<Accelerator> {
        let model = Model::parse(self.gpu_class.as_deref()?).ok()?;
        Some(accelerator_of(&model, self.gpus, None))
    }

    /// The accelerator of the offer this shape was taken from: the one held, else the one
    /// `listed` (what the catalog states now for the same offer and zone) when it spells
    /// the stored class and count, else the one the stored count and class rebuild.
    pub(crate) fn accelerator_with(&self, listed: Option<&Accelerator>) -> Option<Accelerator> {
        let same = listed.filter(|each| {
            (each.count, Some(class_of(each))) == (self.gpus, self.gpu_class.as_deref())
        });
        self.accelerator_facts
            .clone()
            .or_else(|| same.cloned())
            .or_else(|| self.accelerator())
    }

    /// The offer this shape was taken from, priced at `price`, available. The architecture
    /// is read in the one spelling, so a record written when offers spelt Arm `aarch64`
    /// rebuilds an `arm64` offer. The ISA level is the one held; a held name that is not a
    /// level (a file edited by hand) states none, as an offer that has not said. The
    /// accelerator is the one held when there is one, else the one the count and class
    /// spell.
    #[must_use]
    pub fn to_offer(&self, price: u64) -> Offer {
        Offer {
            id: self.id.clone(),
            zone: self.zone.clone(),
            cpu_millis: self.cpu_millis,
            memory_bytes: self.memory_bytes,
            disk_bytes: self.disk_bytes,
            accelerator: self.accelerator_with(None),
            os: self.os.clone(),
            arch: normalise_arch(&self.arch).to_owned(),
            isa_level: self.isa_level.as_deref().and_then(|name| name.parse().ok()),
            exec: match self.exec {
                StoredExec::Container => ExecMode::Container,
                StoredExec::SandboxExec => ExecMode::SandboxExec,
            },
            price_cents_per_hour: Some(price),
            price_unit: PriceUnit::UsCents,
            available: true,
            purchase: None,
            quota: None,
            block: None,
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
    /// Whether its hello did not match its offer (design 5.6.1, step 6): the machine is
    /// released at the step that judged the hello, and the host is never taken back for a
    /// request. A record an earlier release condemned while leaving it registered is drained
    /// and released as soon as its host is quiet.
    pub condemned: bool,
    /// For `Failed` and `Orphan`: how many clean lists have shown no instance of it. A list
    /// that shows an orphan starts the count over.
    pub clean_lists: u8,
    /// For `Failed` and `Orphan`: when the last of them was taken.
    pub last_clean: Option<Millis>,
    /// Whether the registry has ever shown its host live and heard from, on a step of the
    /// scaler. It is set once and never cleared. A host that has been heard from has
    /// booted, so the lost-Welcome rule (the host said hello and was never heard from; see
    /// `Pass::on_silent`) never applies to it, even when a later restart of its agent
    /// says hello again and the heartbeat count starts over. New records start `false`;
    /// files written before the field existed read it as `true`, the safe direction: the
    /// rule stays off for a host that may have been heard from, and the `unknown_hold`
    /// rules still release a lost Welcome.
    #[serde(default = "seen_live_default")]
    pub seen_live: bool,
    /// The end of the machine's reserved block of time, as the provider's last answer or
    /// listing stated it (`Held::ends_unix_seconds`); `None` for a machine in no block.
    /// Before it the machine is not released as idle or retired at `max_age`; from it, its
    /// going is expected and is no loss. It is never written, so a release before this
    /// field, which refuses an unknown key, still reads a file this release wrote (a
    /// rollback restarts from it); the first list after a restart states it again.
    #[serde(default, skip_serializing)]
    pub ends_at: Option<Millis>,
}

/// The value of [`Record::seen_live`] for a file that has none: `true`.
fn seen_live_default() -> bool {
    true
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
    /// The provider's quota was reached. Written by earlier releases, which cooled a pool on
    /// a `quota` refusal; this release passes the instance type over instead and writes it
    /// no more, and still reads it.
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

/// A pool's go: the instant of its first launch since it was last in `Plan`, and what its
/// machines had cost by the time they left the records. Kept for as long as the pool stays
/// live. The cost since
/// the go is `settled` plus what the machines still in the records cost from `at` on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Go {
    /// When the go began: the step that made the pool's first launch since the pool was last
    /// in `Plan`. It is written with the record of that launch.
    pub at: Millis,
    /// The cost since `at`, in cents, of the machines whose records were forgotten when
    /// their billed time left the month window.
    pub settled: u64,
}

/// A pool's failure brake: how many boots have failed in a row, and whether the pool has been
/// tripped to `Plan`. The count grows the cooldown; the trip is a latch that only an operator's
/// reset ends. Kept only while it holds something.
///
/// Read tolerantly, unlike the rest of the file: a field a newer release adds is ignored and
/// one it stops writing reads as empty, so this release starts on a newer records file.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Brake {
    /// The failures since the last success or reset: a refusal of the kinds `Denied` and
    /// `Shape`, a boot that failed, and a spot preemption. A `Quota` refusal counted in
    /// earlier releases and counts no more: pass 2 passes the instance type over instead.
    pub failures: u32,
    /// When the count reached the pool's limit and the pool was tripped to `Plan`, if it was.
    /// The pool launches nothing while this is set, whatever its `launch` and `live_until`
    /// say, and a success does not clear it; only a reset does.
    #[serde(default)]
    pub tripped_at: Option<Millis>,
}

/// An offer cooling down because the hello of a machine acquired for it did not match it
/// (design 5.6.1, step 6), keyed by (provider, instance type, zone). Written with the
/// release of that machine, in the same records, so a restart keeps it; the step drops it
/// once `until` has come. [`Records::offer_cooldowns`] gives the held ones as the catalog's
/// cool-down store ([`Cooldowns`]) reads them.
///
/// Read tolerantly, as [`Brake`] is: a field a newer release adds to an entry is ignored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OfferCooldown {
    /// The provider of the offer ([`Settings::provider`](crate::Settings::provider)).
    pub provider: String,
    /// The provider's instance type: the offer's id.
    pub offer: String,
    /// The zone of the offer.
    pub zone: Option<String>,
    /// The first instant the offer is no longer cooling down.
    pub until: Millis,
    /// The pool whose machine's hello failed.
    pub pool: String,
    /// The name of the host whose hello failed.
    pub host: String,
    /// Every dimension the hello failed.
    pub failed: Vec<HelloDimension>,
}

impl OfferCooldown {
    /// The key of the offer as the catalog's cool-down store knows it; `None` when the
    /// provider is not a provider id (a records file edited by hand), which no catalog has.
    #[must_use]
    pub fn key(&self) -> Option<OfferKey> {
        let provider = ProviderId::new(&self.provider).ok()?;
        Some(OfferKey {
            provider,
            offer: self.offer.clone(),
            zone: self.zone.clone(),
        })
    }

    /// Whether this cools the same offer as `other`: the same provider, instance type and
    /// zone.
    fn same_offer(&self, other: &Self) -> bool {
        (&self.provider, &self.offer, &self.zone) == (&other.provider, &other.offer, &other.zone)
    }
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
    /// from has been silent, as the scaler first saw it. Written so that a scaler whose
    /// restarts are further apart than the save interval still reaches the hold; a faster
    /// crash loop keeps the host held and counted. The time the scaler
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
    /// last step the old process ran, which errs toward counting less. While a silent or
    /// missing time is pending the records are saved at least every sixth of `unknown_hold`
    /// (at most five minutes) of observed time, so the bound is that close; with none
    /// pending nothing is written for it. `None` in a file
    /// that has none: the whole span back to each time is taken as downtime.
    pub last_step: Option<Millis>,
    /// The go of each pool that has launched since it was last in `Plan`, by name ([`Go`]).
    /// The step that makes the first such launch writes it, and the first step that sees the
    /// pool in `Plan` mode (or past `live_until`) removes it, which is how a new go begins a
    /// new budget.
    /// Absent in files written before it existed, which read it empty.
    pub go: BTreeMap<String, Go>,
    /// The provider kind each pool launched with, by pool name, noted at its launches. A kind
    /// total still counts the records of a pool that has been taken out of the file because
    /// the records remember its kind; a pool with no entry (records from before it existed)
    /// counts in every kind's total. Absent in files written before it existed.
    pub pool_kinds: BTreeMap<String, String>,
    /// The failure brake of each pool that has one ([`Brake`]), by name. A pool with no
    /// failures and no trip has no entry. A pool the server no longer has keeps its entry, so
    /// taking a tripped pool out of the file and putting it back does not clear the trip.
    /// Absent in files written before it existed, which read it empty.
    pub brake: BTreeMap<String, Brake>,
    /// The offers cooling down after a hello that did not match them ([`OfferCooldown`]), one
    /// per (provider, instance type, zone), in the order they were first cooled. Written only
    /// while one is held, so a file with none is the file the release before this one wrote
    /// and reads; absent in files written before it existed, which read it empty.
    pub cooled_offers: Vec<OfferCooldown>,
}

#[derive(Serialize)]
struct WireOut<'a> {
    version: u32,
    instances: &'a [Record],
    cooldowns: &'a BTreeMap<String, Cooldown>,
    silent_since: &'a BTreeMap<String, Millis>,
    missing_since: &'a BTreeMap<String, Millis>,
    last_step: Option<Millis>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    go: &'a BTreeMap<String, Go>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pool_kinds: &'a BTreeMap<String, String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    brake: &'a BTreeMap<String, Brake>,
    #[serde(skip_serializing_if = "<[OfferCooldown]>::is_empty")]
    cooled_offers: &'a [OfferCooldown],
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
    #[serde(default)]
    go: BTreeMap<String, Go>,
    #[serde(default)]
    pool_kinds: BTreeMap<String, String>,
    #[serde(default)]
    brake: BTreeMap<String, Brake>,
    #[serde(default)]
    cooled_offers: Vec<OfferCooldown>,
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
            go: &self.go,
            pool_kinds: &self.pool_kinds,
            brake: &self.brake,
            cooled_offers: &self.cooled_offers,
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
                go: wire.go,
                pool_kinds: wire.pool_kinds,
                brake: wire.brake,
                cooled_offers: wire.cooled_offers,
            })
        }
    }

    /// Cools the offer of `entry` down: a new entry is added, and one already held for the
    /// same provider, instance type and zone takes `entry`'s host and dimensions and keeps the
    /// later of the two ends, so a cool-down is never shortened.
    pub(crate) fn cool_offer(&mut self, entry: OfferCooldown) {
        match self
            .cooled_offers
            .iter_mut()
            .find(|held| held.same_offer(&entry))
        {
            Some(held) => {
                let until = held.until.max(entry.until);
                *held = OfferCooldown { until, ..entry };
            }
            None => self.cooled_offers.push(entry),
        }
    }

    /// Drops the offer cool-downs that have ended at `now`: an end at `now` has ended.
    pub(crate) fn forget_cooled_offers(&mut self, now: Millis) {
        self.cooled_offers.retain(|held| now < held.until);
    }

    /// The offer cool-downs these records hold, as the catalog's cool-down store holds them:
    /// each until its end, on the clock the scaler's [`Millis`] read (the server's). An entry
    /// whose provider is not a provider id is left out.
    #[must_use]
    pub fn offer_cooldowns(&self) -> Cooldowns {
        let mut cooldowns = Cooldowns::new();
        for held in &self.cooled_offers {
            if let Some(key) = held.key() {
                cooldowns.hold_until(key, Instant::from_millis(held.until.as_millis()));
            }
        }
        cooldowns
    }
}
