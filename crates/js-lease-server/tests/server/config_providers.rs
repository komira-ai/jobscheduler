//! A pool whose platforms reach more than one launching machine provider (design 5.6,
//! step 5): the file is read with each provider built from its own table, in the order the
//! platforms list them, each with its own `provider_limits`, and the plan's scaler settings
//! carry every provider in that order. Read through [`Config::from_config_file`].

use std::collections::BTreeMap;
use std::time::Duration;

use js_lease_server::{
    BuildCtx, Built, Config, ElasticBuilt, Floor, KindError, Kinds, OwnerOnlyFiles, StartupError,
};
use js_registry::{PoolConfig, Registry};
use js_types::{ExecMode, Model, ProviderId, ResourceClass, Resources, Shape};

use crate::config_support::{
    API_KEY, NullElastic, PoolText, clock, file, kinds, quote, quoted, server_with_agents,
};
use crate::support::Scratch;

/// A kind of hosts that run containers, with two GPUs of class `nvidia-a10`, under the
/// provider id `other`: a second launching provider beside `lambdalabs`.
fn other_cloud(_table: &toml::Table, _ctx: &BuildCtx<'_>) -> Result<Built, KindError> {
    Ok(Built::Elastic(ElasticBuilt {
        idle_costs: false,
        provider: Box::new(NullElastic("other")),
        floor: Floor {
            gpus: 2,
            gpu_class: Some(Model::parse("nvidia-a10").unwrap()),
        },
        exec: vec![ExecMode::Container],
    }))
}

/// A kind of hosts with no GPU that cost money idle, under the provider id `idle`.
fn idle_cloud(_table: &toml::Table, _ctx: &BuildCtx<'_>) -> Result<Built, KindError> {
    Ok(Built::Elastic(ElasticBuilt {
        idle_costs: true,
        provider: Box::new(NullElastic("idle")),
        floor: Floor {
            gpus: 0,
            gpu_class: None,
        },
        exec: vec![ExecMode::Container],
    }))
}

/// The kinds of the tests, `other-cloud` and `idle-cloud`.
fn walk_kinds() -> Kinds {
    kinds()
        .with("other-cloud", other_cloud)
        .with("idle-cloud", idle_cloud)
}

/// Checks `text` as the file `js.toml` of `scratch`, with [`walk_kinds`].
fn load(scratch: &Scratch, text: &str) -> Result<Config, StartupError> {
    let path = scratch.write("js.toml", text, 0o644);
    Config::from_config_file(&path, &walk_kinds(), &OwnerOnlyFiles, clock())
}

/// The message with which `text` is refused.
fn refusal(scratch: &Scratch, text: &str) -> String {
    match load(scratch, text) {
        Ok(_) => panic!("the file should be refused"),
        Err(err) => err.to_string(),
    }
}

/// The registry of a pool `gpu` of four machines, for the plan's scaler settings.
fn registry() -> Registry {
    let resources = Resources::new(1000, 1 << 30, 1, Some("nvidia-a10".to_owned())).unwrap();
    let shape = Shape::new(resources, BTreeMap::new());
    let name = ProviderId::new("gpu").unwrap();
    Registry::new(
        PoolConfig::new(name, ResourceClass::whole_node(), shape, 4),
        clock(),
        std::sync::Arc::new(js_sched_placer::place::SchedPlacer::default()),
    )
}

/// A machine provider `name` of `kind`, with a key file when the kind reads one.
fn provider(scratch: &Scratch, name: &str, kind: &str) -> String {
    let key = scratch.write(&format!("key-{name}"), API_KEY, 0o600);
    let key = match kind {
        "lambdalabs" => format!("api_key_file = {}\n", quoted(&key)),
        _ => String::new(),
    };
    format!(
        "[[machine_provider]]\nname = {}\nkind = {}\n{key}",
        quote(name),
        quote(kind)
    )
}

