//! The launch side's records: the ids of instances and images, the state an instance is
//! in, the tags that mark what this scheduler launched, and the request that launches an
//! instance. They are the crate's own words; the cloud's XML is read into them once, by
//! the client of the port.

use std::fmt;

use js_elastic::{AcquireId, HostRef, HostStatus, SchedulerId};
use sha2::{Digest, Sha256};

use crate::model::{InstanceType, Zone};

/// The tag that holds the id of the scheduler that launched the instance.
pub const SCHEDULER_TAG: &str = "js-scheduler";

/// The tag that holds the id of the acquire the instance was launched for.
pub const ACQUIRE_TAG: &str = "js-acquire-id";

/// The tag that holds the host name; the console shows it as the instance's name.
pub const NAME_TAG: &str = "Name";

/// The prefix of the key of a tag made from a label.
pub const LABEL_PREFIX: &str = "js-label:";

/// The text `text` if it is `prefix` and lower-case hexadecimal digits whose number
/// `fits`.
fn hex_after(text: &str, prefix: &str, fits: impl Fn(usize) -> bool) -> Option<String> {
    let rest = text.strip_prefix(prefix)?;
    let hex = rest.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    (hex & fits(rest.len())).then_some(text.to_owned())
}

/// The id of an instance: `i-` and 8 or 17 lower-case hexadecimal digits.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct InstanceId(String);

impl InstanceId {
    /// The id `text`, or `None` when it is not an instance id.
    pub fn new(text: &str) -> Option<Self> {
        hex_after(text, "i-", |len| matches!(len, 8 | 17)).map(Self)
    }

    /// The id of the `n`th instance the fake cloud launches: `i-` and 17 hexadecimal digits.
    pub(crate) fn numbered(n: u64) -> Self {
        Self(format!("i-{n:017x}"))
    }

    /// The id as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The scheduler's reference to the instance: its id.
    pub fn host_ref(&self) -> HostRef {
        HostRef(self.0.clone())
    }
}

impl fmt::Display for InstanceId {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str(&self.0)
    }
}

/// The id of a machine image: `ami-` and 8 to 17 lower-case hexadecimal digits.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ImageId(String);

impl ImageId {
    /// The id `text`, or `None` when it is not an image id.
    pub fn new(text: &str) -> Option<Self> {
        hex_after(text, "ami-", |len| (8..=17).contains(&len)).map(Self)
    }

    /// The id as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ImageId {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str(&self.0)
    }
}

/// The state of an instance, as `instanceState/name` of `DescribeInstances` spells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum InstanceState {
    /// `pending`: starting.
    Pending,
    /// `running`.
    Running,
    /// `shutting-down`: being terminated.
    ShuttingDown,
    /// `terminated`: gone, and shown for a while.
    Terminated,
    /// `stopping`: being stopped.
    Stopping,
    /// `stopped`.
    Stopped,
}

/// Every state, in the order the cloud documents them.
const STATES: [InstanceState; 6] = [
    InstanceState::Pending,
    InstanceState::Running,
    InstanceState::ShuttingDown,
    InstanceState::Terminated,
    InstanceState::Stopping,
    InstanceState::Stopped,
];

impl InstanceState {
    /// The state the cloud calls `text`, or `None` for a name this release does not know.
    pub fn parse(text: &str) -> Option<Self> {
        STATES.into_iter().find(|state| state.name() == text)
    }

    /// The state as the cloud spells it.
    pub fn name(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::ShuttingDown => "shutting-down",
            Self::Terminated => "terminated",
            Self::Stopping => "stopping",
            Self::Stopped => "stopped",
        }
    }

    /// The status the scheduler reads the state as. A stopped instance is read as
    /// terminated: this provider never stops one, and one stopped by someone else is not
    /// running work.
    pub fn status(self) -> HostStatus {
        match self {
            Self::Pending => HostStatus::Booting,
            Self::Running => HostStatus::Active,
            Self::ShuttingDown | Self::Stopping => HostStatus::Terminating,
            Self::Terminated | Self::Stopped => HostStatus::Terminated,
        }
    }
}

/// One tag of an instance.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Tag {
    /// The key.
    pub key: String,
    /// The value.
    pub value: String,
}

/// One instance of `DescribeInstances`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instance {
    /// `instanceId`.
    pub id: InstanceId,
    /// `instanceState/name`.
    pub state: InstanceState,
    /// `tagSet`.
    pub tags: Vec<Tag>,
    /// `instanceType`.
    pub instance_type: InstanceType,
    /// `placement/availabilityZone`.
    pub zone: Zone,
    /// `imageId`.
    pub image: ImageId,
}

