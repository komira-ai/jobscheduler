//! The configuration of a server with an elastic pool: the file the server reads
//! (`Config::from_config_file`), and the cell's provider kind, `sim-cloud`, that hands the
//! server's scaler the pool's [`Cloud`] behind its recording wrapper.
//!
//! A kind is a plain function (`Kinds::with` takes a `fn`), so it cannot be given the
//! cloud. [`read`] leaves the clouds in a thread-local slot just before it has the server
//! read the file, and the kind takes one from there; the file is read on the caller's
//! thread, so the kind finds the clouds of that read. The kind is the only reader of the
//! slot. A file of one elastic pool names its provider `{ kind = "sim-cloud" }` and the
//! kind takes the one cloud; the file of a server of [`crate::ServerSpec::ordered`] has a
//! `[[machine_provider]]` of the kind for each cloud, whose key `cloud` names the cloud it
//! takes ([`ordered_text`]).

use std::cell::RefCell;
use std::path::Path;
use std::time::Duration;

use js_lease_server::{
    BuildCtx, Built, Config, ElasticBuilt, Floor, KindError, Kinds, OwnerOnlyFiles, SharedClock,
    StartupError,
};
use js_types::{ExecMode, Model};

use super::{Cloud, DEFAULT_IDLE, ElasticPool, MAX_AGE, MIN_HOST_LIFETIME};

/// The name of the cell's provider kind.
pub const KIND: &str = "sim-cloud";

/// The address the server advertises to the machines it launches: a reserved
/// documentation domain, never reached (the agents are the cell's nodes).
pub const ADVERTISE_URL: &str = "https://scheduler.example.org";

/// Where the boot script of a launched machine would fetch the agent from, with its
/// digest: never fetched.
const AGENT_URL: &str = "https://scheduler.example.org/agent";
const AGENT_SHA256: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

/// The key of a `sim-cloud` table that names the cloud it is built over.
const CLOUD_KEY: &str = "cloud";

thread_local! {
    /// The clouds the tables of the `sim-cloud` kind are built over, left by [`read`].
    static NEXT: RefCell<Vec<Cloud>> = const { RefCell::new(Vec::new()) };
}

/// The server's configuration read from the file at `path`, its tables of the `sim-cloud`
/// kind built over `clouds`, on `clock`.
///
/// # Errors
///
/// When the server refuses the file, as `Config::from_config_file`.
pub fn read(path: &Path, clouds: &[Cloud], clock: SharedClock) -> Result<Config, StartupError> {
    NEXT.set(clouds.to_vec());
    let kinds = Kinds::standard().with(KIND, sim_cloud);
    let read = Config::from_config_file(path, &kinds, &OwnerOnlyFiles, clock);
    NEXT.set(Vec::new());
    read
}

/// The `sim-cloud` kind: a cloud [`read`] left, behind its recording wrapper: the one its
/// key `cloud` names (by its pool's name), or the first left for a table without the key.
/// Without such a cloud left (a second table of the kind for one cloud) it refuses the
/// table.
fn sim_cloud(table: &toml::Table, _ctx: &BuildCtx<'_>) -> Result<Built, KindError> {
    let named = table.get(CLOUD_KEY).and_then(toml::Value::as_str);
    let missing = KindError {
        field: CLOUD_KEY.to_owned(),
        reason: "no cloud was left for this table in this read of the file".to_owned(),
    };
    let taken = NEXT.with_borrow_mut(|left| {
        let at = left
            .iter()
            .position(|cloud| named.is_none_or(|name| cloud.pool().name() == name));
        at.map(|at| left.remove(at))
    });
    taken.map(elastic).map(Built::Elastic).ok_or(missing)
}

/// The pool of `cloud`: its provider the cloud behind its recording wrapper, its floor the
/// GPUs of the cloud's shape, of the shape's class, on hosts that run containers.
fn elastic(cloud: Cloud) -> ElasticBuilt {
    let shape = cloud.shape();
    let class = shape.gpu_class.as_deref();
    let floor = Floor {
        gpus: shape.gpus,
        gpu_class: class.and_then(|class| Model::parse(class).ok()),
    };
    ElasticBuilt {
        provider: cloud.provider(),
        floor,
        idle_costs: false,
        exec: vec![ExecMode::Container],
    }
}

