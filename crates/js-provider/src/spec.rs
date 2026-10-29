use js_types::{LabelError, Labels, LeaseId, Request, RequestError, RequestId, ResourceClass};

/// A plain, constant code for a refusal. No central code table exists beyond this
/// enum; a provider may add a code and may never remove a start-contract one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum RefusalCode {
    /// Start contract: the maximum wall time was zero.
    WallTimeNotPositive,
    /// Start contract: a caller label was under the reserved prefix.
    ReservedLabel,
    /// Start contract: a label key or value was malformed.
    InvalidLabel,
    /// Start contract: the class is not one the provider serves.
    UnknownClass,
    /// The request is below its class's minimum.
    BelowClassMinimum,
    /// The provider does not serve this class.
    UnsupportedClass,
    /// No host of the provider could ever satisfy the request.
    NoFit,
    /// A host would fit, but every fitting host is booked.
    NoCapacity,
    /// The lease id was stopped, and a stopped lease id never starts again: the start
    /// is a late or duplicate one and books nothing.
    LeaseEnded,
    /// A whole-node request that no registered host could ever be given whole: none is
    /// live, lost within retention or awaiting, untainted and of a fitting shape. Waiting
    /// will not help, unlike [`RefusalCode::NoCapacity`].
    NoCapableHost,
    /// A whole-node request that used up its tries to get a host ready: it will not be
    /// placed again.
    ExclusiveNotReady,
}

impl RefusalCode {
    /// The codes of the start-contract rules this slice models, one per rule that
    /// `StartSpec::new` refuses with a code of its own: wall time, reserved label,
    /// malformed label and unknown class. The conformance suite holds a check for each.
    pub const START_CONTRACT: [RefusalCode; 4] = [
        RefusalCode::WallTimeNotPositive,
        RefusalCode::ReservedLabel,
        RefusalCode::InvalidLabel,
        RefusalCode::UnknownClass,
    ];

    /// The code as a constant string.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::WallTimeNotPositive => "wall-time-not-positive",
            Self::ReservedLabel => "reserved-label",
            Self::InvalidLabel => "invalid-label",
            Self::UnknownClass => "unknown-class",
            Self::BelowClassMinimum => "below-class-minimum",
            Self::UnsupportedClass => "unsupported-class",
            Self::NoFit => "no-fit",
            Self::NoCapacity => "no-capacity",
            Self::LeaseEnded => "lease-ended",
            Self::NoCapableHost => "no-capable-host",
            Self::ExclusiveNotReady => "exclusive-not-ready",
        }
    }
}

impl From<RequestError> for RefusalCode {
    fn from(err: RequestError) -> Self {
        match err {
            RequestError::WallTimeNotPositive => Self::WallTimeNotPositive,
            RequestError::Label(LabelError::Reserved(_)) => Self::ReservedLabel,
            RequestError::Label(_) => Self::InvalidLabel,
            // A `disk-gib` or `exec` that is not a requirement names no host.
            RequestError::Requirements(_) => Self::NoFit,
        }
    }
}

/// Everything a provider needs to start one attempt, already checked.
///
/// The only constructor, [`StartSpec::new`], runs the wall-time and reserved-label rules
/// of the start contract of design 5.2 and the class checks, so a provider never sees a
/// request that broke one of them. The fence and retry rules of 5.2 are not modelled yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartSpec {
    lease: LeaseId,
    class: ResourceClass,
    request: Request,
    labels: Labels,
    request_id: Option<RequestId>,
}

impl StartSpec {
    /// Checks `request` against the start contract and the classes the provider
    /// serves, in this order: wall time positive, labels free of reserved keys and
    /// well formed, class known, class minimum met. The first failure is the code.
    pub fn new(
        lease: LeaseId,
        request: Request,
        known: &[ResourceClass],
    ) -> Result<Self, RefusalCode> {
        let labels = request.validate()?;
        let class = known
            .iter()
            .find(|class| class.id() == &request.class)
            .ok_or(RefusalCode::UnknownClass)?
            .clone();
        class
            .check(&request.resources)
            .map_err(|_| RefusalCode::BelowClassMinimum)?;
        Ok(Self {
            lease,
            class,
            request,
            labels,
            request_id: None,
        })
    }

    /// This spec carrying the id the consumer gave the request. A provider that queues
    /// reads it to tell a repeat of a request from a new one; it is never persisted by
    /// the lease table, and a spec without one is unchanged.
    pub fn with_request_id(mut self, id: RequestId) -> Self {
        self.request_id = Some(id);
        self
    }

    /// The consumer's id for the request, if it gave one.
    pub fn request_id(&self) -> Option<&RequestId> {
        self.request_id.as_ref()
    }

    /// The lease this attempt runs under.
    pub fn lease(&self) -> LeaseId {
        self.lease
    }

    /// The resolved class of the request.
    pub fn class(&self) -> &ResourceClass {
        &self.class
    }

    /// The request.
    pub fn request(&self) -> &Request {
        &self.request
    }