/// A file whose `rack` (static) and the launching machine providers `launching` (each a
/// name and a kind) are listed by the platform `ours` in the order `order`, with the
/// elastic pool `gpu` on `ours` given `limits` as its `provider_limits` and `extra` keys.
fn walk(
    scratch: &Scratch,
    launching: &[(&str, &str)],
    order: &[&str],
    (limits, extra): (&str, &[(&str, &str)]),
) -> String {
    let mut tables = "[[machine_provider]]\nname = \"rack\"\nkind = \"static\"\n".to_owned();
    for (name, kind) in launching {
        tables.push_str(&provider(scratch, name, kind));
    }
    let listed: Vec<String> = order.iter().map(|name| quote(name)).collect();
    tables.push_str(&format!(
        "[[platform]]\nname = \"ours\"\nkind = \"default\"\nmachine_providers = [{}]\n",
        listed.join(", ")
    ));
    let mut pool = PoolText::elastic(scratch)
        .without("provider")
        .set("platforms", "[\"ours\"]")
        .set("provider_limits", limits);
    for (key, value) in extra {
        pool = pool.set(key, value);
    }
    file(
        &format!("{}\n{tables}", server_with_agents(scratch)),
        &[&pool],
    )
}

/// The ids of the providers of the elastic pool of `config`, in order.
fn ids(config: &Config) -> Vec<String> {
    let plan = config.elastic_pool().unwrap();
    plan.providers().map(|each| each.id().to_string()).collect()
}

/// Asserts that a pool whose platform lists a static provider and then two launching ones
/// is read with both launching providers, in the platform's order, and that listing them the
/// other way round reverses them; that the scaler settings carry the first with its own
/// `provider_limits` (share 1, its allowed types) and the second after it with its own
/// (share 2, its price), and ask again after the soonest `acquire_retry` of the two; and
/// that the pool's floor is the fewest GPUs of the class both providers' hosts share (one
/// `nvidia-a10`, of one and two). Catches a pool that keeps only one launching provider,
/// one that reorders them, limits read for the first provider applied to both, and a floor
/// above what a host of either provider brings.
#[test]
fn a_pool_reaches_a_static_provider_and_two_launching_ones_in_order() {
    let scratch = Scratch::new("providers_walk");
    let launching = [("lambda", "lambdalabs"), ("burst", "other-cloud")];
    let limits = "{ lambda = { max_machines = 1, allowed_instance_types = [\"gpu_1x_a10\"], acquire_retry = \"90s\" }, burst = { max_machines = 2, max_price_cents_per_hour = 300, acquire_retry = \"30s\" } }";
    let order = ["rack", "lambda", "burst"];
    let config = load(&scratch, &walk(&scratch, &launching, &order, (limits, &[]))).unwrap();
    assert_eq!(ids(&config), ["null", "other"]);
    let plan = config.elastic_pool().unwrap();
    assert_eq!((plan.floor.gpus, plan.kind.as_str()), (1, "lambdalabs"));
    let settings = plan.scaler_settings(&registry());
    assert_eq!(settings.provider.as_str(), "null");
    assert_eq!(settings.provider_share, Some(1));
    let types = Some(vec!["gpu_1x_a10".to_owned()]);
    assert_eq!(settings.allowed_instance_types, types);
    assert_eq!(settings.max_price_cents_per_hour, None);
    assert_eq!(settings.acquire_retry, Duration::from_secs(30));
    let [burst] = settings.later.as_slice() else {
        panic!("one provider after the first: {:?}", settings.later);
    };
    assert_eq!(
        (burst.provider.as_str(), burst.kind.as_str()),
        ("other", "other-cloud")
    );
    assert_eq!(burst.share, Some(2));
    assert_eq!(burst.allowed_instance_types, None);
    assert_eq!(burst.max_price_cents_per_hour, Some(300));

    let reversed = ["burst", "rack", "lambda"];
    let config = load(
        &scratch,
        &walk(&scratch, &launching, &reversed, (limits, &[])),
    )
    .unwrap();
    assert_eq!(ids(&config), ["other", "null"]);
    let settings = config.elastic_pool().unwrap().scaler_settings(&registry());
    assert_eq!(settings.provider_share, Some(2));
    assert_eq!(settings.later[0].share, Some(1));
}

