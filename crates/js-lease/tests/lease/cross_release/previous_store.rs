/// The whole file.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Document {
    pub version: u32,
    pub term: u64,
    pub next_sequence: u64,
    /// How many `[[lease]]` tables follow. A file cut short (a partial restore) has
    /// fewer and is refused instead of loading as a smaller table.
    pub lease_count: u64,
    /// How many `[[reservation]]` tables follow; absent (written only when above zero)
    /// is none. Checked like `lease_count`: a file cut short is refused.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub reservation_count: u64,
    #[serde(default)]
    pub lease: Vec<StoredLease>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reservation: Vec<StoredReservation>,
    /// Fields of a later version, kept as read.
    #[serde(flatten)]
    pub extra: toml::Table,
}

fn is_zero(count: &u64) -> bool {
    *count == 0
}

/// One reservation record as written: the held part (`host`, `reserved_at_ms`,
/// `deadline_ms`, all three or none), the counters and the exclusions.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct StoredReservation {
    pub request_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reserved_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_ms: Option<u64>,
    /// Whether the held host was quiet when it was set aside. Absent is not; a build that
    /// predates the field reads the row without it and keeps it on rewrite.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub assigned: bool,
    /// The end of the held host's tail when it was set aside; only on a row that holds a
    /// host. Absent is unknown; a build that predates the field reads the row without it
    /// and keeps it on rewrite.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tail_end_ms: Option<u64>,
    /// The place on the wait list and when the request was first presented, both or
    /// neither.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_place: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queued_since_ms: Option<u64>,
    #[serde(default)]
    pub give_ups: u32,
    #[serde(default)]
    pub prepare_failures: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclusion: Vec<StoredExclusion>,
    /// Fields of a later version, kept as read.
    #[serde(flatten)]
    pub extra: toml::Table,
}

/// One excluded host: until `until_ms`, or for good when `forever` is set.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct StoredExclusion {
    pub host: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub forever: bool,
}

/// One lease as written.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct StoredLease {
    pub term: u64,
    pub sequence: u64,
    pub state: LeaseState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    pub acquired_at_ms: u64,
    pub expires_at_ms: u64,
    pub ends_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validation_run: Option<String>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    /// Whether the lease is of a whole-node class, whose ttl and maximum wall time
    /// count from the moment it is observed ready. Absent in a file written before the
    /// field existed, which loads as not whole.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub whole: bool,
    /// The ttl granted at acquire, kept while a whole lease is pending.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_ms: Option<u64>,
    /// The maximum wall time of the request, kept while a whole lease is pending.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wall_ms: Option<u64>,
    /// Fields of a later version, kept as read.
    #[serde(flatten)]
    pub extra: toml::Table,
}
