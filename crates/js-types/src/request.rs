use std::collections::BTreeMap;
use std::time::Duration;

use crate::{
    Capabilities, ClassId, LabelError, Labels, Requirements, RequirementsError, ReservedKey,
    Resources,
};

/// A request broke a rule that holds for every class and provider (design 5.2).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RequestError {
    /// The maximum wall time was zero.
    #[error("the maximum wall time must be positive")]
    WallTimeNotPositive,
    /// A label was refused.
    #[error(transparent)]
    Label(#[from] LabelError),
    /// The `disk-gib` or `exec` capability is not a requirement.
    #[error(transparent)]
    Requirements(#[from] RequirementsError),
}

/// What a caller asks for: one run of a class with resources, capabilities, a wall
/// time cap and labels. Fields are plain data; [`Request::validate`] applies the rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// The resource class the run is admitted under.
    pub class: ClassId,
    /// The resources the run needs.
    pub resources: Resources,
    /// Capabilities a host must report.
    pub capabilities: Capabilities,
    /// The longest the run may take. Required, and positive.
    pub max_wall: Duration,
    /// Labels the caller supplies. The reserved prefix is refused.
    pub labels: BTreeMap<String, String>,
    /// The validation run id the caller minted, if the run belongs to a validation.
    pub validation_run: Option<String>,
}

impl Request {
    /// Applies the rules every request must meet: a positive wall time, labels free
    /// of reserved keys, and a `disk-gib` and `exec` capability that are requirements
    /// ([`Requirements::try_from_request`]). Returns the labels to stamp on every billable resource: the
    /// caller's, plus the validation run label when a validation run id is set.
    pub fn validate(&self) -> Result<Labels, RequestError> {
        if self.max_wall.is_zero() {
            return Err(RequestError::WallTimeNotPositive);
        }
        Requirements::try_from_request(self)?;
        let labels = Labels::from_caller(&self.labels)?;
        match &self.validation_run {
            None => Ok(labels),
            Some(id) => Ok(labels.with_reserved(ReservedKey::ValidationRun, id)?),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> Request {
        Request {
            class: ClassId::new("gpu-host").unwrap(),
            resources: Resources::new(1, 1, 1, Some("a100".into())).unwrap(),
            capabilities: Capabilities::new(),
            max_wall: Duration::from_secs(60),
            labels: BTreeMap::new(),
            validation_run: None,
        }
    }

    /// Asserts that a valid request without a validation run yields only the caller's
    /// labels.
    #[test]
    fn valid_request_keeps_caller_labels() {
        let mut r = request();
        r.labels.insert("team".into(), "infra".into());
        let labels = r.validate().unwrap();
        assert_eq!(labels.get("team"), Some("infra"));
        assert_eq!(labels.iter().count(), 1);
    }

    /// Asserts that a validation run id becomes the reserved validation-run label.
    #[test]
    fn validation_run_becomes_a_label() {
        let mut r = request();
        r.validation_run = Some("val-7".into());
        let labels = r.validate().unwrap();
        assert_eq!(labels.get("jobscheduler.dev/validation-run"), Some("val-7"));
    }

    /// Asserts that a zero wall time is refused.
    #[test]
    fn zero_wall_time_is_refused() {
        let mut r = request();
        r.max_wall = Duration::ZERO;
        assert_eq!(r.validate(), Err(RequestError::WallTimeNotPositive));
    }

    /// Asserts that a request whose `disk-gib` or `exec` is not a requirement is refused
    /// by `validate`, and one that names good values passes.
    #[test]
    fn bad_disk_or_mode_is_refused() {
        let mut r = request();
        r.capabilities.insert("disk-gib".into(), "100".into());
        r.capabilities.insert("exec".into(), "sandbox-exec".into());
        assert!(r.validate().is_ok());
        r.capabilities.insert("disk-gib".into(), "lots".into());
        assert_eq!(
            r.validate(),
            Err(RequestError::Requirements(RequirementsError::BadDisk(
                "lots".into()
            )))
        );
        r.capabilities.insert("disk-gib".into(), "1".into());
        r.capabilities.insert("exec".into(), "vm".into());
        assert!(matches!(
            r.validate(),
            Err(RequestError::Requirements(RequirementsError::BadExec(_)))
        ));
    }

    /// Asserts that a reserved caller label and a malformed validation run id are
    /// refused as label errors.
    #[test]
    fn bad_labels_are_refused() {
        let mut r = request();
        r.labels
            .insert("jobscheduler.dev/tenant".into(), "x".into());
        assert!(matches!(
            r.validate(),
            Err(RequestError::Label(LabelError::Reserved(_)))
        ));
        let mut r = request();
        r.validation_run = Some("BAD".into());
        assert!(matches!(
            r.validate(),
            Err(RequestError::Label(LabelError::BadValue { .. }))
        ));
    }
}