/// Asserts that a pool whose providers' hosts bring GPUs of no common class (one
/// `nvidia-a10` beside none) has a floor of no GPU, so the registry admits a host of either.
/// Catches a floor taken from the first provider alone.
#[test]
fn a_pool_of_providers_with_no_common_gpu_class_has_no_gpu_floor() {
    let scratch = Scratch::new("providers_floor");
    let launching = [("lambda", "lambdalabs"), ("cpu", "any-mode")];
    let order = ["rack", "lambda", "cpu"];
    let config = load(&scratch, &walk(&scratch, &launching, &order, ("{}", &[]))).unwrap();
    let plan = config.elastic_pool().unwrap();
    assert_eq!((plan.floor.gpus, plan.floor.gpu_class.clone()), (0, None));
}

/// Asserts the exact refusals of a pool's providers: two machine providers of one kind,
/// which the scheduler cannot tell apart, are refused naming the second; a pool whose
/// execution mode one provider cannot launch is refused naming that provider's kind, the
/// second one here; and a pool whose platforms reach only `static` is refused as before,
/// pointing at `kind = "joined"`. Catches two providers of one id that share a catalog and
/// an acquire memory, a mode checked against the first provider alone, and a pool with no
/// provider to launch with that starts.
#[test]
fn the_refusals_of_a_pools_providers_are_exact() {
    let scratch = Scratch::new("providers_refused");
    let twins = [("lambda-a", "lambdalabs"), ("lambda-b", "lambdalabs")];
    let order = ["rack", "lambda-a", "lambda-b"];
    assert_eq!(
        refusal(&scratch, &walk(&scratch, &twins, &order, ("{}", &[]))),
        "machine_provider \"lambda-b\": kind: its provider is \"null\", as is that of an earlier machine provider of the pool; a pool reaches each provider once"
    );

    let modes = [("cpu", "any-mode"), ("lambda", "lambdalabs")];
    let order = ["rack", "cpu", "lambda"];
    let sandbox = [("exec", "\"sandbox-exec\"")];
    assert_eq!(
        refusal(&scratch, &walk(&scratch, &modes, &order, ("{}", &sandbox))),
        "pool \"gpu\": exec: sandbox-exec hosts cannot be launched by provider kind \"lambdalabs\""
    );

    let none: [(&str, &str); 0] = [];
    let order = ["rack"];
    assert_eq!(
        refusal(&scratch, &walk(&scratch, &none, &order, ("{}", &[]))),
        "pool \"gpu\": platforms: a pool's platforms reach no machine provider that launches; a pool of registered hosts alone is kind = \"joined\""
    );
}

/// Asserts that a pool is held to the lifetime keys of a pool with accelerators when any of
/// its providers' hosts cost money idle, a later one included: with `cpu` (no GPU, free when
/// idle) first and `idle` (costly idle) after it, a pool without `max_age` is refused, and
/// with `cpu` alone it is read. Catches a pool whose costly later provider launches machines
/// no age bounds.
#[test]
fn a_later_provider_whose_hosts_cost_money_idle_needs_the_lifetime_keys() {
    let scratch = Scratch::new("providers_idle");
    let both = [("cpu", "any-mode"), ("idle", "idle-cloud")];
    let order = ["rack", "cpu", "idle"];
    let text = walk(&scratch, &both, &order, ("{}", &[])).replace("max_age = \"2h\"\n", "");
    let said = refusal(&scratch, &text);
    assert_eq!(
        said,
        "pool \"gpu\": max_age: required of a pool with accelerators: an idle host costs money by the hour, so the file names how long it may live"
    );
    let alone = [("cpu", "any-mode")];
    let order = ["rack", "cpu"];
    let text = walk(&scratch, &alone, &order, ("{}", &[])).replace("max_age = \"2h\"\n", "");
    assert!(load(&scratch, &text).is_ok());
}