impl Instance {
    /// The value of the tag `key`, if the instance has one.
    pub fn tag(&self, key: &str) -> Option<&str> {
        self.tags
            .iter()
            .find(|tag| tag.key == key)
            .map(|tag| tag.value.as_str())
    }
}

/// How an instance is bought, among the ways this provider launches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchPurchase {
    /// On demand.
    OnDemand,
    /// Spot, at the market price (no maximum price is set).
    Spot,
}

/// The token that makes a repeated `RunInstances` the same launch: the cloud answers the
/// instances of the first call and starts none, for as long as it remembers the token
/// (it documents hours, not days). It is derived from the scheduler and the acquire, so
/// every retry of one acquire carries the same one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientToken(String);

impl ClientToken {
    /// The token of the launch for `acquire` by `scheduler`: the lower-case hexadecimal
    /// SHA-256 of the scheduler id, a newline and the acquire id (64 characters).
    pub fn derive(acquire: &AcquireId, scheduler: &SchedulerId) -> Self {
        let digest = Sha256::digest(format!("{}\n{acquire}", scheduler.as_str()).as_bytes());
        Self(digest.iter().map(|byte| format!("{byte:02x}")).collect())
    }

    /// The token as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The user data of a launch: the bytes the machine reads at boot. They can hold a join
/// token, so the `Debug` text says how many there are and never what they are.
#[derive(Clone, PartialEq, Eq)]
pub struct UserData(Vec<u8>);

impl UserData {
    /// The user data `bytes`.
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    /// The bytes. Call it only to send them, or to check what was sent.
    pub fn expose(&self) -> &[u8] {
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

impl fmt::Debug for UserData {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(out, "UserData({} bytes)", self.0.len())
    }
}

/// One launch: `RunInstances` for a single instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRequest {
    /// The machine image.
    pub image: ImageId,
    /// The instance type.
    pub instance_type: InstanceType,
    /// The availability zone.
    pub zone: Zone,
    /// How the instance is bought.
    pub purchase: LaunchPurchase,
    /// What the machine reads at boot.
    pub user_data: UserData,
    /// The tags the instance is created with.
    pub tags: Vec<Tag>,
    /// The token that makes a repeat of the call the same launch.
    pub client_token: ClientToken,
}

/// Which instances `DescribeInstances` is asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DescribeFilter {
    /// The instances that carry every one of these tags (`tag:<key>=<value>` filters).
    Tags(Vec<Tag>),
    /// These instances, by id. The cloud answers an error when it knows none of them.
    Ids(Vec<InstanceId>),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An instance with the given tags.
    fn instance(tags: &[(&str, &str)]) -> Instance {
        Instance {
            id: InstanceId::new("i-0123456789abcdef0").unwrap(),
            state: InstanceState::Pending,
            tags: tags
                .iter()
                .map(|(key, value)| Tag {
                    key: (*key).to_owned(),
                    value: (*value).to_owned(),
                })
                .collect(),
            instance_type: InstanceType("m7.large".to_owned()),
            zone: Zone("zone-a1".to_owned()),
            image: ImageId::new("ami-0123456789abcdef0").unwrap(),
        }
    }

    /// Asserts that an instance id is `i-` and 8 or 17 lower-case hexadecimal digits, that
    /// every other text is refused (a short, an in-between, a long, an upper-case, a
    /// non-hex, an empty and a wrongly prefixed one), and that the id reads back as
    /// given, as text, as a host reference and as a display.
    #[test]
    fn an_instance_id_is_i_and_8_or_17_hex_digits() {
        for good in ["i-01234567", "i-0123456789abcdef0"] {
            let id = InstanceId::new(good).unwrap();
            assert_eq!(id.as_str(), good);
            assert_eq!(id.to_string(), good);
            assert_eq!(id.host_ref(), HostRef(good.to_owned()));
        }
        assert_eq!(
            InstanceId::numbered(255),
            InstanceId::new("i-000000000000000ff").unwrap()
        );
        for bad in [
            "",
            "i-",
            "i-0123456",
            "i-012345678",
            "i-0123456789abcdef01",
            "i-0123456G",
            "i-0123456A",
            "x-01234567",
            "ami-01234567",
        ] {
            assert_eq!(InstanceId::new(bad), None, "{bad:?}");
        }
    }

    /// Asserts that an image id is `ami-` and 8 to 17 lower-case hexadecimal digits at the
    /// edges of that range and nowhere outside it, and that other text is refused.
    #[test]
    fn an_image_id_is_ami_and_8_to_17_hex_digits() {
        for good in ["ami-01234567", "ami-0123456789abcdef0"] {
            let id = ImageId::new(good).unwrap();
            assert_eq!(id.as_str(), good);
            assert_eq!(id.to_string(), good);
        }
        for bad in [
            "",
            "ami-",
            "ami-0123456",
            "ami-0123456789abcdef01",
            "ami-0123456Z",
            "ami-0123456A",
            "i-01234567",
        ] {
            assert_eq!(ImageId::new(bad), None, "{bad:?}");
        }
    }