    /// The labels to write onto every billable resource of the attempt, the
    /// validation run label included.
    pub fn labels(&self) -> &Labels {
        &self.labels
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::Duration;

    use js_types::{Capabilities, ClassId, Resources};

    use super::*;

    fn request() -> Request {
        Request {
            class: ClassId::new("gpu-host").unwrap(),
            resources: Resources::new(1, 1, 1, Some("a100".into())).unwrap(),
            capabilities: Capabilities::new(),
            max_wall: Duration::from_secs(60),
            labels: BTreeMap::new(),
            validation_run: Some("val-1".into()),
        }
    }

    fn make(request: Request) -> Result<StartSpec, RefusalCode> {
        StartSpec::new(LeaseId::new(1, 1), request, &[ResourceClass::gpu_host()])
    }

    /// Asserts that a valid request builds a spec carrying the lease, the resolved
    /// class and the labels with the validation run stamped.
    #[test]
    fn valid_request_builds_a_spec() {
        let spec = make(request()).unwrap();
        assert_eq!(spec.lease(), LeaseId::new(1, 1));
        assert_eq!(spec.class().id().as_str(), "gpu-host");
        assert_eq!(spec.request().max_wall, Duration::from_secs(60));
        assert_eq!(
            spec.labels().get("jobscheduler.dev/validation-run"),
            Some("val-1")
        );
    }

    /// Asserts that a request whose `disk-gib` or `exec` is not a requirement is refused
    /// as one no host could serve, and that good values build a spec.
    #[test]
    fn a_bad_disk_or_exec_names_no_host() {
        for (key, value) in [("disk-gib", "lots"), ("exec", "vm")] {
            let mut r = request();
            r.capabilities.insert(key.into(), value.into());
            assert_eq!(make(r), Err(RefusalCode::NoFit), "{key}");
        }
        let mut r = request();
        r.capabilities.insert("disk-gib".into(), "10".into());
        r.capabilities.insert("exec".into(), "container".into());
        assert!(make(r).is_ok());
    }

    /// Asserts that each start-contract rule maps to its own code.
    #[test]
    fn each_rule_has_its_code() {
        let mut r = request();
        r.max_wall = Duration::ZERO;
        assert_eq!(make(r), Err(RefusalCode::WallTimeNotPositive));
        let mut r = request();
        r.labels.insert("jobscheduler.dev/run".into(), "x".into());
        assert_eq!(make(r), Err(RefusalCode::ReservedLabel));
        let mut r = request();
        r.labels.insert("k".into(), "BAD".into());
        assert_eq!(make(r), Err(RefusalCode::InvalidLabel));
        let mut r = request();
        r.class = ClassId::new("other").unwrap();
        assert_eq!(make(r), Err(RefusalCode::UnknownClass));
        let mut r = request();
        r.resources = Resources::new(1, 1, 0, None).unwrap();
        assert_eq!(make(r), Err(RefusalCode::BelowClassMinimum));
    }

    /// Asserts that the wall-time rule is checked before the class rule, so a request
    /// that breaks both reports the wall time.
    #[test]
    fn rules_run_in_the_documented_order() {
        let mut r = request();
        r.max_wall = Duration::ZERO;
        r.class = ClassId::new("other").unwrap();
        assert_eq!(make(r), Err(RefusalCode::WallTimeNotPositive));
    }

    /// Asserts that every code has a distinct constant string.
    #[test]
    fn codes_are_distinct_constants() {
        let codes = [
            RefusalCode::WallTimeNotPositive,
            RefusalCode::ReservedLabel,
            RefusalCode::InvalidLabel,
            RefusalCode::UnknownClass,
            RefusalCode::BelowClassMinimum,
            RefusalCode::UnsupportedClass,
            RefusalCode::NoFit,
            RefusalCode::NoCapacity,
            RefusalCode::LeaseEnded,
            RefusalCode::NoCapableHost,
            RefusalCode::ExclusiveNotReady,
        ];
        let names: std::collections::BTreeSet<_> = codes.iter().map(|c| c.as_str()).collect();
        assert_eq!(names.len(), codes.len());
        assert_eq!(RefusalCode::NoFit.as_str(), "no-fit");
        assert_eq!(RefusalCode::LeaseEnded.as_str(), "lease-ended");
        assert_eq!(RefusalCode::NoCapableHost.as_str(), "no-capable-host");
        assert_eq!(
            RefusalCode::ExclusiveNotReady.as_str(),
            "exclusive-not-ready"
        );
    }

    /// Asserts that a spec has no request id until one is given, that the id given is
    /// read back, and that giving one changes nothing else of the spec.
    #[test]
    fn the_request_id_is_optional_and_changes_nothing_else() {
        let plain = make(request()).unwrap();
        assert_eq!(plain.request_id(), None);
        let id = RequestId::new("r-1").unwrap();
        let named = plain.clone().with_request_id(id.clone());
        assert_eq!(named.request_id(), Some(&id));
        assert_eq!(
            (named.lease(), named.request(), named.labels()),
            (plain.lease(), plain.request(), plain.labels())
        );
    }
}
