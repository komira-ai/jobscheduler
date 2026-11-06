//! The launch side's records: the names and ids of instances, the image a boot disk is
//! made from, the status an instance is in, the labels that mark what this scheduler
//! launched, the request that inserts an instance, and the zone operation the cloud answers
//! a change with. They are the crate's own words; the cloud's JSON is read into them once,
//! by the client of the port.

use std::fmt;
use std::time::Duration;

use js_elastic::{AcquireId, HostRef, HostStatus, SchedulerId};
use sha2::{Digest, Sha256};

use crate::error::ErrorReason;
use crate::model::{AttachedAccelerator, MachineTypeName, Project, Zone};

/// The label that holds the id of the scheduler that launched the instance, in the label
/// grammar ([`label_value`]).
pub const SCHEDULER_LABEL: &str = "js-scheduler";

/// The label that holds the id of the acquire the instance was launched for, in the label
/// grammar ([`label_value`]).
pub const ACQUIRE_LABEL: &str = "js-acquire-id";

/// The prefix of the key of a label made from one of the acquire's labels.
pub const LABEL_PREFIX: &str = "js-label-";

/// The key of the metadata item that holds the boot payload: `user-data`, the key
/// cloud-init's Compute Engine data source reads its user data from.
pub const PAYLOAD_KEY: &str = "user-data";

/// The longest name, label key and label value the cloud takes.
pub(crate) const MAX_NAME: usize = 63;

/// The longest instance id: the cloud's ids are unsigned 64-bit numbers, at most 20 digits.
const MAX_ID_DIGITS: usize = 20;

/// The characters of a label value written as it is; a value written as a hash starts with
/// [`HASHED`], which is not one of them.
fn plain_value_byte(byte: u8) -> bool {
    byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'
}

/// The first character of a label value that is a hash of the id, not the id.
const HASHED: char = '_';

/// The value a label carries for the id `id` (a scheduler id or an acquire id): the id as
/// it is when it is at most 63 lower-case letters, digits and `-`; otherwise `_` and the
/// first 62 lower-case hexadecimal digits of the SHA-256 of the id. Either way it is a value
/// of the label grammar (at most 63 lower-case letters, digits, `_` and `-`), and two ids
/// never share a value: a value written as it is holds no `_`, and a hashed one starts with
/// it.
pub fn label_value(id: &str) -> String {
    let plain = (id.len() <= MAX_NAME) & id.bytes().all(plain_value_byte);
    match plain {
        true => id.to_owned(),
        false => {
            let digest = Sha256::digest(id.as_bytes());
            let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
            format!("{HASHED}{}", &hex[..MAX_NAME - 1])
        }
    }
}

/// The id a label value of [`label_value`] was made from, when the value holds it as it
/// is; `None` for a hashed value, which no longer holds it.
pub(crate) fn id_of_label(value: &str) -> Option<&str> {
    (!value.starts_with(HASHED)).then_some(value)
}

/// Whether `text` is a value of the label grammar: at most 63 lower-case letters, digits,
/// `_` and `-`, possibly none.
pub fn is_label_value(text: &str) -> bool {
    (text.len() <= MAX_NAME)
        & text
            .bytes()
            .all(|byte| plain_value_byte(byte) || byte == b'_')
}

/// Whether `text` is a key of the label grammar: 1 to 63 lower-case letters, digits, `_`
/// and `-`, starting with a letter.
pub fn is_label_key(text: &str) -> bool {
    text.bytes().next().is_some_and(|b| b.is_ascii_lowercase()) & is_label_value(text)
}

/// Whether `text` is a name of the cloud's resource grammar: 1 to 63 lower-case letters,
/// digits and `-`, starting with a letter and not ending with `-`
/// (`[a-z]([-a-z0-9]{0,61}[a-z0-9])?`).
pub fn is_resource_name(text: &str) -> bool {
    let bytes = text.as_bytes();
    let fits = [
        (1..=MAX_NAME).contains(&text.len()),
        bytes.first().is_some_and(u8::is_ascii_lowercase),
        bytes.last().is_some_and(|b| *b != b'-'),
        bytes.iter().copied().all(plain_value_byte),
    ];
    fits.into_iter().all(|fit| fit)
}

/// The name of an instance, as the cloud's resource grammar has it ([`is_resource_name`]).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct InstanceName(String);

impl InstanceName {
    /// The name `text`, or `None` when it is not a name of the resource grammar.
    pub fn new(text: &str) -> Option<Self> {
        is_resource_name(text).then(|| Self(text.to_owned()))
    }

    /// The name as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for InstanceName {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str(&self.0)
    }
}

/// The cloud's id of an instance: an unsigned 64-bit number written in decimal, with no
/// leading zero.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct InstanceId(String);

