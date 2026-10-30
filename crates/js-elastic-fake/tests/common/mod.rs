//! Fixtures the tests of the fake cloud share.
#![allow(dead_code)]

use std::time::Duration;

use js_elastic::{
    AcquireId, AcquireRequest, Acquired, BootPayload, GpuClass, Held, HostName, HostRef, Offer,
    PriceUnit, SchedulerId,
};
use js_elastic_fake::{Billing, ElasticFake, FakeClock};
use js_types::ExecMode;

/// The time the fake's clock starts at, in milliseconds.
pub const START: u64 = 1_000_000_000_000;

/// The price of the fake's one offer, in cents per hour: one cent a minute.
pub const PRICE: u64 = 60;

pub fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

pub fn mins(n: u64) -> Duration {
    Duration::from_secs(60 * n)
}

/// The one offer the fake lists: an a10 machine in zone `z1`.
pub fn offer(price: Option<u64>) -> Offer {
    Offer {
        id: "a10".to_owned(),
        zone: Some("z1".to_owned()),
        cpu_millis: 8000,
        memory_bytes: 64 << 30,
        disk_bytes: 512 << 30,
        gpus: 1,
        gpu_class: Some(GpuClass::parse("nvidia-a10").unwrap()),
        os: "linux".to_owned(),
        arch: "x86_64".to_owned(),
        exec: ExecMode::Container,
        price_cents_per_hour: price,
        price_unit: PriceUnit::UsCents,
        available: true,
    }
}

/// A cloud that bills as `billing`, lists the one offer at [`PRICE`] and starts at [`START`].
pub fn cloud(billing: Billing) -> ElasticFake {
    ElasticFake::new(FakeClock::new(START), billing, vec![offer(Some(PRICE))])
}

/// An acquire of the machine `acquire` for the offer the cloud lists. `believed` is the
/// price the caller thinks the offer has, which the cloud must not bill by.
pub fn request_at(acquire: &str, believed: Option<u64>) -> AcquireRequest {
    let scheduler = SchedulerId::new("sched").unwrap();
    let acquire = AcquireId::new(acquire).unwrap();
    AcquireRequest {
        name: HostName::derive(&scheduler, &acquire),
        acquire,
        pool: "gpu".to_owned(),
        offer: offer(believed),
        labels: Vec::new(),
        payload: BootPayload::new("user-data-MARKER".to_owned()),
    }
}

pub fn request(acquire: &str) -> AcquireRequest {
    request_at(acquire, Some(PRICE))
}

/// The machine an acquire made, which must have been accepted.
pub fn accepted(answer: Acquired) -> Held {
    match answer {
        Acquired::Accepted { held, extras } => {
            assert!(extras.is_empty());
            held
        }
        other => panic!("expected an accepted launch, got {other:?}"),
    }
}

/// Launches `acquire` and returns the machine's id.
pub fn launch(cloud: &ElasticFake, acquire: &str) -> HostRef {
    use js_elastic::ElasticProvider;
    accepted(cloud.acquire(&request(acquire))).id
}
