//! Egress for jobs that run in `crun` containers: one configured mode, default none.
//!
//! - [`Config`] (portable): the flags `--egress`, `--egress-allow`, `--egress-dns`,
//!   `--egress-pool`, `--egress-ip` and `--egress-nft`, checked once at start.
//! - [`ruleset`] (portable): the nftables text of an allow-list and the comparison that
//!   reads it back.
//! - [`Egress`] (Linux): the privileged part, which runs in the agent process, as root, in
//!   the host's initial network namespace: the veth pair, the addresses and the default
//!   route, the firewall and its read-back. It never runs in the job's container and never
//!   as the job user. The design and what each step defends against are in
//!   `docs/design/agent-egress.md`.
//!
//! The runtime that starts the containers calls, in this order: [`Egress::check`] before
//! every create (a failure is `egress-unchecked` and creates nothing), `Oci::start`,
//! `Oci::container_pid`, [`Egress::attach`], and [`Attachment::release`] when the run ends.

mod config;
#[cfg(target_os = "linux")]
mod host;
pub mod ruleset;

pub use config::{
    Config, ConfigError, DEFAULT_IP, DEFAULT_NFT, DEFAULT_POOL, DNS_MAX, Flags, Mode, Pool,
    ROUTE_WAIT,
};
#[cfg(target_os = "linux")]
pub use host::{Attachment, Egress, EgressError};
