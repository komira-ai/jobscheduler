//! What a machine is told to do when it boots, as data. The service builds a [`BootPlan`],
//! renders it into a [`BootPayload`](crate::BootPayload) and hands that to a provider;
//! a provider never sees a plan.

use crate::ids::HostName;

/// The container program the agent starts jobs with. The bootstrap names it in the agent's
/// unit and checks it exists; a provider whose image keeps it elsewhere installs it here.
pub const CONTAINER_BIN: &str = "/usr/bin/docker";

/// The runtime the agent runs jobs with (`--runtime`): named in the agent's unit, since the
/// Linux farm's own unit names `crun` and no host gets a runtime by default.
///
/// Rollout order: an agent that does not know `--runtime` exits 2 at start on this flag. The
/// plan's [`Artifact`] is a URL and a digest and carries no version, so the boot payload
/// cannot tell whether the agent it installs knows the flag and cannot gate on it: the pinned
/// artifact must be bumped to an agent that does before a lease server that renders this flag
/// is deployed (see `docs/runbooks/first-live-launch.md`).
pub const RUNTIME: &str = "docker";

/// The agent's state root, which the bootstrap creates (owner-only) and names in the
/// agent's unit. It lies on the machine's root disk.
pub const STATE_ROOT: &str = "/var/lib/js-agent/state";

/// The agent's scratch root, named in the agent's unit. The agent refuses to start when
/// it shares a device with [`STATE_ROOT`], so a provider whose machines have one disk
/// mounts a file system of its own here in its `host_prep`, and the unit does not start
/// without that mount.
pub const SCRATCH_ROOT: &str = "/var/lib/js-agent/scratch";

/// The name of a place in a plan where a secret goes. The plan holds the name, never
/// the secret.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SlotName(pub String);

/// One step a booting machine runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootStep {
    /// Runs a program with these arguments. Never a shell line, and never a secret.
    Run(Vec<String>),
    /// Writes a secret to a file the machine owner alone reads.
    SecretFile {
        /// Where to write it.
        path: String,
        /// Which secret.
        slot: SlotName,
    },
    /// Downloads a file and checks it against a hash.
    Fetch {
        /// Where from.
        url: String,
        /// The SHA-256 the file must have.
        sha256: [u8; 32],
        /// Where to put it.
        dest: String,
    },
}

/// A file to download and the hash it must have.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artifact {
    /// Where from.
    pub url: String,
    /// The SHA-256 the file must have.
    pub sha256: [u8; 32],
}

/// Everything one launch tells its machine, with secrets as named slots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootPlan {
    /// The agent to install.
    pub agent: Artifact,
    /// The scheduler's address, which the agent dials.
    pub scheduler_url: String,
    /// The pool the machine is for.
    pub pool: String,
    /// The machine's name.
    pub host: HostName,
    /// What the provider's stock image lacks.
    pub host_prep: Vec<BootStep>,
    /// What the pool's network mode adds.
    pub network: Vec<BootStep>,
}

impl BootPlan {
    /// The line that names the machine to the scheduler when its agent first calls:
    /// `<pool>:<name>`.
    pub fn instance(&self) -> String {
        format!("{}:{}", self.pool, self.host)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{AcquireId, SchedulerId};

    /// Asserts that a plan names its machine to the scheduler as `<pool>:<name>`, and
    /// that a secret appears in a plan as a slot name, never as text.
    #[test]
    fn a_plan_names_its_machine_and_holds_secrets_as_slots() {
        let host = HostName::derive(
            &SchedulerId::new("s").unwrap(),
            &AcquireId::new("a").unwrap(),
        );
        let plan = BootPlan {
            agent: Artifact {
                url: "https://example.org/agent".to_owned(),
                sha256: [7; 32],
            },
            scheduler_url: "https://sched.example.org:7301".to_owned(),
            pool: "gpu".to_owned(),
            host: host.clone(),
            host_prep: vec![BootStep::Run(vec!["apt-get".into(), "update".into()])],
            network: vec![BootStep::SecretFile {
                path: "/etc/net.key".to_owned(),
                slot: SlotName("network.key".to_owned()),
            }],
        };
        assert_eq!(plan.instance(), format!("gpu:{host}"));
        assert_eq!(plan.clone(), plan);
        let fetch = BootStep::Fetch {
            url: "https://example.org/x".to_owned(),
            sha256: [0; 32],
            dest: "/opt/x".to_owned(),
        };
        assert_ne!(fetch, plan.host_prep[0]);
        let shown = format!("{plan:?}");
        assert!(shown.contains("network.key"));
    }
}