    /// Asserts that every instance state parses from the name the cloud gives it and
    /// names itself the same, that an unknown name does not parse, and that the status
    /// each state is read as is: pending is booting, running is active, shutting-down and
    /// stopping are terminating, terminated and stopped are terminated.
    #[test]
    fn instance_states_parse_name_and_map_to_statuses() {
        let table = [
            ("pending", InstanceState::Pending, HostStatus::Booting),
            ("running", InstanceState::Running, HostStatus::Active),
            (
                "shutting-down",
                InstanceState::ShuttingDown,
                HostStatus::Terminating,
            ),
            (
                "terminated",
                InstanceState::Terminated,
                HostStatus::Terminated,
            ),
            ("stopping", InstanceState::Stopping, HostStatus::Terminating),
            ("stopped", InstanceState::Stopped, HostStatus::Terminated),
        ];
        for (name, state, status) in table {
            assert_eq!(InstanceState::parse(name), Some(state));
            assert_eq!(state.name(), name);
            assert_eq!(state.status(), status);
        }
        assert_eq!(InstanceState::parse("Running"), None);
        assert_eq!(InstanceState::parse(""), None);
    }

    /// Asserts that a tag is found by its key, that the first of two with one key wins,
    /// and that a key the instance does not carry has no value.
    #[test]
    fn an_instance_answers_the_value_of_a_tag() {
        let one = instance(&[("a", "1"), ("b", "2"), ("a", "3")]);
        assert_eq!(one.tag("a"), Some("1"));
        assert_eq!(one.tag("b"), Some("2"));
        assert_eq!(one.tag("c"), None);
    }

    /// Asserts that a client token is the 64 lower-case hexadecimal digits of the SHA-256
    /// of the scheduler id, a newline and the acquire id (pinned against a known digest),
    /// that the same acquire and scheduler always give the same token, and that another
    /// acquire or another scheduler gives another. It catches a token that ignores the
    /// scheduler, which would join two schedulers' launches of one acquire id.
    #[test]
    fn a_client_token_is_a_hash_of_the_scheduler_and_the_acquire() {
        let one = SchedulerId::new("one").unwrap();
        let two = SchedulerId::new("two").unwrap();
        let a = AcquireId::new("a").unwrap();
        let b = AcquireId::new("b").unwrap();
        let token = ClientToken::derive(&a, &one);
        assert_eq!(token.as_str().len(), 64);
        assert!(
            token
                .as_str()
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        );
        assert_eq!(token, ClientToken::derive(&a, &one));
        assert_ne!(token, ClientToken::derive(&b, &one));
        assert_ne!(token, ClientToken::derive(&a, &two));
        let expected = Sha256::digest(b"one\na");
        let hex: String = expected.iter().map(|byte| format!("{byte:02x}")).collect();
        assert_eq!(token.as_str(), hex);
    }

    /// Asserts that user data hands back the bytes it was given and their count, and that
    /// its `Debug` text, and that of a request that holds it, says how many bytes there
    /// are and never what they are.
    #[test]
    fn user_data_debug_names_the_size_and_never_the_bytes() {
        let data = UserData::new(b"join-token=hunter2".to_vec());
        assert_eq!(data.expose(), b"join-token=hunter2");
        assert_eq!(data.len(), 18);
        assert!(!data.is_empty());
        assert!(UserData::new(Vec::new()).is_empty());
        assert_eq!(format!("{data:?}"), "UserData(18 bytes)");
        let request = RunRequest {
            image: ImageId::new("ami-01234567").unwrap(),
            instance_type: InstanceType("m7.large".to_owned()),
            zone: Zone("zone-a1".to_owned()),
            purchase: LaunchPurchase::Spot,
            user_data: data,
            tags: Vec::new(),
            client_token: ClientToken::derive(
                &AcquireId::new("a").unwrap(),
                &SchedulerId::new("s").unwrap(),
            ),
        };
        let printed = format!("{request:?}");
        assert!(printed.contains("UserData(18 bytes)"));
        assert!(!printed.contains("hunter2"));
        assert_eq!(request.clone(), request);
        assert_eq!(
            DescribeFilter::Ids(Vec::new()),
            DescribeFilter::Ids(Vec::new())
        );
        assert_ne!(
            DescribeFilter::Tags(Vec::new()),
            DescribeFilter::Ids(Vec::new())
        );
        assert_ne!(LaunchPurchase::Spot, LaunchPurchase::OnDemand);
    }
}
