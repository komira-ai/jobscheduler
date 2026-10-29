//! The clocks and the configuration.

use std::num::NonZeroUsize;
use std::time::Duration;

use js_elastic::SchedulerId;
use js_provider_lambdalabs::{Clock, ConfigError, ElasticConfig, ManualClock, SystemClock};

fn config(key: &str) -> Result<ElasticConfig, ConfigError> {
    ElasticConfig::new(
        SchedulerId::new("s").unwrap(),
        key,
        NonZeroUsize::new(1).unwrap(),
    )
}

#[test]
fn the_system_clock_never_goes_back() {
    let clock = SystemClock::new();
    let first = clock.now();
    let second = SystemClock::default().now();
    assert!(clock.now() >= first);
    // A second clock has its own origin; both start near zero.
    assert!(second < Duration::from_secs(3600));
}

#[test]
fn the_manual_clock_moves_only_when_told() {
    let clock = ManualClock::new();
    assert_eq!(clock.now(), Duration::ZERO);
    assert_eq!(clock.now(), Duration::ZERO);
    clock.advance(Duration::from_millis(1500));
    clock.advance(Duration::from_secs(1));
    assert_eq!(clock.now(), Duration::from_millis(2500));
}

#[test]
fn an_ssh_key_name_is_one_to_sixty_four_printable_characters() {
    assert!(config("k").is_ok());
    assert!(config(&"k".repeat(64)).is_ok());
    for bad in ["", "two words", "tab\t", "é"] {
        assert_eq!(config(bad).unwrap_err(), ConfigError::SshKey, "{bad:?}");
    }
    assert_eq!(config(&"k".repeat(65)).unwrap_err(), ConfigError::SshKey);
    assert!(ConfigError::SshKey.to_string().contains("ssh key"));
}

#[test]
fn the_default_launch_interval_is_the_apis_twelve_seconds() {
    use js_elastic::{Acquired, ElasticProvider};
    use js_provider_lambdalabs::{Architecture, FakeLambda, LambdaProvider, OptionTable};
    use std::sync::Arc;

    use crate::rig::{TABLE, ask_as, offer};

    let fake = Arc::new(FakeLambda::new());
    fake.set_offers(vec![offer(
        "gpu_1x_sample",
        75,
        1,
        Architecture::X86_64,
        &["region-a"],
    )]);
    let clock = Arc::new(ManualClock::new());
    // No launch_interval call: the default is what is under test.
    let config = ElasticConfig::new(
        SchedulerId::new("s").unwrap(),
        "k",
        NonZeroUsize::new(5).unwrap(),
    )
    .unwrap()
    .enable()
    .live();
    let provider = LambdaProvider::new(
        fake.clone(),
        clock.clone(),
        OptionTable::parse(TABLE).unwrap(),
        config,
    );
    assert!(matches!(
        provider.acquire(&ask_as("s", "a")),
        Acquired::Accepted { .. }
    ));
    clock.advance(Duration::from_secs(11));
    assert_eq!(
        provider.acquire(&ask_as("s", "b")),
        Acquired::Deferred {
            wait: Duration::from_secs(1)
        }
    );
    clock.advance(Duration::from_secs(1));
    assert!(matches!(
        provider.acquire(&ask_as("s", "b")),
        Acquired::Accepted { .. }
    ));
}
