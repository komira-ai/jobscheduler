//! The whole-node settings on a pool: which classes take them and what is refused.

mod support;

use js_registry::{
    ConfigError, DEFAULT_REGAIN_GRACE_MS, DEFAULT_RESERVE_BACKOFF_MS, PoolConfig, WholeNodeConfig,
};
use js_types::{Admission, ClassId, Dims, ProviderId, ResourceClass};
use support::{config, pool_shape};

fn pool_of(admission: Admission, max_machines: usize) -> PoolConfig {
    let class = ResourceClass::new(ClassId::new("any").unwrap(), admission, 0);
    let name = ProviderId::new("p").unwrap();
    PoolConfig::new(name, class, pool_shape(), max_machines)
}

/// A pool has no settings until it is given some, and a dedicated pool (class `Whole`)
/// takes the dedicated defaults: they are kept, and with 6 hosts and a ceiling of 4 the
/// surplus is 2. It catches a setting that is checked and then dropped, and a surplus
/// that counts the wrong way round.
#[test]
fn a_dedicated_pool_keeps_its_settings_and_reports_the_surplus() {
    let pool = pool_of(Admission::Whole, 6);
    assert_eq!((pool.whole_node(), pool.whole_surplus()), (None, 0));
    let full = pool.clone().with_whole_node(WholeNodeConfig::dedicated(6));
    let full = full.unwrap();
    assert_eq!(full.whole_node(), Some(&WholeNodeConfig::dedicated(6)));
    assert_eq!(full.whole_surplus(), 0);
    let fewer = WholeNodeConfig::dedicated(4);
    let some = pool.with_whole_node(fewer).unwrap();
    assert_eq!(some.whole_surplus(), 2);
}

/// A mixed pool (class `Book`) fits when `max_whole_node + keep_for_ordinary` is at most
/// `max_machines` and is refused one over; a dedicated pool refuses a kept host and a `max_whole_node` above `max_machines`; a class
/// that books all without the whole-node lease refuses the settings; a zero names its
/// key. Each is the refusal the design names, so a loosened comparison fails.
#[test]
fn settings_are_checked_against_the_pool() {
    let book = Admission::Book(Dims::new());
    let fits = pool_of(book.clone(), 3).with_whole_node(WholeNodeConfig::mixed(2));
    assert!(fits.is_ok());
    let over = pool_of(book.clone(), 3).with_whole_node(WholeNodeConfig::mixed(3));
    assert_eq!(over.unwrap_err(), ConfigError::WholeCeiling);
    let none_kept = WholeNodeConfig {
        keep_for_ordinary: 0,
        ..WholeNodeConfig::mixed(3)
    };
    assert!(pool_of(book, 3).with_whole_node(none_kept).is_ok());
    let kept = WholeNodeConfig {
        keep_for_ordinary: 1,
        ..WholeNodeConfig::dedicated(3)
    };
    let dedicated = pool_of(Admission::Whole, 3).with_whole_node(kept);
    assert_eq!(dedicated.unwrap_err(), ConfigError::DedicatedKeep);
    let above = pool_of(Admission::Whole, 3).with_whole_node(WholeNodeConfig::dedicated(4));
    assert_eq!(above.unwrap_err(), ConfigError::WholeAboveMachines);
    let all = config(3).with_whole_node(WholeNodeConfig::dedicated(3));
    assert_eq!(all.unwrap_err(), ConfigError::WholeNodeClass);
    let zero = WholeNodeConfig {
        max_draining: 0,
        ..WholeNodeConfig::dedicated(3)
    };
    let refused = pool_of(Admission::Whole, 3)
        .with_whole_node(zero)
        .unwrap_err();
    assert_eq!(refused, ConfigError::WholeZero("max_draining"));
    assert_eq!(refused.to_string(), "max_draining must be at least 1");
}

/// Each default of the design's table, key by key, so a changed default fails here.
#[test]
fn dedicated_defaults_are_the_tables() {
    let c = WholeNodeConfig::dedicated(6);
    assert_eq!(
        (c.max_whole_node, c.max_draining, c.keep_for_ordinary),
        (6, 1, 0)
    );
    assert_eq!(
        (c.max_whole_node_per_consumer, c.allow_last_capable),
        (None, false)
    );
    assert_eq!((c.drain_after_ms, c.drain_deadline_ms), (0, None));
    assert_eq!((c.max_reserve_attempts, c.max_replace), (2, 3));
    assert_eq!(c.prepare_timeout_ms, 600_000);
    assert_eq!(c.reserve_backoff_ms, DEFAULT_RESERVE_BACKOFF_MS);
    assert_eq!(c.regain_grace_ms, DEFAULT_REGAIN_GRACE_MS);
    assert_eq!(c.per_consumer_limit(), 6);
    assert_eq!(c.drain_deadline_ms(900), 900);
}

/// A mixed pool keeps one host for ordinary work; set keys win over the defaults, and
/// an absent deadline resolves to the max wall time given at use.
#[test]
fn mixed_keeps_one_and_set_keys_win() {
    let c = WholeNodeConfig::mixed(2);
    assert_eq!((c.max_whole_node, c.keep_for_ordinary), (2, 1));
    let set = WholeNodeConfig {
        max_whole_node_per_consumer: Some(1),
        drain_deadline_ms: Some(5),
        ..c
    };
    assert_eq!(
        (set.per_consumer_limit(), set.drain_deadline_ms(900)),
        (1, 5)
    );
}

/// Every key where zero is meaningless is refused by name when it is zero, whichever of
/// them it is; `drain_after_ms` may be zero (it is, by default) and is not refused.
#[test]
fn zero_is_refused_per_key() {
    let base = || WholeNodeConfig::dedicated(3);
    let cases = [
        (
            "max_whole_node",
            WholeNodeConfig {
                max_whole_node: 0,
                ..base()
            },
        ),
        (
            "max_draining",
            WholeNodeConfig {
                max_draining: 0,
                ..base()
            },
        ),
        (
            "max_whole_node_per_consumer",
            WholeNodeConfig {
                max_whole_node_per_consumer: Some(0),
                ..base()
            },
        ),
        (
            "max_reserve_attempts",
            WholeNodeConfig {
                max_reserve_attempts: 0,
                ..base()
            },
        ),
        (
            "max_replace",
            WholeNodeConfig {
                max_replace: 0,
                ..base()
            },
        ),
        (
            "reserve_backoff",
            WholeNodeConfig {
                reserve_backoff_ms: 0,
                ..base()
            },
        ),
        (
            "prepare_timeout",
            WholeNodeConfig {
                prepare_timeout_ms: 0,
                ..base()
            },
        ),
        (
            "regain_grace",
            WholeNodeConfig {
                regain_grace_ms: 0,
                ..base()
            },
        ),
        (
            "drain_deadline",
            WholeNodeConfig {
                drain_deadline_ms: Some(0),
                ..base()
            },
        ),
    ];
    for (key, config) in cases {
        let refused = pool_of(Admission::Whole, 3).with_whole_node(config);
        assert_eq!(refused.unwrap_err(), ConfigError::WholeZero(key));
    }
    assert!(pool_of(Admission::Whole, 3).with_whole_node(base()).is_ok());
}
