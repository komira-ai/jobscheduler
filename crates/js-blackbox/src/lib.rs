//! What the black-box tests of the released js-agent and js-server judge by.
//!
//! The tests (tests/released.rs) take the released artifacts as the farm takes them: the js-agent
//! `.deb` of the release, installed with apt from the suite `testing` and started under systemd
//! with flags of the farm's shape ([`AGENT_FLAGS`]), and the js-server image of the release (the
//! js-lease-server OCI image, a file of the GitHub release), loaded into Docker and started with
//! the control host's command line ([`SERVER_ARGS`]), their neighbours faked. They drive both only
//! from outside: the job service's API, the agent's health route, the files the package installs,
//! the jobs the agent runs. This crate is the part of them that decides: the pass conditions as
//! numbers (the constants below), the wait that measures them on an injected clock
//! ([`Watch`]: the black-box test's is the system's, and it waits for real; the judge's tests move
//! a clock of their own), the readers of the job service's answers ([`host`], [`job`], [`result`]) and of
//! the programs the tests run ([`loaded_image`], [`previous_version`]), and the farm's shapes with
//! the values a test fills in ([`render`]).
//!
//! # The pass conditions
//!
//! | test | passes when |
//! |---|---|
//! | J6, the released server image | `/readyz` answers 200 within [`READY_WITHIN`] of the container's start, and a job run through the released agent ends with exit code 0 |
//! | J1, the released `.deb` with the farm's flags | the host is in a session (listed live by the server, its health route 200) within [`SESSION_WITHIN`] of the unit's start, and a job ends with exit code 0 |
//! | the server restarted while a job runs | the job ends with exit code 0 in its first attempt, and no listing of the hosts read after the restart shows the host lost |
//! | the agent updated while a job runs | the agent of the previous release is updated to the released one: the job is handed over (it ends with exit code 0 in its first attempt) and the host lists the new version within [`SESSION_WITHIN`] |
//! | the server unreachable for [`OUTAGE`] | the job keeps running through the outage, the agent is back in a session within [`BACK_WITHIN`] of the server's return, and the job ends with exit code 0 |
//! | the server's name not found for [`OUTAGE`] after the first contact | the host is listed live and its health route answers 200 at every probe of the outage |
//! | J5, [`BURST`] starts at [`MAX_PLACEMENTS`] placements, the mount provider's store stalled for [`STALL`] | in no [`CALL_WINDOW`] do more than [`MOST_CALLS_IN_WINDOW`] Prepare calls, nor more than as many Mount calls, reach the provider; one Release per job reaches it; every job ends, none as its own fault ([`own_fault`]) |
//! | J5, the same burst with the limit met at Prepare (the provider places at Prepare) | the same |
//! | J5, the mount provider restarted while [`WAITING`] starts wait | at least [`WAITING`] Prepare calls reach the provider after its restart, and every job ends with exit code 0 |
//! | J5, the mount provider answering unavailable for [`PROVIDER_OUTAGE`] while [`WAITING`] starts wait | every job ends with exit code 0 |
//! | J5, a ref the mount provider refuses | the start fails `provider-refused` (the job's own fault), after one Prepare |

mod provider;
mod read;
mod render;
mod watch;

use std::time::Duration;

pub use provider::{
    Busiest, FAILED_TO_START, INPUT_UNAVAILABLE, PROVIDER_REFUSED, ProviderCall, busiest,
    own_fault, per_second, provider_calls,
};
pub use read::{
    HostSeen, JobSeen, ResultSeen, Version, host, job, loaded_image, previous_version, result,
};
pub use render::{arguments, render};
pub use watch::{Held, Seen, Waited, Watch};

/// J6: how long after the server's container starts its readiness route must answer 200.
pub const READY_WITHIN: Duration = Duration::from_secs(60);

/// J1: how long after the agent's unit starts the host must be in a session; also how long an
/// updated agent has to list its new version.
pub const SESSION_WITHIN: Duration = Duration::from_secs(60);

/// How long after the server is reachable again the agent must be back in a session.
pub const BACK_WITHIN: Duration = Duration::from_secs(30);

/// How long the server is unreachable, and how long its name is not found.
pub const OUTAGE: Duration = Duration::from_secs(60);

/// J5: the job starts of the burst.
pub const BURST: usize = 20;

/// J5: the placements the mount provider holds at once.
pub const MAX_PLACEMENTS: usize = 2;

/// J5: how long the mount provider's store stalls at the start of the burst.
pub const STALL: Duration = Duration::from_secs(30);

/// J5: the window the provider's calls are counted in.
pub const CALL_WINDOW: Duration = Duration::from_secs(30);

/// J5: the most calls of one kind (Prepare, Mount) that may reach the provider in a
/// [`CALL_WINDOW`]: one per start of the burst.
pub const MOST_CALLS_IN_WINDOW: usize = 20;

/// J5: the starts that wait through the provider's restart and through its outage, three times
/// the placements it holds.
pub const WAITING: usize = 6;

/// J5: how long the mount provider answers every call unavailable.
pub const PROVIDER_OUTAGE: Duration = Duration::from_secs(60);

/// How often a wait or a hold probes.
pub const PROBE_EVERY: Duration = Duration::from_secs(1);

/// The job service's word for a host it hears.
pub const LIVE: &str = "HOST_STATE_LIVE";

/// The job service's word for a host it has not heard within its grace.
pub const LOST: &str = "HOST_STATE_LOST";

/// The job service's word for a running job.
pub const RUNNING: &str = "JOB_STATE_RUNNING";

/// The job service's word for a job that ended with exit code 0.
pub const SUCCEEDED: &str = "JOB_STATE_SUCCEEDED";

/// The shape of the flags file of the farm's Linux hosts (`/etc/js-agent/flags`, which the
/// packaged `js-agent-crun.service` reads with `--flags-file`), with placeholders for what is the
/// farm's own. The file's header says what it mirrors.
pub const AGENT_FLAGS: &str = include_str!("../farm/js-agent.flags");

/// The shape of the control host's js-server command line, one argument a line, with
/// placeholders for the addresses. The file's header says what it mirrors.
pub const SERVER_ARGS: &str = include_str!("../farm/js-server.args");

/// The shape of the farm's Linux pool file, which the server's first `--agent-pool-config`
/// names.
pub const FARM_POOL: &str = include_str!("../farm/pool.toml");

/// The shape of the farm's Mac pool file, which the server's second `--agent-pool-config` names.
pub const MAC_POOL: &str = include_str!("../farm/pool-mac.toml");
