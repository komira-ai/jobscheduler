//! Which release of the agent a node says hello as.
//!
//! The wire crate (`js-agent-wire`) has only dropped fields since v0.1.208, so an agent of
//! the previous release (v0.1.213, recorded in `crates/js-agent/tests/agent/skew/v0.1.213.json`)
//! differs on the wire from this one only in what its hello says: the agent version, the
//! `accepts_host_credential` field this release no longer has, and its capability set. Its heartbeats are built from the
//! same optional fields and carry nothing this server cannot read. The profile changes the
//! hello and nothing else; the node's behaviour past it is the cell's agent as before.
//!
//! What the previous release's hello says that this one's does not:
//!
//! - `agent_version` is `0.1.0`;
//! - it names no `agent_process` in its hello and no `seq` in its heartbeats (the fields
//!   came after that release), so a server serves it as an agent that does not number its
//!   beats;
//! - `accepts_host_credential` is `true`, a field of the hello this release dropped (the
//!   server ignores it, as it ignores any field it does not know);
//! - the capabilities `start-fence`, `runtime`, `exec`, `os`, `egress` and `arch` beside
//!   the ones the shape and the per-job messages give, a capability the shape already
//!   names keeping the shape's value.

use js_agent_wire::{Heartbeat, Hello, ProtocolRange, START_FENCE_CAPABILITY};
use serde_json::Value;

/// The agent version of the previous release's recorded hello.
const PREVIOUS_VERSION: &str = "0.1.0";

/// The capabilities of the previous release's recorded hello beside the per-job one, as
/// that hello names them.
const PREVIOUS_CAPABILITIES: [(&str, &str); 6] = [
    ("arch", "x86_64"),
    ("egress", "none"),
    ("exec", "container"),
    ("os", "linux"),
    ("runtime", "container"),
    (START_FENCE_CAPABILITY, "true"),
];

/// The protocol range of [`AgentProfile::ProtocolAhead`]: majors this server does not
/// speak.
const AHEAD: ProtocolRange = ProtocolRange { min: 2, max: 3 };

/// The release whose hello an agent node sends.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AgentProfile {
    /// The agent of this release.
    #[default]
    Current,
    /// The agent of the previous release (v0.1.213), as its recorded hello says.
    PreviousRelease,
    /// An agent that asks for protocol majors 2 to 3, which this server refuses with 400
    /// `unsupported-protocol`: a hello no server of this release can take. It is for a
    /// scenario to show that a skew check can fail.
    ProtocolAhead,
}

impl AgentProfile {
    /// `hello` as an agent of this profile sends it, given the hello of the current agent.
    pub(super) fn shape(self, mut hello: Hello) -> Hello {
        match self {
            Self::Current => {}
            Self::PreviousRelease => {
                PREVIOUS_VERSION.clone_into(&mut hello.agent_version);
                hello.agent_process = None;
                let named = &mut hello.capabilities;
                for (key, value) in PREVIOUS_CAPABILITIES {
                    named
                        .entry(key.to_owned())
                        .or_insert_with(|| value.to_owned());
                }
            }
            Self::ProtocolAhead => hello.protocol = AHEAD,
        }
        hello
    }

    /// `hello` as the JSON body an agent of this profile sends: the previous release's body
    /// also names the `accepts_host_credential` field it still had.
    ///
    /// # Errors
    ///
    /// When the hello does not encode, which no wire type fails to do.
    pub(super) fn encode(self, hello: &Hello) -> Result<String, serde_json::Error> {
        let mut body = serde_json::to_value(hello)?;
        let previous = (self == Self::PreviousRelease).then_some(true);
        let fields = body.as_object_mut().into_iter();
        for (fields, flag) in fields.zip(previous) {
            fields.insert("accepts_host_credential".to_owned(), Value::Bool(flag));
        }
        serde_json::to_string(&body)
    }

    /// `beat` as an agent of this profile sends it: the previous release does not number its
    /// heartbeats.
    pub(super) fn shape_beat(self, mut beat: Heartbeat) -> Heartbeat {
        if self == Self::PreviousRelease {
            beat.seq = None;
        }
        beat
    }
}
