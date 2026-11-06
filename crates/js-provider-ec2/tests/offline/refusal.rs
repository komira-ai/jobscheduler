//! What a failed launch means for its acquire.

use js_elastic::{AcquireId, Acquired, CapacityProvider, Reason, Refusal, RefusalCode};
use js_provider_ec2::{Action, ApiError, launch_answer};

use crate::rig::{Rig, ask, live, rig};

fn refused(code: RefusalCode, text: &str) -> Acquired {
    Acquired::Refused(Refusal {
        code,
        reason: Reason::Api(text.to_owned()),
    })
}

/// Asserts that every cloud error code the launch answer lists is refused with its
/// constant (capacity, quota, shape or denied) and carries the code as the reason; that a
/// missing credential is refused as denied under `NoCredential`; and that a throttle, a
/// server fault, a code no row lists, and every error with no code (a transport error, a
/// timeout, an unreadable and an incomplete answer) is unknown. It catches a table that
/// maps by HTTP status or by an error's kind, which would turn a launch that may have
/// landed into a refusal.
#[test]
fn every_listed_cloud_error_maps_to_its_constant() {
    let table = [
        ("InsufficientInstanceCapacity", RefusalCode::Capacity),
        ("SpotMaxPriceTooLow", RefusalCode::Capacity),
        ("InstanceLimitExceeded", RefusalCode::Quota),
        ("VcpuLimitExceeded", RefusalCode::Quota),
        ("MaxSpotInstanceCountExceeded", RefusalCode::Quota),
        ("InvalidAMIID.NotFound", RefusalCode::Shape),
        ("InvalidParameterValue", RefusalCode::Shape),
        ("Unsupported", RefusalCode::Shape),
        ("InsufficientFreeAddressesInSubnet", RefusalCode::Shape),
        ("UnauthorizedOperation", RefusalCode::Denied),
        ("AuthFailure", RefusalCode::Denied),
        ("OptInRequired", RefusalCode::Denied),
    ];
    for (code, constant) in table {
        assert_eq!(
            launch_answer(&ApiError::answered(code)),
            refused(constant, code),
            "{code}"
        );
    }
    assert_eq!(
        launch_answer(&ApiError::NoCredential),
        refused(RefusalCode::Denied, "NoCredential")
    );
    let unknown = [
        ApiError::answered("RequestLimitExceeded"),
        ApiError::answered("Throttling"),
        ApiError::answered("ServiceUnavailable"),
        ApiError::answered("InternalError"),
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

/// Asserts that an acquire whose launch the cloud refuses with a code of the table is
/// refused with the table's constant and the code as the reason, launches nothing, leaves
/// the acquire resolved and the room free, and that a missing credential is denied; and that
/// a code the table does not list, a throttle and a timeout are unknown and leave the acquire
/// unresolved. It catches a refusal that holds a room, or an unknown that frees it.
#[test]
fn a_refused_launch_frees_the_room_and_an_unknown_one_keeps_it() {
    let table = [
        ("InsufficientInstanceCapacity", RefusalCode::Capacity),
        ("InstanceLimitExceeded", RefusalCode::Quota),
        ("InvalidParameterValue", RefusalCode::Shape),
        ("UnauthorizedOperation", RefusalCode::Denied),
    ];
    for (code, constant) in table {
        let rig = rig(live(1));
        rig.account
            .fail_next(Action::RunInstances, ApiError::answered(code));
        assert_eq!(
            rig.provider.acquire(&ask("a1")),
            Acquired::Refused(Refusal {
                code: constant,
                reason: Reason::Api(code.to_owned())
            })
        );
        assert_eq!(rig.account.launched(), 0);
        assert!(rig.provider.unresolved().is_empty());
        assert_eq!(rig.provider.capacity().can_add, Some(1));
    }
    let rig = rig(live(1));
    rig.account
        .fail_next(Action::RunInstances, ApiError::NoCredential);
    assert_eq!(
        rig.provider.acquire(&ask("a1")),
        Acquired::Refused(Refusal {
            code: RefusalCode::Denied,
            reason: Reason::Api("NoCredential".to_owned())
        })
    );
    for error in [
        ApiError::answered("SomethingNew"),
        ApiError::answered("ServiceUnavailable"),
        ApiError::Timeout,
    ] {
        let rig = rig_unknown(error);
        assert_eq!(rig.provider.unresolved(), [AcquireId::new("a1").unwrap()]);
        assert_eq!(rig.provider.capacity().can_add, Some(0));
    }
}

/// A rig whose first launch fails with `error`, and whose acquire `a1` has been tried.
fn rig_unknown(error: ApiError) -> Rig {
    let rig = rig(live(1));
    rig.account.fail_next(Action::RunInstances, error);
    assert_eq!(rig.provider.acquire(&ask("a1")), Acquired::Unknown);
    rig
}
