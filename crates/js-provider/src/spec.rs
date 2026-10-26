use js_types::{LabelError, Labels, LeaseId, Request, RequestError, ResourceClass};

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
        }
    }
}

impl From<RequestError> for RefusalCode {
    fn from(err: RequestError) -> Self {
        match err {
            RequestError::WallTimeNotPositive => Self::WallTimeNotPositive,
            RequestError::Label(LabelError::Reserved(_)) => Self::ReservedLabel,
            RequestError::Label(_) => Self::InvalidLabel,
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
        })
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
        ];
        let names: std::collections::BTreeSet<_> = codes.iter().map(|c| c.as_str()).collect();
        assert_eq!(names.len(), codes.len());
        assert_eq!(RefusalCode::NoFit.as_str(), "no-fit");
        assert_eq!(RefusalCode::LeaseEnded.as_str(), "lease-ended");
    }
}
