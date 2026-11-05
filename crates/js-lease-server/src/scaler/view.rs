//! The registry's words turned into the scaler's: a host as [`HostFacts`], a waiting
//! request as [`Waiting`].

use js_elastic::{GpuClass, Offer, PriceUnit};
use js_registry::{HostMark, HostState, HostView, Waiting as Request};
use js_scaler::{HostFacts, HostState as Hearing, Mark, Waiting};
use js_types::{ExecMode, IsaLevel, Shape, normalise_arch};

/// Bytes in a GiB, the unit of the `disk-gib` capability.
const GIB: u64 = 1 << 30;

/// The shape of a host in the form of an offer, so that one rule judges a host and an
/// offer alike. The price and availability of a host are not read.
///
/// The offer's one disk number serves two checks of [`Offer::fit`]: the scratch disk a
/// request books and the request's `disk-gib` requirement. It is the larger of the scratch
/// disk the pool books for the host (`booked`, the shape the scheduler places against and
/// the registry gates every waiting request on) and the disk the host advertised in its
/// `disk-gib` capability (from its hello, so it is the host's own word, parsed as whole
/// GiB; text that is not, or does not fit, counts as none). Taking the larger keeps a
/// request the registry admitted covered by a host that serves it. A host with neither
/// offers none, so a request for disk is not counted as served by it. It is never
/// `u64::MAX`: a host said to have all the disk covers a request the scheduler refuses.
///
/// The architecture is the host's `arch` capability in the one spelling ([`normalise_arch`]:
/// an agent that reports `aarch64` offers `arm64`). The ISA level is the one the pool books
/// for the host: the highest of the levels the `isa_level` capability of `booked` lists
/// ([`IsaLevel::highest_listed`]). The registry places a request against the booked shape,
/// which every host of the pool was held to at its hello, so a level the host reports above
/// it is not stated: the scaler would count the host for a request the registry never
/// places on it. A pool that books no level, or a text that is not a list of levels of one
/// family, states none.
///
/// Today only agent pools and static host files can book disk; elastic and joined pools
/// have no `disk_bytes` key, so a request for disk there is refused before the scaler
/// sees it.
fn offer_of(shape: &Shape, booked: &Shape, exec: ExecMode) -> Offer {
    let resources = shape.resources();
    let capabilities = shape.capabilities();
    let text = |key: &str| capabilities.get(key).cloned().unwrap_or_default();
    let reported = capabilities
        .get("disk-gib")
        .and_then(|gib| gib.parse::<u64>().ok())
        .and_then(|gib| gib.checked_mul(GIB))
        .unwrap_or(0);
    let disk_bytes = booked.resources().disk_bytes().max(reported);
    Offer {
        id: String::new(),
        zone: None,
        cpu_millis: resources.cpu_millis(),
        memory_bytes: resources.memory_bytes(),
        disk_bytes,
        gpus: u32::try_from(resources.gpus()).unwrap_or(u32::MAX),
        gpu_class: resources
            .gpu_class()
            .and_then(|class| GpuClass::parse(class).ok()),
        os: text("os"),
        arch: normalise_arch(&text("arch")).to_owned(),
        isa_level: booked
            .capabilities()
            .get("isa_level")
            .map(String::as_str)
            .and_then(IsaLevel::highest_listed),
        exec,
        price_cents_per_hour: None,
        price_unit: PriceUnit::UsCents,
        available: true,
    }
}

fn mark_of(mark: HostMark) -> Mark {
    match mark {
        HostMark::Reserved => Mark::Reserved,
        HostMark::Held => Mark::Held,
        HostMark::Draining => Mark::Draining,
        HostMark::Releasing => Mark::Releasing,
    }
}

fn hearing(state: HostState) -> Hearing {
    match state {
        HostState::Live => Hearing::Live,
        HostState::Lost => Hearing::Lost,
        HostState::Awaiting => Hearing::Awaiting,
    }
}

