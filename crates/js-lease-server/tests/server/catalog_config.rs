//! The `quota_backoff`, `acquire_retry`, `catalog_refresh` and `offer_cooldown` keys of a
//! pool's `provider_limits`: their defaults, the values a file gives, and the refusal of a
//! zero, read through [`Config::resolve_file`]; and the limits the scaler's pick of an offer
//! is given.

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
    let arch = BTreeMap::from([("arch".to_owned(), "x86_64".to_owned())]);
    let shape = Shape::new(resources, arch);
    let name = ProviderId::new("gpu").unwrap();
    Registry::new(
        PoolConfig::new(name, ResourceClass::whole_node(), shape, 4),
        clock(),
        std::sync::Arc::new(js_sched_placer::place::SchedPlacer::default()),
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
    assert_eq!(settings.provider, plan.providers().next().unwrap().id());
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

/// The `quota_backoff` and `acquire_retry` the file gives the provider `lambda` of the pool
/// `gpu`, as the limits read them and as the scaler settings of the plan carry them.
fn failover(scratch: &Scratch, limits: &str) -> [(Duration, Duration); 2] {
    let resolved = match resolve(scratch, &limited(scratch, limits)) {
        Ok(resolved) => resolved,
        Err(err) => panic!("the file should resolve: {err}"),
    };
    let read = &resolved.pool("gpu").unwrap().provider_limits["lambda"];
    let config = load(scratch, &limited(scratch, limits)).unwrap();
    let settings = config.elastic_pool().unwrap().scaler_settings(&registry());
    [
        (read.quota_backoff, read.acquire_retry),
        (settings.quota_backoff, settings.acquire_retry),
    ]
}

/// Asserts that a provider table that gives neither `quota_backoff` nor `acquire_retry`
/// resolves them to one hour and sixty seconds, both in the limits and in the scaler
/// settings of the plan. Catches a default that is not 1h and 60s, and a value that is read
/// but never reaches the scaler.
#[test]
fn acquire_failover_keys_default_to_one_hour_and_sixty_seconds() {
    let scratch = Scratch::new("catalog_config_failover_defaults");
    let expected = (Duration::from_secs(3600), Duration::from_secs(60));
    let [read, settings] = failover(&scratch, "{ lambda = { max_machines = 2 } }");
    assert_eq!(read, expected);
    assert_eq!(settings, expected);
}

/// Asserts that `quota_backoff = "2h"` with `acquire_retry = "90s"` arrive as those values
/// in the limits and in the scaler settings, and that each given alone leaves the other at
/// its default. Catches a key read into the other's field, and a key dropped on the way to
/// the scaler.
#[test]
fn acquire_failover_keys_are_read_from_the_config_file() {
    let scratch = Scratch::new("catalog_config_failover_values");
    let hour = Duration::from_secs(3600);
    let minute = Duration::from_secs(60);
    let both = "{ lambda = { quota_backoff = \"2h\", acquire_retry = \"90s\" } }";
    let expected = (Duration::from_secs(7200), Duration::from_secs(90));
    assert_eq!(failover(&scratch, both), [expected, expected]);
    let backoff = "{ lambda = { quota_backoff = \"2h\" } }";
    let expected = (Duration::from_secs(7200), minute);
    assert_eq!(failover(&scratch, backoff), [expected, expected]);
    let retry = "{ lambda = { acquire_retry = \"90s\" } }";
    let expected = (hour, Duration::from_secs(90));
    assert_eq!(failover(&scratch, retry), [expected, expected]);
}

/// Asserts that a `quota_backoff` of zero, written `0s` or `0`, is refused naming the pool,
/// the provider and the key, with the full message. Catches a zero back-off read as a quota
/// refusal that passes over nothing.
#[test]
fn zero_quota_backoff_is_a_startup_error_naming_the_key() {
    let scratch = Scratch::new("catalog_config_zero_backoff");
    for zero in ["0s", "0"] {
        let limits = format!("{{ lambda = {{ quota_backoff = {} }} }}", quote(zero));
        assert_eq!(
            refused(&scratch, &limited(&scratch, &limits)),
            "pool \"gpu\": provider_limits.lambda.quota_backoff: must be a positive duration such as 600s, 10m or 1h"
        );
    }
}

/// Asserts that an `acquire_retry` of zero, written `0s` or `0`, is refused naming the pool,
/// the provider and the key, with the full message. Catches a zero retry read as a request
/// that asks the provider again at every tick.
#[test]
fn zero_acquire_retry_is_a_startup_error_naming_the_key() {
    let scratch = Scratch::new("catalog_config_zero_retry");
    for zero in ["0s", "0"] {
        let limits = format!("{{ lambda = {{ acquire_retry = {} }} }}", quote(zero));
        assert_eq!(
            refused(&scratch, &limited(&scratch, &limits)),
            "pool \"gpu\": provider_limits.lambda.acquire_retry: must be a positive duration such as 600s, 10m or 1h"
        );
    }
}

/// Asserts that an `acquire_retry` that is not a duration (`soon`) is refused with the full
/// message naming the pool, the provider and the key. Catches an unparsable value that is
/// read as the default.
#[test]
fn unparsable_acquire_retry_names_the_provider() {
    let scratch = Scratch::new("catalog_config_bad_retry");
    let limits = "{ lambda = { acquire_retry = \"soon\" } }";
    assert_eq!(
        refused(&scratch, &limited(&scratch, limits)),
        "pool \"gpu\": provider_limits.lambda.acquire_retry: must be a positive duration such as 600s, 10m or 1h"
    );
}

/// The `size_tolerance_percent` the file gives the provider `lambda` of the pool `gpu`, as
/// the limits read it, and the tolerance and the offer cool-down the scaler settings of the
/// plan carry.
fn tolerance(scratch: &Scratch, limits: &str) -> (u8, u8, Duration) {
    let resolved = match resolve(scratch, &limited(scratch, limits)) {
        Ok(resolved) => resolved,
        Err(err) => panic!("the file should resolve: {err}"),
    };
    let read = resolved.pool("gpu").unwrap().provider_limits["lambda"].size_tolerance_percent;
    let config = load(scratch, &limited(scratch, limits)).unwrap();
    let settings = config.elastic_pool().unwrap().scaler_settings(&registry());
    (
        read,
        settings.size_tolerance_percent,
        settings.offer_cooldown,
    )
}

/// Asserts `size_tolerance_percent`: 10 when the table does not give it, the value it gives
/// from 0 to 50 (both edges included) in the limits and in the scaler settings; and a value
/// above 50, a negative one, a fraction or a text is refused naming the pool, the provider and
/// the key, with the full message. Asserts too that the scaler settings carry the
/// `offer_cooldown` of the provider's table (one hour when not given), which is where the
/// length of an offer's cool-down after a mismatched hello comes from. Catches a bad value
/// accepted, a default that is not 10, and a cool-down length read from anywhere else.
#[test]
fn config_key_is_validated() {
    let scratch = Scratch::new("catalog_config_size_tolerance");
    let hour = Duration::from_secs(3600);
    assert_eq!(
        tolerance(&scratch, "{ lambda = { max_machines = 2 } }"),
        (10, 10, hour)
    );
    for given in [0, 25, 50] {
        let limits = format!("{{ lambda = {{ size_tolerance_percent = {given} }} }}");
        assert_eq!(tolerance(&scratch, &limits), (given, given, hour));
    }
    let both = "{ lambda = { size_tolerance_percent = 5, offer_cooldown = \"20m\" } }";
    assert_eq!(
        tolerance(&scratch, both),
        (5, 5, Duration::from_secs(20 * 60))
    );
    let head = "pool \"gpu\": provider_limits.lambda.size_tolerance_percent: ";
    for (value, reason) in [
        ("51", "must be from 0 to 50"),
        ("1000", "must be from 0 to 50"),
        ("-1", "must not be negative"),
        ("10.5", "must be an integer"),
        ("\"10\"", "must be an integer"),
    ] {
        let limits = format!("{{ lambda = {{ size_tolerance_percent = {value} }} }}");
        assert_eq!(
            refused(&scratch, &limited(&scratch, &limits)),
            format!("{head}{reason}"),
            "{value}"
        );
    }
}
