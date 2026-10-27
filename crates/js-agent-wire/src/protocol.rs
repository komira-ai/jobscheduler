//! The protocol version, and how a server and an agent agree on one.
//!
//! The wire is `jobscheduler.agent.v1`: the number after `v` is the major version. The
//! rule:
//!
//! - Servers upgrade before agents. An older agent therefore meets a server that speaks
//!   its major, and a newer agent may meet a server that does not.
//! - A [`crate::Hello`] states the range of majors the agent speaks. The server picks the
//!   highest major both speak and names it in [`crate::Welcome`], or refuses the hello
//!   with [`crate::refusal::UNSUPPORTED_PROTOCOL`] when the ranges do not meet. The agent
//!   treats that refusal as final for the server it asked: it does not retry in a loop.
//! - Within a major, fields are only added. A receiver ignores fields it does not know,
//!   and a field a sender may leave out has a default on decode. A change that cannot be
//!   made that way is a new major.
//! - A hello with no `protocol` field (sent before the field existed) speaks major 1.

use serde::{Deserialize, Serialize};

use crate::error::{Rule, ValidationError};

/// The wire's name, for logs and for the transport's content type.
pub const PROTOCOL_NAME: &str = "jobscheduler.agent.v1";
/// The newest major this crate speaks.
pub const PROTOCOL_MAJOR: u32 = 1;
/// The oldest major this crate still speaks.
pub const OLDEST_PROTOCOL_MAJOR: u32 = 1;

/// The majors a peer speaks, both ends included.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolRange {
    /// The oldest major spoken; at least 1.
    pub min: u32,
    /// The newest major spoken; at least `min`.
    pub max: u32,
}

impl ProtocolRange {
    /// The range this crate speaks.
    pub const THIS: Self = Self {
        min: OLDEST_PROTOCOL_MAJOR,
        max: PROTOCOL_MAJOR,
    };

    /// The newest major both ranges speak, when they meet.
    pub fn negotiate(&self, ours: &Self) -> Option<u32> {
        let newest = self.max.min(ours.max);
        let oldest = self.min.max(ours.min);
        (oldest <= newest).then_some(newest)
    }

    /// Refuses a range that is empty or starts below major 1.
    pub(crate) fn check(
        &self,
        field: &'static str,
        code: &'static str,
    ) -> Result<(), ValidationError> {
        let broken = [self.min == 0, self.min > self.max]
            .into_iter()
            .any(|bad| bad);
        if broken {
            return Err(ValidationError::new(field, Rule::OutOfRange, code));
        }
        Ok(())
    }
}

/// The range a message with no `protocol` field speaks: major 1 alone.
pub(crate) fn first_major_range() -> ProtocolRange {
    ProtocolRange { min: 1, max: 1 }
}

/// The major a message with no `protocol` field names.
pub(crate) fn first_major() -> u32 {
    1
}