/// `host`, a host of the pool `pool` whose jobs run in `exec` mode, as the scaler reads it.
/// What it holds is what it announced, if it did. It is busy when it holds a lease or still has runs to finish. It has given
/// up its leases when the registry does not hear from it and books no lease on it: a silent
/// host that never held one included.
///
/// `shape` is `Some` only when the host announced one (the registry's `claim`), and `None`
/// for a host that has announced none since the registry was put back after a restart. The
/// shell never stands in the pool's floor (`host.shape`) or a stored shape: the floor is
/// below any offer and would read as a hello that falls short.
pub(super) fn host_facts(host: HostView, pool: &str, exec: ExecMode) -> HostFacts {
    let shape = host
        .claim
        .as_ref()
        .map(|claim| offer_of(claim, &host.shape, exec));
    HostFacts {
        shape,
        pool: pool.to_owned(),
        state: hearing(host.state),
        mark: host.mark.map(mark_of),
        busy: host.lease.is_some() | host.releasing,
        lease_given_up: (host.state != HostState::Live) & host.lease.is_none(),
        id: host.id,
        heard_since_hello: host.heard_since_hello,
    }
}

/// `request`, a waiting request, as the scaler reads it.
pub(super) fn waiting(request: Request) -> Waiting {
    Waiting {
        id: request.id,
        demand: request.demand,
        reserved: request.reserved,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use js_types::Resources;

    use super::*;

    fn shape(disk_bytes: u64, capabilities: &[(&str, &str)]) -> Shape {
        let resources = Resources::new(8000, 1 << 34, 0, None)
            .unwrap()
            .with_disk_bytes(disk_bytes);
        let capabilities: BTreeMap<String, String> = capabilities
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect();
        Shape::new(resources, capabilities)
    }

    fn disk(claim: &Shape, booked: &Shape) -> u64 {
        offer_of(claim, booked, ExecMode::Container).disk_bytes
    }

    /// Asserts that a host's offer carries the scratch disk the pool books for it, not
    /// the disk the hello's shape claims (a claim above the booking is never leased
    /// out), and that a host with none booked and none advertised offers zero. It catches an offer
    /// that stays unbounded (`u64::MAX`), which makes the scaler count a host for a
    /// request the scheduler refuses, so that nothing is launched for it.
    #[test]
    fn a_hosts_offer_is_the_disk_the_pool_books() {
        let booked = shape(100 << 30, &[]);
        assert_eq!(disk(&shape(500 << 30, &[]), &booked), 100 << 30);
        assert_eq!(disk(&shape(0, &[]), &shape(0, &[])), 0);
    }

    /// Asserts that a request that books scratch disk does not fit a host that books none,
    /// and fits one that books enough, through the same `fit` rule the catalog's offers
    /// are judged by, with and without a `disk-gib` requirement. It catches a host offer
    /// that stays unbounded, which the scaler would count for a request the scheduler
    /// refuses.
    #[test]
    fn a_request_for_booked_disk_fits_only_a_host_that_books_it() {
        use js_elastic::Demand;
        let wants = |bytes: u64| Demand {
            resources: Resources::new(1000, 1 << 30, 0, None)
                .unwrap()
                .with_disk_bytes(bytes),
            disk_bytes: None,
            os: None,
            arch: None,
            isa_level: None,
            exec: None,
            network: Default::default(),
            host_ports: Vec::new(),
        };
        let none = offer_of(&shape(0, &[]), &shape(0, &[]), ExecMode::Container);
        let some = offer_of(&shape(0, &[]), &shape(10 << 30, &[]), ExecMode::Container);
        assert!(none.fit(&wants(10 << 30)).is_err());
        assert_eq!(some.fit(&wants(10 << 30)), Ok(()));
        assert!(some.fit(&wants((10 << 30) + 1)).is_err());
    }

    /// Asserts that the `disk-gib` capability a host reports is the disk a `disk-gib`
    /// requirement is judged against (the larger of it and the booked disk), that text
    /// that is not a whole number of GiB, or one too large to count, reads as no disk,
    /// and that the offer never overflows. It catches an offer that ignores the
    /// capability, which would launch a machine for a request an existing host serves.
    #[test]
    fn the_disk_gib_capability_counts_and_a_bad_one_reads_as_none() {
        let claim = |text: &str| shape(0, &[("disk-gib", text)]);
        let none = shape(0, &[]);
        assert_eq!(disk(&claim("200"), &none), 200 << 30);
        assert_eq!(disk(&claim("200"), &shape(300 << 30, &[])), 300 << 30);
        for bad in ["lots", "-1", "", "18446744073709551615"] {
            assert_eq!(disk(&claim(bad), &none), 0, "{bad:?}");
        }
    }

    /// A demand for the capabilities `caps`, as the registry reads a waiting request.
    fn demand_of(caps: &[(&str, &str)]) -> js_elastic::Demand {
        let request = js_types::Request {
            class: js_types::ClassId::new("whole-node").unwrap(),
            resources: Resources::new(1000, 1 << 30, 0, None).unwrap(),
            capabilities: caps
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect(),
            max_wall: std::time::Duration::from_secs(60),
            labels: BTreeMap::new(),
            validation_run: None,
            probe_host: None,
        };
        js_elastic::Demand::from_request(&request)
    }

    /// Asserts that the offer of a host whose `arch` capability is `aarch64` (an agent
    /// built before `arm64` was the one spelling) fits a request for `arch=arm64`, and for
    /// `arch=aarch64`, and that a request for `arch=x86_64` against it still fails on the
    /// architecture. Catches an offer that keeps the host's `aarch64`, which `fit`
    /// compares by equality against the request's `arm64`, and a normalisation that makes
    /// every architecture fit.
    #[test]
    fn an_arm64_request_fits_an_aarch64_offer() {
        let host = shape(0, &[("arch", "aarch64"), ("os", "linux")]);
        let offer = offer_of(&host, &shape(0, &[]), ExecMode::Container);
        assert_eq!(offer.arch, "arm64");
        assert_eq!(offer.fit(&demand_of(&[("arch", "arm64")])), Ok(()));
        assert_eq!(offer.fit(&demand_of(&[("arch", "aarch64")])), Ok(()));
        assert_eq!(
            offer.fit(&demand_of(&[("arch", "x86_64")])),
            Err(js_elastic::Dimension::Arch)
        );
        let other = offer_of(&shape(0, &[("arch", "sparc")]), &host, ExecMode::Container);
        assert_eq!(other.arch, "sparc");
    }

    /// Asserts that a host's offer states the highest ISA level the pool books for it, one
    /// level or the list of it and those below, never a higher one the host reports, and
    /// none when the pool books none (whatever the host reports), an unknown name or a
    /// mixed list; and that a request for a level fits only the host whose pool books one at
    /// least as high. Catches an offer that reads the host's own claim, which counts a host
    /// for a request the registry never places on it, so nothing is launched and the
    /// request waits for ever.
    #[test]
    fn a_hosts_offer_states_the_level_the_pool_books() {
        use js_types::X86Level;
        let at = |claim: &[(&str, &str)], booked: &[(&str, &str)]| {
            offer_of(&shape(0, claim), &shape(0, booked), ExecMode::Container)
        };
        let level = |claim: &[(&str, &str)], booked: &[(&str, &str)]| at(claim, booked).isa_level;
        let v3 = Some(IsaLevel::X86_64(X86Level::V3));
        let v4 = [("isa_level", "x86-64-v4")];
        assert_eq!(level(&v4, &[("isa_level", "x86-64-v3")]), v3);
        assert_eq!(
            level(&v4, &[("isa_level", "x86-64-v1,x86-64-v2,x86-64-v3")]),
            v3
        );
        assert_eq!(level(&v4, &[]), None);
        for none in ["", "x86-64-v9", "x86-64-v3,armv8.4-a"] {
            assert_eq!(level(&v4, &[("isa_level", none)]), None, "{none:?}");
        }
        let wants_v3 = demand_of(&[("isa_level", "x86-64-v3")]);
        assert_eq!(at(&v4, &v4).fit(&wants_v3), Ok(()));
        assert_eq!(
            at(&v4, &[("isa_level", "x86-64-v2")]).fit(&wants_v3),
            Err(js_elastic::Dimension::IsaLevel)
        );
        assert_eq!(
            at(&v4, &[]).fit(&wants_v3),
            Err(js_elastic::Dimension::IsaLevel)
        );
    }
}
