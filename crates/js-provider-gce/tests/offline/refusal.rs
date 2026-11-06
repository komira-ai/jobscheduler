//! What a failed insert means for its acquire.

use js_elastic::{AcquireId, Acquired, CapacityProvider, Reason, Refusal, RefusalCode};
use js_provider_gce::{ApiError, Method, launch_answer};
use tracing::Level;

use crate::rig::{Rig, ask, config, live, logged, rig};

fn denied(reason: Reason) -> Acquired {
    Acquired::Refused(Refusal {
        code: RefusalCode::Denied,
        reason,
    })
}

fn refused(code: RefusalCode, text: &str) -> Acquired {
    Acquired::Refused(Refusal {
        code,
        reason: Reason::Api(text.to_owned()),
    })
}

/// Asserts that every error reason and operation code the launch answer lists is refused
/// with its constant (capacity, quota, shape or denied) and carries the reason as the text;
/// that a missing credential is refused as denied under `NoCredential`; and that a rate
/// limit on operations, a throttle, a server fault, a reason no row lists, and every error
/// with no reason (a transport error, a timeout, an unreadable and an incomplete answer) is
/// unknown. It catches a table that maps by HTTP status or by an error's kind, which would
/// turn an insert that may have landed into a refusal.
#[test]
fn every_listed_cloud_error_maps_to_its_constant() {
    let table = [
        ("ZONE_RESOURCE_POOL_EXHAUSTED", RefusalCode::Capacity),
        (
            "ZONE_RESOURCE_POOL_EXHAUSTED_WITH_DETAILS",
            RefusalCode::Capacity,
        ),
        ("QUOTA_EXCEEDED", RefusalCode::Quota),
        ("INVALID_FIELD_VALUE", RefusalCode::Shape),
        ("RESOURCE_NOT_FOUND", RefusalCode::Shape),
        ("invalid", RefusalCode::Shape),
        ("notFound", RefusalCode::Shape),
        ("forbidden", RefusalCode::Denied),
        ("PERMISSION_DENIED", RefusalCode::Denied),
        ("authError", RefusalCode::Denied),
        ("UNAUTHENTICATED", RefusalCode::Denied),
    ];
    for (reason, constant) in table {
        assert_eq!(
            launch_answer(&ApiError::answered(reason)),
            refused(constant, reason),
            "{reason}"
        );
    }
    assert_eq!(
        launch_answer(&ApiError::NoCredential),
        refused(RefusalCode::Denied, "NoCredential")
    );
    let unknown = [
        ApiError::answered("RESOURCE_OPERATION_RATE_EXCEEDED"),
        ApiError::answered("rateLimitExceeded"),
        ApiError::answered("RATE_LIMIT_EXCEEDED"),
        ApiError::answered("backendError"),
        ApiError::answered("alreadyExists"),
        ApiError::answered("SomethingNew"),
        ApiError::Transport("reset".to_owned()),
        ApiError::Timeout,
        ApiError::Unreadable("x".to_owned()),
        ApiError::Incomplete,
    ];
    for error in unknown {
        assert_eq!(launch_answer(&error), Acquired::Unknown, "{error}");
    }
}

/// Asserts that an acquire whose insert the cloud refuses with a reason of the table is
/// refused with the table's constant and the reason as the text, inserts nothing, leaves
/// the acquire resolved and the room free, and that a missing credential is denied; and
/// that a reason the table does not list, a server fault and a timeout are unknown and
/// leave the acquire unresolved, named by `unresolved`, holding its room. It catches a
/// refusal that holds a room, or an unknown that frees it.
#[test]
fn a_refused_insert_frees_the_room_and_an_unknown_one_keeps_it() {
    let table = [
        ("ZONE_RESOURCE_POOL_EXHAUSTED", RefusalCode::Capacity),
        ("QUOTA_EXCEEDED", RefusalCode::Quota),
        ("INVALID_FIELD_VALUE", RefusalCode::Shape),
        ("forbidden", RefusalCode::Denied),
    ];
    for (reason, constant) in table {
        let lab = rig(live(1));
        lab.account
            .fail_next(Method::InsertInstance, ApiError::answered(reason));
        assert_eq!(lab.provider.acquire(&ask("a1")), refused(constant, reason));
        assert_eq!(lab.account.inserted(), 0);
        assert_eq!(lab.provider.capacity().can_add, Some(1));
        assert!(lab.provider.unresolved().is_empty());
    }
    let lab = rig(live(1));
    lab.account
        .fail_next(Method::InsertInstance, ApiError::NoCredential);
    assert_eq!(
        lab.provider.acquire(&ask("a1")),
        refused(RefusalCode::Denied, "NoCredential")
    );
    assert_eq!(lab.provider.capacity().can_add, Some(1));
    for error in [
        ApiError::answered("SomethingNew"),
        ApiError::answered("backendError"),
        ApiError::Timeout,
    ] {
        let lab = rig_unknown(error);
        assert_eq!(lab.provider.capacity().can_add, Some(0));
        assert_eq!(lab.provider.unresolved(), [AcquireId::new("a1").unwrap()]);
    }
}

/// A rig whose first insert fails with `error`, and whose acquire `a1` has been tried.
fn rig_unknown(error: ApiError) -> Rig {
    let lab = rig(live(1));
    lab.account.fail_next(Method::InsertInstance, error);
    assert_eq!(lab.provider.acquire(&ask("a1")), Acquired::Unknown);
    lab
}

/// Asserts that a provider that is not enabled refuses every acquire as denied, and that
/// nothing is sent to the cloud.
#[test]
fn a_provider_that_is_not_enabled_refuses_and_sends_nothing() {
    let lab = rig(config(1).live());
    assert_eq!(lab.provider.acquire(&ask("a1")), denied(Reason::Disabled));
    assert!(lab.account.methods().is_empty());
}

/// Asserts that a dry run builds the insert, logs it once at info level and after that at
/// debug level, refuses it as denied with the dry-run reason, and sends nothing; and that
/// no log line carries the payload.
#[test]
fn a_dry_run_logs_the_insert_once_and_sends_nothing() {
    let info = rig(config(1).enable());
    let (answers, lines) = logged(Level::INFO, || {
        [
            info.provider.acquire(&ask("a1")),
            info.provider.acquire(&ask("a1")),
        ]
    });
    let dry = denied(Reason::DryRun);
    assert_eq!(answers, [dry.clone(), dry]);
    assert_eq!(lines.matches("dry run: an insert would be sent").count(), 1);
    assert!(!lines.contains("user-data"), "{lines}");
    assert!(info.account.methods().is_empty());

    let debug = rig(config(1).enable());
    let (_, lines) = logged(Level::DEBUG, || {
        debug.provider.acquire(&ask("a1"));
        debug.provider.acquire(&ask("a1"));
    });
    assert_eq!(lines.matches("dry run: an insert would be sent").count(), 2);
    assert!(debug.account.methods().is_empty());
}
