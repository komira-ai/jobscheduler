//! The configuration of a server with an elastic pool: the file the server reads
//! (`Config::from_config_file`), and the cell's provider kind, `sim-cloud`, that hands the
//! server's scaler the pool's [`Cloud`] behind its recording wrapper.
//!
//! A kind is a plain function (`Kinds::with` takes a `fn`), so it cannot be given the
//! cloud. [`read`] leaves the cloud in a thread-local slot just before it has the server
//! read the file, and the kind takes it from there; the file is read on the caller's
//! thread, so the kind finds the cloud of that read. The kind is the only reader of the
//! slot.

use std::cell::RefCell;
use std::path::Path;
use std::time::Duration;

use js_lease_server::{
    BuildCtx, Built, Config, ElasticBuilt, Floor, KindError, Kinds, OwnerOnlyFiles, SharedClock,
    StartupError,
};
use js_types::{ExecMode, Model};

use super::{Cloud, MAX_AGE, MIN_HOST_LIFETIME};

/// The name of the cell's provider kind.
pub const KIND: &str = "sim-cloud";

/// The address the server advertises to the machines it launches: a reserved
/// documentation domain, never reached (the agents are the cell's nodes).
pub const ADVERTISE_URL: &str = "https://scheduler.example.org";

/// Where the boot script of a launched machine would fetch the agent from, with its
/// digest: never fetched.
const AGENT_URL: &str = "https://scheduler.example.org/agent";
const AGENT_SHA256: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

thread_local! {
    /// The cloud the next pool of the `sim-cloud` kind is built over, left by [`read`].
    static NEXT: RefCell<Option<Cloud>> = const { RefCell::new(None) };
}

/// The server's configuration read from the file at `path`, its `sim-cloud` pool built
/// over `cloud`, on `clock`.
///
/// # Errors
///
/// When the server refuses the file, as `Config::from_config_file`.
pub fn read(path: &Path, cloud: &Cloud, clock: SharedClock) -> Result<Config, StartupError> {
    NEXT.set(Some(cloud.clone()));
    let kinds = Kinds::standard().with(KIND, sim_cloud);
    Config::from_config_file(path, &kinds, &OwnerOnlyFiles, clock)
}

/// The `sim-cloud` kind: the cloud [`read`] left, behind its recording wrapper. It reads
/// no key of its table. Without a cloud left (a second pool of the kind in one file) it
/// refuses the pool.
fn sim_cloud(_table: &toml::Table, _ctx: &BuildCtx<'_>) -> Result<Built, KindError> {
    let missing = KindError {
        field: "kind".to_owned(),
        reason: "no cloud was left for this read of the file".to_owned(),
    };
    NEXT.take().map(built).ok_or(missing)
}

/// The pool of `cloud`: its provider the cloud behind its recording wrapper, its floor the
/// GPUs of the cloud's shape, of the shape's class, on hosts that run containers.
fn built(cloud: Cloud) -> Built {
    let shape = cloud.shape();
    let class = shape.gpu_class.as_deref();
    let floor = Floor {
        gpus: shape.gpus,
        gpu_class: class.and_then(|class| Model::parse(class).ok()),
    };
    Built::Elastic(ElasticBuilt {
        provider: cloud.provider(),
        floor,
        exec: vec![ExecMode::Container],
    })
}

/// The configuration file of a server whose one pool is `cloud`'s elastic pool: the lease
/// file `state`, the client token in the file `token`, placing as `placement` (its
/// `--placement` spelling), usage recording off, and booting `initialise` (which reads the
/// lease file, the join tokens and the scaler's saved state when they are there, so a restart
/// over the same files reads them too).
///
/// The pool is of class `whole-node` and launches for real (`launch = "live"`); its
/// spending caps are what `max_instances` machines of its dearest offer cost, an hour and a
/// day, so they never refuse a launch the instance limit allows. It names no shared join
/// token: only the one-time tokens of its launched machines register hosts.
#[must_use]
pub fn text(cloud: &Cloud, state: &Path, token: &Path, placement: &str) -> String {
    let pool = cloud.pool();
    let dearest = pool.offers().iter().map(|offer| offer.cents_per_hour).max();
    let hour = dearest
        .unwrap_or(0)
        .saturating_mul(pool.max_instances())
        .max(1);
    let day = hour.saturating_mul(24);
    format!(
        "[server]\n\
         state_file = {state}\n\
         boot = \"initialise\"\n\
         client_token_file = {token}\n\
         listen = \"127.0.0.1:0\"\n\
         agent_listen = \"127.0.0.2:0\"\n\
         advertise_url = {advertise}\n\
         max_lease_wall = \"2h\"\n\
         max_ttl = \"2h\"\n\
         usage_off = true\n\
         placement = {placement}\n\
         \n\
         [[pool]]\n\
         name = {name}\n\
         class = \"whole-node\"\n\
         max_instances = {max}\n\
         max_spend_per_hour = \"{hour}\"\n\
         max_spend_per_day = \"{day}\"\n\
         idle = {idle}\n\
         min_host_lifetime = {lifetime}\n\
         max_age = {age}\n\
         launch = \"live\"\n\
         agent = {{ x86_64 = {{ url = {url}, sha256 = {sha} }} }}\n\
         provider = {{ kind = {kind} }}\n",
        state = quoted(&state.display().to_string()),
        token = quoted(&token.display().to_string()),
        advertise = quoted(ADVERTISE_URL),
        placement = quoted(placement),
        name = quoted(pool.name()),
        max = pool.max_instances(),
        hour = dollars(hour),
        day = dollars(day),
        idle = millis(pool.idle_time()),
        lifetime = millis(MIN_HOST_LIFETIME),
        age = millis(MAX_AGE),
        url = quoted(AGENT_URL),
        sha = quoted(AGENT_SHA256),
        kind = quoted(KIND),
    )
}

/// `cents` as the file spells money: dollars with two decimals.
fn dollars(cents: u64) -> String {
    format!("{}.{:02}", cents / 100, cents % 100)
}

/// `duration` as the file spells a duration: whole milliseconds, quoted.
fn millis(duration: Duration) -> String {
    quoted(&format!("{}ms", duration.as_millis()))
}

/// `text` as a TOML basic string, which the JSON string grammar is a part of.
fn quoted(text: &str) -> String {
    serde_json::Value::from(text).to_string()
}
