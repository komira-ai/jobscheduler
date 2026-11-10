//! A registered host's machine provider and its rank, from its binding alone, and the order of
//! the Default platform a configuration file gives. The files are written in a directory of
//! the test's own; nothing is reached over a network.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use js_agent_wire::InstanceIdentity;
use js_elastic_fake::{Billing, ElasticFake, FakeClock};
use js_registry::{Binding, Credential};
use js_testkit::dir::TestDir;
use js_types::{ExecMode, ManualClock, Model, ProviderId, Timestamp};

use super::{STATIC, order_of, rank, static_then};
use crate::clock::SharedClock;
use crate::config::Config;
use crate::file::Resolved;
use crate::kinds::{BuildCtx, Built, ElasticBuilt, Floor, KindError, Kinds, OwnerOnlyFiles};

fn id(name: &str) -> ProviderId {
    ProviderId::new(name).unwrap()
}

/// A binding of the pool's join token, naming the instance `instance` of `provider` when
/// given.
fn bound(instance: Option<(&str, &str)>) -> Binding {
    let instance = instance.map(|(provider, instance_id)| InstanceIdentity {
        provider: provider.to_owned(),
        instance_id: instance_id.to_owned(),
    });
    Binding::new(Credential::of_token("join-token-0123456789"), instance)
}

/// The position `binding` ranks at in `order`, and the provider it names.
fn at(order: &[ProviderId], binding: Option<&Binding>) -> Option<(u32, String)> {
    let rank = rank(order, binding)?;
    Some((rank.position(), rank.provider().to_string()))
}

/// Asserts that a host whose binding names no instance (or that has no binding) ranks at the
/// position the order gives `static`, that a host whose binding names an instance ranks at the
/// position of the pool's launching provider, in either order and whatever provider text the
/// instance carries, and that a platform listing no provider of a host's kind gives it no
/// rank. It catches a wrong index, a default rank for a host the platform does not list, and
/// a provider read from the instance's text, which the mint route takes as it is given.
#[test]
fn rank_of_a_static_host_and_a_bound_elastic_host() {
    let joined = bound(None);
    let launched = bound(Some(("cloud", "cloud-7")));
    let minted = bound(Some(("lambdalabs", "i-1")));
    let order = [STATIC, id("cloud")];
    assert_eq!(at(&order, None), Some((0, "static".to_owned())));
    assert_eq!(at(&order, Some(&joined)), Some((0, "static".to_owned())));
    assert_eq!(at(&order, Some(&launched)), Some((1, "cloud".to_owned())));
    assert_eq!(at(&order, Some(&minted)), Some((1, "cloud".to_owned())));
    let reversed = [id("cloud"), STATIC];
    assert_eq!(at(&reversed, Some(&joined)), Some((1, "static".to_owned())));
    assert_eq!(
        at(&reversed, Some(&launched)),
        Some((0, "cloud".to_owned()))
    );

    assert_eq!(at(&[STATIC], Some(&launched)), None);
    assert_eq!(at(&[id("cloud")], Some(&joined)), None);
    assert_eq!(at(&[id("cloud")], None), None);
}

/// Asserts that a configuration that names no platform keeps `static` first and the pool's
/// launched hosts after. It catches a default order that puts the launched hosts first.
#[test]
fn without_a_platform_static_comes_before_the_pools_launched_hosts() {
    assert_eq!(static_then(&id("burst")), [STATIC, id("burst")]);
}

/// The test's directory.
fn scratch(name: &str) -> TestDir {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/provider-rank");
    TestDir::under(root, name)
}

/// Writes `text` to `name` in `dir`, readable by its owner alone.
fn write(dir: &Path, name: &str, text: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, text).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    path
}

/// The `fake-cloud` kind: an elastic fake cloud with no offer, whose floor is one a100.
fn fake_cloud(_: &toml::Table, _: &BuildCtx<'_>) -> Result<Built, KindError> {
    let cloud = ElasticFake::new(FakeClock::new(0), Billing::Lambda, Vec::new());
    Ok(Built::Elastic(ElasticBuilt {
        idle_costs: false,
        provider: Box::new(cloud),
        floor: Floor {
            gpus: 1,
            gpu_class: Some(Model::parse("nvidia-a100").unwrap()),
        },
        exec: vec![ExecMode::Container],
    }))
}

/// The machine providers, platforms and pools of a file of the server, `tables` and the
/// elastic pool `gpu` with `pool` keys.
fn resolved(name: &str, tables: &str, pool: &str) -> Resolved {
    let dir = scratch(name);
    let token = write(dir.path(), "client-token", &"t".repeat(40));
    let key = write(dir.path(), "api-key", "a-fake-api-key");
    let quoted = |path: &Path| format!("{:?}", path.display().to_string());
    let text = format!(
        "[server]\nstate_file = {state}\nboot = \"initialise\"\nclient_token_file = {token}\n\
         max_lease_wall = \"2h\"\nlisten = \"127.0.0.1:0\"\nagent_listen = \"127.0.0.2:0\"\n\n\
         {tables}\n\
         [[pool]]\nname = \"gpu\"\nclass = \"whole-node\"\nmax_instances = 4\n\
         max_spend_per_hour = \"12.00\"\nmax_spend_per_day = \"96.00\"\nidle = \"10m\"\n\
         max_age = \"2h\"\nmin_host_lifetime = \"5m\"\n{pool}\n",
        state = quoted(&dir.path().join("state")),
        token = quoted(&token),
        tables = tables.replace("KEY", &quoted(&key)),
        pool = pool.replace("KEY", &quoted(&key)),
    );
    let path = write(dir.path(), "js.toml", &text);
    let kinds = Kinds::standard().with("fake-cloud", fake_cloud);
    let clock = SharedClock::new(ManualClock::new(Timestamp::from_millis(0)));
    Config::resolve_file(&path, &kinds, &OwnerOnlyFiles, clock).unwrap()
}

/// Asserts that the order of a pool's Default platform is its `machine_providers` as the
/// providers bindings name: a `static` machine provider as `static`, the launching one as the
/// pool, whether the file declares the tables or names the provider on the pool; and that a
/// pool the file does not resolve has no order of its own (it takes the default). It catches
/// an order taken from anywhere but the platform, and a launching provider named by its table
/// name, which no binding carries.
#[test]
fn the_order_is_the_default_platforms_machine_providers() {
    let tables = "[[machine_provider]]\nname = \"rack\"\nkind = \"static\"\n\n\
                  [[machine_provider]]\nname = \"cloud\"\nkind = \"fake-cloud\"\n\
                  api_key_file = KEY\n\n\
                  [[platform]]\nname = \"ours\"\nkind = \"default\"\n\
                  machine_providers = [\"rack\", \"cloud\"]\n";
    let explicit = resolved("order-explicit", tables, "platforms = [\"ours\"]");
    let shorthand = resolved(
        "order-shorthand",
        "",
        "provider = { kind = \"fake-cloud\", api_key_file = KEY }",
    );
    for resolved in [&explicit, &shorthand] {
        assert_eq!(
            order_of(resolved, &id("gpu")),
            Some(vec![STATIC, id("gpu")])
        );
        assert_eq!(order_of(resolved, &id("other")), None);
    }
}