impl InstanceId {
    /// The id `text`, or `None` when it is not 1 to 20 decimal digits with no leading zero
    /// that fit 64 bits.
    pub fn new(text: &str) -> Option<Self> {
        let digits = (1..=MAX_ID_DIGITS).contains(&text.len())
            & text.bytes().all(|b| b.is_ascii_digit())
            & !text.starts_with('0');
        (digits & text.parse::<u64>().is_ok()).then(|| Self(text.to_owned()))
    }

    /// The id of the `n`th instance the fake cloud inserts.
    pub(crate) fn numbered(n: u64) -> Self {
        Self(n.max(1).to_string())
    }

    /// The id as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for InstanceId {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str(&self.0)
    }
}

/// Which instance of a zone a call names: by its name, or by its id (the `instance`
/// parameter of `instances.get` and `instances.delete` takes either, as the pattern of the
/// Compute Engine discovery document allows: a name of the resource grammar or 1 to 20
/// digits).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum InstanceKey {
    /// By name.
    Name(InstanceName),
    /// By id.
    Id(InstanceId),
}

impl InstanceKey {
    /// The key as the path of a call writes it.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Name(name) => name.as_str(),
            Self::Id(id) => id.as_str(),
        }
    }
}

/// One instance of one zone of one project, as `instances.get` and `instances.delete`
/// name it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct InstanceRef {
    /// The project.
    pub project: Project,
    /// The zone.
    pub zone: Zone,
    /// The instance.
    pub instance: InstanceKey,
}

/// The scheduler's reference to an instance: `<zone>/<id>`, such as
/// `us-central1-a/1234567890123456789`. The id never names another instance, so a stale
/// reference can never reach a newer instance of the same name.
pub fn host_ref(zone: &Zone, id: &InstanceId) -> HostRef {
    HostRef(format!("{}/{}", zone.as_str(), id.as_str()))
}

/// The zone and id a reference of [`host_ref`] names, or `None` for text that is not one.
pub fn parse_host_ref(reference: &HostRef) -> Option<(Zone, InstanceId)> {
    let (zone, id) = reference.as_str().split_once('/')?;
    Some((Zone::new(zone).ok()?, InstanceId::new(id)?))
}

/// A machine image a boot disk is made from: a global image of a project
/// (`projects/<project>/global/images/<image>`) or the newest image of a family
/// (`projects/<project>/global/images/family/<family>`), as `initializeParams.sourceImage`
/// takes it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ImagePath(String);

impl ImagePath {
    /// The path `text`, or `None` when it is neither form: `projects/`, a project id,
    /// `/global/images/`, then an image name or `family/` and a family name, each of the
    /// resource grammar.
    pub fn new(text: &str) -> Option<Self> {
        let rest = text.strip_prefix("projects/")?;
        let (project, rest) = rest.split_once('/')?;
        Project::new(project).ok()?;
        let rest = rest.strip_prefix("global/images/")?;
        let name = rest.strip_prefix("family/").unwrap_or(rest);
        is_resource_name(name).then(|| Self(text.to_owned()))
    }

    /// The path as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ImagePath {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str(&self.0)
    }
}

/// The status of an instance, as the `status` of an instance resource spells it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum InstanceStatus {
    /// `PROVISIONING`: resources are being allocated.
    Provisioning,
    /// `STAGING`: resources are acquired and the instance is being prepared to boot.
    Staging,
    /// `RUNNING`.
    Running,
    /// `STOPPING`: being stopped or deleted.
    Stopping,
    /// `STOPPED`.
    Stopped,
    /// `SUSPENDING`.
    Suspending,
    /// `SUSPENDED`.
    Suspended,
    /// `TERMINATED`: stopped, or ended by the cloud; it still exists.
    Terminated,
    /// `REPAIRING`: the cloud is repairing it after an internal error.
    Repairing,
    /// A status this release does not know, printable ASCII only and at most 40
    /// characters.
    Other(String),
}

/// The statuses the cloud documents, by name.
const STATUSES: [(&str, InstanceStatus); 9] = [
    ("PROVISIONING", InstanceStatus::Provisioning),
    ("STAGING", InstanceStatus::Staging),
    ("RUNNING", InstanceStatus::Running),
    ("STOPPING", InstanceStatus::Stopping),
    ("STOPPED", InstanceStatus::Stopped),
    ("SUSPENDING", InstanceStatus::Suspending),
    ("SUSPENDED", InstanceStatus::Suspended),
    ("TERMINATED", InstanceStatus::Terminated),
    ("REPAIRING", InstanceStatus::Repairing),
];

/// The longest unknown status kept.
const STATUS_KEPT: usize = 40;

