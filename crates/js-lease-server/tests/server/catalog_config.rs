//! The `catalog_refresh` and `offer_cooldown` keys of a pool's `provider_limits`: their
//! defaults, the values a file gives, and the refusal of a zero, read through
//! [`Config::resolve_file`]; and the limits the scaler's pick of an offer is given.

use std::collections::BTreeMap;
use std::time::Duration;

use js_lease_server::{Config, OwnerOnlyFiles, Resolved, StartupError};
use js_registry::{PoolConfig, Registry};
use js_types::{ProviderId, ResourceClass, Resources, Shape};

use crate::config_support::{
    API_KEY, PoolText, clock, file, kinds, load, quote, quoted, server_with_agents,
};
use crate::support::Scratch;

/// The file of the pool `gpu` on the platform `ours` (a `static` and a `lambdalabs` machine
/// provider), with `limits` as its `provider_limits`.
fn limited(scratch: &Scratch, limits: &str) -> String {
    let key = scratch.write("key-lambda", API_KEY, 0o600);
    let tables = format!(
        "[[machine_provider]]\nname = \"rack\"\nkind = \"static\"\n\n\
         [[machine_provider]]\nname = \"lambda\"\nkind = \"lambdalabs\"\napi_key_file = {}\n\n\
         [[platform]]\nname = \"ours\"\nkind = \"default\"\nmachine_providers = [\"rack\", \"lambda\"]\n",
        quoted(&key)
    );
    let pool = PoolText::elastic(scratch)
        .without("provider")
        .without("max_instances")
        .set("platforms", &format!("[{}]", quote("ours")))
        .set("max_machines", "4")
        .set("provider_limits", limits);
    let head = format!("{}\n{tables}", server_with_agents(scratch));
    file(&head, &[&pool])
}

fn resolve(scratch: &Scratch, text: &str) -> Result<Resolved, StartupError> {
    let path = scratch.write("js.toml", text, 0o644);
    Config::resolve_file(&path, &kinds(), &OwnerOnlyFiles, clock())
}

/// The message with which `text` is refused.
fn refused(scratch: &Scratch, text: &str) -> String {
    match resolve(scratch, text) {
        Ok(_) => panic!("the file should be refused"),
        Err(err) => err.to_string(),
    }
}

/// The refresh and the cool-down the file gives the provider `lambda` of the pool `gpu`.
fn read(scratch: &Scratch, limits: &str) -> (Duration, Duration) {
    let resolved = match resolve(scratch, &limited(scratch, limits)) {
        Ok(resolved) => resolved,
        Err(err) => panic!("the file should resolve: {err}"),
    };
    let limits = &resolved.pool("gpu").unwrap().provider_limits["lambda"];
    (limits.catalog_refresh, limits.offer_cooldown)
}

/// Asserts that a `catalog_refresh` of zero, written `0s` or `0`, is refused naming the
/// pool, the provider and the key, with the full message. Catches a zero refresh read as
/// a catalog fetched at every tick, or as no refresh at all.
#[test]
fn zero_catalog_refresh_is_refused_naming_the_key() {
    let scratch = Scratch::new("catalog_config_zero_refresh");
    for zero in ["0s", "0"] {
        let limits = format!("{{ lambda = {{ catalog_refresh = {} }} }}", quote(zero));
        assert_eq!(
            refused(&scratch, &limited(&scratch, &limits)),
            "pool \"gpu\": provider_limits.lambda.catalog_refresh: must be a positive duration such as 600s, 10m or 1h"
        );
    }
}

/// Asserts that an `offer_cooldown` of zero, written `0s` or `0`, is refused naming the
/// pool, the provider and the key, with the full message. Catches a zero cool-down read as
/// an offer that cools down for no time, which would make the cool-down a no-op unseen.
#[test]
fn zero_offer_cooldown_is_refused_naming_the_key() {
    let scratch = Scratch::new("catalog_config_zero_cooldown");
    for zero in ["0s", "0"] {
        let limits = format!("{{ lambda = {{ offer_cooldown = {} }} }}", quote(zero));
        assert_eq!(
            refused(&scratch, &limited(&scratch, &limits)),
            "pool \"gpu\": provider_limits.lambda.offer_cooldown: must be a positive duration such as 600s, 10m or 1h"
        );
    }
}

/// Asserts that a provider table that gives neither key resolves both to one hour, that a
/// table giving `30m` and `2h` resolves to those, and that a table giving one of them
/// leaves the other at its hour. Catches a default that is not an hour, and a key read
/// into the wrong field.
#[test]
fn catalog_refresh_and_offer_cooldown_default_to_one_hour() {
    let scratch = Scratch::new("catalog_config_defaults");
    let hour = Duration::from_secs(3600);
    assert_eq!(
        read(&scratch, "{ lambda = { max_machines = 2 } }"),
        (hour, hour)
    );
    let both = "{ lambda = { catalog_refresh = \"30m\", offer_cooldown = \"2h\" } }";
    assert_eq!(
        read(&scratch, both),
        (Duration::from_secs(1800), Duration::from_secs(7200))
    );
    let refresh = "{ lambda = { catalog_refresh = \"90s\" } }";
    assert_eq!(read(&scratch, refresh), (Duration::from_secs(90), hour));
    let cooldown = "{ lambda = { offer_cooldown = \"10m\" } }";
    assert_eq!(read(&scratch, cooldown), (hour, Duration::from_secs(600)));
}

/// A registry of the whole-node pool `gpu`, for the scaler settings a plan gives.
fn registry() -> Registry {
    let resources = Resources::new(1000, 1 << 30, 1, Some("nvidia-a10".to_owned())).unwrap();
    let shape = Shape::new(resources, BTreeMap::new());
    let name = ProviderId::new("gpu").unwrap();
    Registry::new(
        PoolConfig::new(name, ResourceClass::whole_node(), shape, 4),
        clock(),
    )
}

/// Asserts that the scaler settings a loaded plan gives carry what the pick of an offer
/// (design 5.6, pass 2) reads of the pool's launching provider: its id, its `max_machines`
/// share, its `allowed_instance_types` and its `max_price_cents_per_hour`; and that with no
/// such keys the share is the pool's `max_machines` and no type or price is limited.
/// Catches limits that admission reads and the scaler's pick does not.
#[test]
fn the_scaler_settings_carry_the_providers_limits() {
    let scratch = Scratch::new("catalog_config_scaler_limits");
    let limits = "{ lambda = { allowed_instance_types = [\"gpu_1x_a10\"], max_price_cents_per_hour = 150, max_machines = 3 } }";
    let config = load(&scratch, &limited(&scratch, limits)).unwrap();
    let plan = config.elastic_pool().unwrap();
    let settings = plan.scaler_settings(&registry());
    assert_eq!(settings.provider, plan.provider().id());
    assert_eq!(settings.provider_share, Some(3));
    assert_eq!(
        settings.allowed_instance_types,
        Some(vec!["gpu_1x_a10".to_owned()])
    );
    assert_eq!(settings.max_price_cents_per_hour, Some(150));

    let refresh = "{ lambda = { catalog_refresh = \"2h\" } }";
    let config = load(&scratch, &limited(&scratch, refresh)).unwrap();
    let settings = config.elastic_pool().unwrap().scaler_settings(&registry());
    assert_eq!(settings.provider_share, Some(4));
    assert_eq!(settings.allowed_instance_types, None);
    assert_eq!(settings.max_price_cents_per_hour, None);
}