/// The files the configuration file of an elastic pool's server names.
pub(crate) struct Files<'a> {
    /// The state file.
    pub(crate) state: &'a Path,
    /// The file that holds the client token.
    pub(crate) token: &'a Path,
    /// The store file (`server.store`): the line of the store the server keeps its state in.
    pub(crate) store: &'a Path,
    /// Whether that store is SQLite, which needs `server.allow_sqlite_store = true`.
    pub(crate) allow_sqlite: bool,
}

/// The configuration file of a server whose one pool is `cloud`'s elastic pool: the lease
/// file, the client token's file and the store's file of `files`,
/// usage recording off, and booting `initialise` (which reads
/// the state file, and the join tokens and the scaler's saved state from the store, when
/// they are there, so a restart over the same files and store reads them too).
///
/// The pool is of class `whole-node` and launches for real (`launch = "live"`); its
/// spending caps are what `max_instances` machines of its dearest offer cost, an hour and a
/// day, so they never refuse a launch the instance limit allows. It names no shared join
/// token: only the one-time tokens of its launched machines register hosts. A queued job
/// waits for a machine two hours (`max_queue_wait`, the server's default being one), longer
/// than the longest the elastic fault scenarios keep demand waiting through the cloud's
/// cooldowns and its brake.
#[must_use]
pub(crate) fn text(cloud: &Cloud, files: &Files<'_>) -> String {
    let pool = cloud.pool();
    let dearest = pool.offers().iter().map(|offer| offer.cents_per_hour).max();
    let hour = dearest
        .unwrap_or(0)
        .saturating_mul(pool.max_instances())
        .max(1);
    let day = hour.saturating_mul(24);
    format!(
        "{server}\
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
         max_queue_wait = \"2h\"\n\
         agent = {{ x86_64 = {{ url = {url}, sha256 = {sha} }} }}\n\
         provider = {{ kind = {kind} }}\n",
        server = server_table(files),
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

/// The `[server]` table of a configuration file over `files`, and the blank line after it:
/// the files, a first boot, the listeners (none bound: the server runs in process), the
/// address the launched machines are told, a lease's longest wall time and usage off.
fn server_table(files: &Files<'_>) -> String {
    format!(
        "[server]\n\
         state_file = {state}\n\
         boot = \"initialise\"\n\
         client_token_file = {token}\n\
         store = {store}\n\
         allow_sqlite_store = {allow_sqlite}\n\
         listen = \"127.0.0.1:0\"\n\
         job_listen = \"127.0.0.1:0\"\n\
         agent_listen = \"127.0.0.2:0\"\n\
         advertise_url = {advertise}\n\
         max_lease_wall = \"2h\"\n\
         usage_off = true\n\
         \n",
        state = quoted(&files.state.display().to_string()),
        token = quoted(&files.token.display().to_string()),
        store = quoted(&files.store.display().to_string()),
        allow_sqlite = files.allow_sqlite,
        advertise = quoted(ADVERTISE_URL),
    )
}

/// The files the configuration file of a server of [`crate::ServerSpec::ordered`] names
/// besides those of [`Files`].
pub(crate) struct Ordered<'a> {
    /// The pool's name.
    pub(crate) pool: &'a str,
    /// How many hosts of the `static` machine provider (registered with the shared join
    /// token) the pool admits.
    pub(crate) hosts: usize,
    /// The file that holds the pool's shared join token (`join_token_file`).
    pub(crate) joins: &'a Path,
}

/// The name of the `static` machine provider of a server of [`crate::ServerSpec::ordered`].
pub const STATIC_PROVIDER: &str = "registered";

/// The class of the pool of a server of [`crate::ServerSpec::ordered`]: one of the
/// server's own naming, which books the share each job asks for (a class the server does
/// not ship), so a job that asks for part of a host is placed on registered hosts and
/// launched machines alike. The `whole-node` class of a pool of [`text`] keeps no host for
/// such work.
pub const ORDERED_CLASS: &str = "shared-host";

/// The name of the platform of a server of [`crate::ServerSpec::ordered`].
pub const PLATFORM: &str = "cell";

/// The configuration file of a server of [`crate::ServerSpec::ordered`]: the `[server]`
/// table of [`text`], a `static` machine provider ([`STATIC_PROVIDER`]), one machine
/// provider of the `sim-cloud` kind for each of `clouds` (named, and naming its cloud, by
/// its pool's name), a `default` platform ([`PLATFORM`]) listing them first to last, the
/// static provider first, and the pool `ordered.pool` over that platform, with a
/// `provider_limits` table for each cloud: its `max_machines` (the cloud's
/// `max_instances`), `quota_backoff` and `acquire_retry`.
///
/// The pool is as [`text`]'s but of class [`ORDERED_CLASS`], with `max_instances` the static hosts and every cloud's
/// share together (at most 255, which the server refuses above), its spending caps those of
/// so many machines of the dearest offer of any cloud, and the shared join token of
/// `ordered.joins`, which the static hosts say hello with.
#[must_use]
pub(crate) fn ordered_text(clouds: &[Cloud], files: &Files<'_>, ordered: &Ordered<'_>) -> String {
    let pools = clouds.iter().map(Cloud::pool);
    let shares: u64 = pools.clone().map(ElasticPool::max_instances).sum();
    let hosts = u64::try_from(ordered.hosts).unwrap_or(u64::MAX);
    let max = shares.saturating_add(hosts);
    let offers = pools.clone().flat_map(|pool| pool.offers().iter());
    let dearest = offers.map(|offer| offer.cents_per_hour).max();
    let hour = dearest.unwrap_or(0).saturating_mul(max).max(1);
    let day = hour.saturating_mul(24);
    let names: Vec<String> = pools.clone().map(|pool| quoted(pool.name())).collect();
    let providers = pools.clone().map(|pool| {
        let name = quoted(pool.name());
        format!(
            "[[machine_provider]]\n\
             name = {name}\n\
             kind = {kind}\n\
             {key} = {name}\n\
             \n",
            kind = quoted(KIND),
            key = CLOUD_KEY,
        )
    });
    let limits = pools.map(|pool| {
        format!(
            "\n[pool.provider_limits.{name}]\n\
             max_machines = {max}\n\
             quota_backoff = {backoff}\n\
             acquire_retry = {retry}\n",
            name = quoted(pool.name()),
            max = pool.max_instances(),
            backoff = millis(pool.quota_backoff()),
            retry = millis(pool.acquire_retry()),
        )
    });
    format!(
        "{server}\
         [[machine_provider]]\n\
         name = {static_name}\n\
         kind = \"static\"\n\
         \n\
         {providers}\
         [[platform]]\n\
         name = {platform}\n\
         kind = \"default\"\n\
         machine_providers = [{static_name}, {names}]\n\
         \n\
         [[pool]]\n\
         name = {pool}\n\
         class = {class}\n\
         platforms = [{platform}]\n\
         max_instances = {max}\n\
         max_spend_per_hour = \"{hour}\"\n\
         max_spend_per_day = \"{day}\"\n\
         idle = {idle}\n\
         min_host_lifetime = {lifetime}\n\
         max_age = {age}\n\
         launch = \"live\"\n\
         max_queue_wait = \"2h\"\n\
         join_token_file = {joins}\n\
         agent = {{ x86_64 = {{ url = {url}, sha256 = {sha} }} }}\n\
         {limits}",
        server = server_table(files),
        static_name = quoted(STATIC_PROVIDER),
        providers = providers.collect::<String>(),
        platform = quoted(PLATFORM),
        names = names.join(", "),
        pool = quoted(ordered.pool),
        class = quoted(ORDERED_CLASS),
        hour = dollars(hour),
        day = dollars(day),
        idle = millis(
            clouds
                .first()
                .map_or(DEFAULT_IDLE, |cloud| cloud.pool().idle_time())
        ),
        lifetime = millis(MIN_HOST_LIFETIME),
        age = millis(MAX_AGE),
        joins = quoted(&ordered.joins.display().to_string()),
        url = quoted(AGENT_URL),
        sha = quoted(AGENT_SHA256),
        limits = limits.collect::<String>(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::elastic::{ElasticPool, OfferSpec};
    use crate::server::Shape;

    /// The floor the built kind gives over a cloud of `shape`.
    fn floor_of(shape: &Shape) -> Floor {
        let offers = vec![OfferSpec::new("sim_1x_a100", "zone-a", 120)];
        let cloud = Cloud::new(&ElasticPool::ec2_like("burst", offers, 2), shape);
        elastic(cloud).floor
    }

    /// Asserts that the file of an ordered server declares the static provider and one
    /// machine provider of the cell's kind for each cloud, naming its cloud, lists them
    /// first to last in its platform, the static one first, gives each cloud its
    /// `provider_limits` (share, `quota_backoff`, `acquire_retry`), admits the static hosts
    /// and every share in `max_instances`, and names the shared join token's file. Catches a
    /// provider left out or listed out of order, a share or a failover time not written, a
    /// pool too small for its static hosts, and static hosts with no token to say hello with.
    #[test]
    fn the_ordered_file_lists_the_providers_first_to_last_with_their_limits() {
        let offers = vec![OfferSpec::new("sim_cpu", "zone-a", 120)];
        let first = ElasticPool::ec2_like("ec2", offers.clone(), 3);
        let second = ElasticPool::lambda_like("lambda", offers, 4)
            .failover(Duration::from_secs(90), Duration::from_secs(30));
        let first = Cloud::ordered(&first, &Shape::cpu_host(), &[]);
        let second = Cloud::ordered(&second, &Shape::cpu_host(), std::slice::from_ref(&first));
        let files = Files {
            state: Path::new("/scratch/leases.toml"),
            token: Path::new("/scratch/token"),
            store: Path::new("/scratch/store"),
            allow_sqlite: true,
        };
        let ordered = Ordered {
            pool: "burst",
            hosts: 5,
            joins: Path::new("/scratch/agent-tokens"),
        };
        let text = ordered_text(&[first, second], &files, &ordered);
        let wanted = [
            "name = \"registered\"\nkind = \"static\"\n",
            "name = \"ec2\"\nkind = \"sim-cloud\"\ncloud = \"ec2\"\n",
            "name = \"lambda\"\nkind = \"sim-cloud\"\ncloud = \"lambda\"\n",
            "machine_providers = [\"registered\", \"ec2\", \"lambda\"]\n",
            "class = \"shared-host\"\nplatforms = [\"cell\"]\nmax_instances = 12\n",
            "join_token_file = \"/scratch/agent-tokens\"\n",
            "[pool.provider_limits.\"ec2\"]\nmax_machines = 3\nquota_backoff = \"3600000ms\"\nacquire_retry = \"60000ms\"\n",
            "[pool.provider_limits.\"lambda\"]\nmax_machines = 4\nquota_backoff = \"90000ms\"\nacquire_retry = \"30000ms\"\n",
        ];
        for line in wanted {
            assert!(text.contains(line), "{line:?} is not in:\n{text}");
        }
        assert!(text.parse::<toml::Table>().is_ok(), "{text}");
    }

    /// Asserts that the floor of the pool is the GPUs of the cloud's shape, of the shape's
    /// class when it has GPUs, and none of any class when it has none.
    #[test]
    fn the_floor_is_the_gpus_of_the_shape_in_its_class() {
        let with_gpus = floor_of(&Shape::gpu_host());
        assert_eq!(with_gpus.gpus, 1);
        assert_eq!(with_gpus.gpu_class, Model::parse("nvidia-a100").ok());
        assert!(with_gpus.gpu_class.is_some());
        let without = floor_of(&Shape::cpu_host());
        assert_eq!((without.gpus, without.gpu_class), (0, None));
    }
}