impl InstanceStatus {
    /// The status the cloud calls `text`; a name this release does not know is
    /// [`InstanceStatus::Other`].
    pub fn parse(text: &str) -> Self {
        STATUSES
            .iter()
            .find(|(name, _)| *name == text)
            .map_or_else(
                || {
                    Self::Other(
                        text.chars()
                            .filter(char::is_ascii_graphic)
                            .take(STATUS_KEPT)
                            .collect(),
                    )
                },
                |(_, status)| status.clone(),
            )
    }

    /// The status the scheduler reads this as. `PROVISIONING`, `STAGING` and `REPAIRING`
    /// are booting; `RUNNING` is active; `STOPPING` and `SUSPENDING` are terminating;
    /// `TERMINATED`, `STOPPED` and `SUSPENDED` are terminated: this provider never stops or
    /// suspends an instance, and one stopped or suspended by someone else runs no work. An
    /// unknown status is the scheduler's unknown, a machine that exists and may bill.
    pub fn status(&self) -> HostStatus {
        match self {
            Self::Provisioning | Self::Staging | Self::Repairing => HostStatus::Booting,
            Self::Running => HostStatus::Active,
            Self::Stopping | Self::Suspending => HostStatus::Terminating,
            Self::Terminated | Self::Stopped | Self::Suspended => HostStatus::Terminated,
            Self::Other(text) => HostStatus::Unknown(text.clone()),
        }
    }
}

/// One label of an instance.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Label {
    /// The key.
    pub key: String,
    /// The value.
    pub value: String,
}

/// One instance, as `instances.get` and `instances.aggregatedList` give it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instance {
    /// `id`.
    pub id: InstanceId,
    /// `name`.
    pub name: String,
    /// `status`.
    pub status: InstanceStatus,
    /// `labels`.
    pub labels: Vec<Label>,
    /// `machineType`: the type's name, not its URL.
    pub machine_type: MachineTypeName,
    /// `zone`: the zone's name, not its URL.
    pub zone: Zone,
}

impl Instance {
    /// The value of the label `key`, if the instance has one.
    pub fn label(&self, key: &str) -> Option<&str> {
        self.labels
            .iter()
            .find(|label| label.key == key)
            .map(|label| label.value.as_str())
    }

    /// The scheduler's reference to the instance.
    pub fn host_ref(&self) -> HostRef {
        host_ref(&self.zone, &self.id)
    }
}

/// The provisioning model of an instance (`scheduling.provisioningModel`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProvisioningModel {
    /// `STANDARD`.
    Standard,
    /// `SPOT`: the cloud may preempt it.
    Spot,
}

impl ProvisioningModel {
    /// The model as the API spells it.
    pub fn name(self) -> &'static str {
        match self {
            Self::Standard => "STANDARD",
            Self::Spot => "SPOT",
        }
    }
}

/// What the cloud does on a host maintenance event (`scheduling.onHostMaintenance`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostMaintenance {
    /// `TERMINATE`: the instance is stopped, and it is not moved to another host.
    Terminate,
}

impl HostMaintenance {
    /// The setting as the API spells it.
    pub fn name(self) -> &'static str {
        match self {
            Self::Terminate => "TERMINATE",
        }
    }
}

/// What the cloud does to an instance it ends: at a Spot preemption, or when its run
/// duration is reached (`scheduling.instanceTerminationAction`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminationAction {
    /// `DELETE`: the instance is deleted, so it never lingers as `TERMINATED`.
    Delete,
}

impl TerminationAction {
    /// The action as the API spells it.
    pub fn name(self) -> &'static str {
        match self {
            Self::Delete => "DELETE",
        }
    }
}

/// The `scheduling` of an inserted instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scheduling {
    /// `provisioningModel`.
    pub provisioning_model: ProvisioningModel,
    /// `onHostMaintenance`.
    pub on_host_maintenance: HostMaintenance,
    /// `automaticRestart`.
    pub automatic_restart: bool,
    /// `instanceTerminationAction`, when one is set.
    pub termination_action: Option<TerminationAction>,
    /// `maxRunDuration`, in whole seconds, when one is set: the cloud ends the instance
    /// once it has run this long.
    pub max_run_duration: Option<Duration>,
}

/// The boot payload of an inserted instance, as one metadata item: the bytes the machine
/// reads at boot. They can hold a join token, so the `Debug` text says how many there are
/// and never what they are.
#[derive(Clone, PartialEq, Eq)]
pub struct Payload(String);

impl Payload {
    /// The payload `text`.
    pub fn new(text: String) -> Self {
        Self(text)
    }

    /// The text. Call it only to send it, or to check what was sent.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// How many bytes there are.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for Payload {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(out, "Payload({} bytes)", self.0.len())
    }
}

