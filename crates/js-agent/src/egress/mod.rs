//! Egress for jobs that run in `crun` containers: a ceiling the agent is configured with
//! (default none), and a level each job asks for at or under it (design 5.8).
//!
//! - [`Config`] (portable): the flags `--egress`, `--egress-allow`, `--egress-dns`,
//!   `--egress-forward-port`, `--egress-pool`, `--egress-ip`, `--egress-nft` and
//!   `--egress-nsenter`, checked once at start; [`Config::ceiling`] is the most a job may
//!   ask for.
//! - [`admit`] (portable): the comparison of a start's `network` and `ports` with the
//!   ceiling, which every admission calls; a job above it is refused, never lowered.
//! - [`ruleset`] (portable): the nftables text of the ceiling (`allow-list` or `open`) and
//!   the comparison that reads it back.
//! - [`Egress`] (Linux): the privileged part, which runs in the agent process, as root, in
//!   the host's initial network namespace: the veth pair, the addresses and the default
//!   route, the firewall and its read-back. It never runs in the job's container and never
//!   as the job user. The design and what each step defends against are in
//!   `docs/design/agent-egress.md`.
//!
//! The runtime that starts the containers calls, in this order: [`Egress::check`] before
//! every create (a failure is `egress-unchecked` and creates nothing), `Oci::start`,
//! `Oci::container_pid`, [`Egress::attach`] with the job's level, and
//! [`Attachment::release`] when the run ends. A `none` job is not attached: its container has
//! loopback only, and its init waits for no route.

mod admit;
mod config;
#[cfg(target_os = "linux")]
mod host;
pub mod ruleset;

pub use admit::{NetworkRefusal, admit};
pub use config::{
    Config, ConfigError, DEFAULT_IP, DEFAULT_NFT, DEFAULT_NSENTER, DEFAULT_POOL, DNS_MAX,
    FORWARD_PORT_MAX, Flags, Mode, Pool, ROUTE_WAIT,
};
#[cfg(target_os = "linux")]
pub use host::{Attachment, Egress, EgressError, HOST_LOCK};