/// The `requestId` of an insert: a UUID the cloud remembers for a while, so that a
/// repeated insert with it is the same insert. It is derived from the scheduler and the
/// acquire, so every retry of one acquire carries the same one.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RequestId(String);

impl RequestId {
    /// The request id of the insert for `acquire` by `scheduler`: the first 16 bytes of the
    /// SHA-256 of the scheduler id, a newline and the acquire id, with the version nibble
    /// set to 8 and the variant bits to `10` (a version 8 UUID, RFC 9562), written as 32
    /// lower-case hexadecimal digits in the groups 8-4-4-4-12.
    pub fn derive(acquire: &AcquireId, scheduler: &SchedulerId) -> Self {
        let digest = Sha256::digest(format!("{}\n{acquire}", scheduler.as_str()).as_bytes());
        let mut bytes = [0u8; 16];
        bytes.copy_from_slice(&digest[..16]);
        bytes[6] = (bytes[6] & 0x0f) | 0x80;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        Self(format!(
            "{}-{}-{}-{}-{}",
            &hex[0..8],
            &hex[8..12],
            &hex[12..16],
            &hex[16..20],
            &hex[20..32]
        ))
    }

    /// The id as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The network interface of an inserted instance: the network and subnetwork it is
/// attached to, as resource paths, with no external address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkInterface {
    /// `network`.
    pub network: String,
    /// `subnetwork`.
    pub subnetwork: String,
}

/// One insert: `instances.insert` for a single instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InsertRequest {
    /// The project.
    pub project: Project,
    /// The zone.
    pub zone: Zone,
    /// `name`.
    pub name: InstanceName,
    /// `machineType`: the type's name in the zone.
    pub machine_type: MachineTypeName,
    /// `scheduling`.
    pub scheduling: Scheduling,
    /// The boot disk's `initializeParams.sourceImage`.
    pub image: ImagePath,
    /// The boot disk's `initializeParams.diskSizeGb`, when the offer asks for a size; the
    /// image's own size otherwise.
    pub disk_size_gb: Option<u64>,
    /// `guestAccelerators`: the accelerators attached to the instance, when the machine
    /// type takes attached ones and the offer has them.
    pub accelerators: Option<AttachedAccelerator>,
    /// `labels`.
    pub labels: Vec<Label>,
    /// The metadata item [`PAYLOAD_KEY`]: the boot payload.
    pub payload: Payload,
    /// `networkInterfaces`: one.
    pub network: NetworkInterface,
    /// The `requestId` query parameter.
    pub request_id: RequestId,
}

/// Which instances `instances.aggregatedList` is asked for: those that carry every one of
/// these labels (a `filter` of `labels.<key> = "<value>"` terms joined by `AND`), from the
/// page `page_token` names (the first page when `None`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListFilter {
    /// The project.
    pub project: Project,
    /// The labels.
    pub labels: Vec<Label>,
    /// The `pageToken` of the page asked for.
    pub page_token: Option<String>,
}

/// One page of a list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page<T> {
    /// The rows of the page.
    pub items: Vec<T>,
    /// `nextPageToken`, when there is a further page.
    pub next_page_token: Option<String>,
    /// Whether the answer named a scope it could not reach (`unreachables`), so that the
    /// page is not whole.
    pub unreachable: bool,
}

/// One zone operation of one project, as `zoneOperations.get` names it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct OperationRef {
    /// The project.
    pub project: Project,
    /// The zone.
    pub zone: Zone,
    /// The operation's `name`.
    pub name: String,
}

/// The `status` of an operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationStatus {
    /// `PENDING`.
    Pending,
    /// `RUNNING`.
    Running,
    /// `DONE`: finished, with an error or without.
    Done,
}

impl OperationStatus {
    /// The status the cloud calls `text`, or `None` for a name this release does not know.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "PENDING" => Some(Self::Pending),
            "RUNNING" => Some(Self::Running),
            "DONE" => Some(Self::Done),
            _ => None,
        }
    }
}

/// A zone operation: what the cloud answers an insert or a delete with, and what
/// `zoneOperations.get` reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Operation {
    /// Which operation it is.
    pub reference: OperationRef,
    /// `status`.
    pub status: OperationStatus,
    /// `targetId`: the id of the instance the operation is about, when the answer gives
    /// one.
    pub target_id: Option<InstanceId>,
    /// `error.errors[].code`, in order: empty while the operation has not failed.
    pub error_codes: Vec<ErrorReason>,
}

impl Operation {
    /// Whether the operation finished with an error.
    pub fn failed(&self) -> bool {
        (self.status == OperationStatus::Done) & !self.error_codes.is_empty()
    }
}
