//! The negative cases of the four invariants of the walk over a pool's machine providers
//! (in [`NEGATIVE_CASES`], which the parent's table check reads with its own), each with its
//! correct siblings: hand-built records of the clouds' recording wrappers, with no server.

use std::time::Duration;

use js_elastic::RefusalCode;
use js_jobs::{AllocationId, AllocationStep, Booked, JobId};
use js_sim_cell::{AcquireAnswer, AcquireSeen, AllocationRow, CloudEvent, CloudFacts, EarlierSeen};

use super::*;

/// Every negative case of a walk invariant and the one invariant it must trip.
pub(crate) const NEGATIVE_CASES: &[NegativeCase] = &[
    (
        Invariant::ProviderOrder,
        second_asked_while_first_could_serve,
    ),
    (
        Invariant::NoSecondLaunchForOneAcquire,
        two_machines_for_one_acquire,
    ),
    (
        Invariant::QuotaPassOverHonoured,
        acquired_inside_the_quota_window,
    ),
    (
        Invariant::FreeHostBeatsAcquire,
        acquired_while_a_host_was_free,
    ),
];

/// The first provider's offer, and its zone.
const SMALL: &str = "sim_small";
const ZONE: &str = "zone-a";

/// The `quota_backoff` of these cases' first provider.
const BACKOFF: Duration = Duration::from_secs(600);

/// The first provider, `ec2`, of at most two machines, offering [`SMALL`] in [`ZONE`].
fn first() -> CloudFacts {
    CloudFacts::new("ec2", 2)
        .ranked(0, BACKOFF)
        .with_offer(SMALL, Some(ZONE))
}

/// The second provider, `lambda`, of at most two machines, offering `lambda_cpu` in
/// `zone-l`.
fn second() -> CloudFacts {
    CloudFacts::new("lambda", 2)
        .ranked(1, BACKOFF)
        .with_offer("lambda_cpu", Some("zone-l"))
}

/// The first provider as an acquire of the second saw it: holding `held` of its two
/// machines, its offer [`SMALL`] open (no definite refusal) when `open`.
fn standing(held: u64, open: bool) -> EarlierSeen {
    let offer = (SMALL.to_owned(), Some(ZONE.to_owned()));
    EarlierSeen {
        provider: "ec2".to_owned(),
        held,
        max: 2,
        open: open.then_some(offer).into_iter().collect(),
    }
}

/// An acquire at `at` of `offer` in `zone` for the acquire id `acquire`, answered
/// `answer`, while the registry held `open_hosts` open and the providers before stood as
/// `earlier`.
fn seen(at: u64, (offer, zone): (&str, &str), acquire: &str, answer: AcquireAnswer) -> AcquireSeen {
    AcquireSeen {
        at: ms(at),
        offer: offer.to_owned(),
        zone: Some(zone.to_owned()),
        acquire: acquire.to_owned(),
        answer,
        open_hosts: Vec::new(),
        earlier: Vec::new(),
    }
}

/// A machine, `fake-0001`, answered to an acquire.
fn accepted() -> AcquireAnswer {
    AcquireAnswer::Accepted {
        machine: "fake-0001".to_owned(),
    }
}

/// An acquire of the second provider at 200 while the first held `held` machines, its
/// offer open when `open`.
fn second_asked(held: u64, open: bool) -> Observations {
    let mut asked = seen(200, ("lambda_cpu", "zone-l"), "burst-1", accepted());
    asked.earlier = vec![standing(held, open)];
    Observations::quiet()
        .at(ms(300))
        .with_cloud(first())
        .with_cloud(second().with_seen(asked))
}

/// The second provider was asked while the first held none of its two machines and had
/// never refused its offer.
fn second_asked_while_first_could_serve() -> Observations {
    second_asked(0, true)
}

/// An acquire of offer [`SMALL`] in [`ZONE`] at `at` for the acquire id `acquire`,
/// answered `answer`, as the first provider's wrapper records it.
fn acquire(acquire: &str, answer: AcquireAnswer) -> CloudEvent {
    CloudEvent::Acquire {
        offer: SMALL.to_owned(),
        zone: Some(ZONE.to_owned()),
        acquire: acquire.to_owned(),
        answer,
    }
}

/// The first provider launched two machines for one acquire id, `burst-1`.
fn two_machines_for_one_acquire() -> Observations {
    let cloud = first()
        .with_launched("fake-0001", host(1), "burst-1")
        .with_launched("fake-0002", host(2), "burst-1");
    Observations::quiet().at(ms(300)).with_cloud(cloud)
}

/// The first provider refused [`SMALL`] in [`ZONE`] for quota at `refused`, and was asked
/// for it again at `again`.
fn asked_again(refused: u64, again: u64, zone: &str) -> Observations {
    let quota = AcquireAnswer::Refused(RefusalCode::Quota);
    let mut later = acquire("burst-2", accepted());
    if let CloudEvent::Acquire { zone: asked, .. } = &mut later {
        *asked = Some(zone.to_owned());
    }
    let cloud = first()
        .with_call(ms(refused), acquire("burst-1", quota))
        .with_call(ms(again), later);
    Observations::quiet().at(ms(again + 100)).with_cloud(cloud)
}

/// The first provider refused [`SMALL`] in [`ZONE`] for quota at 100 and was asked for it
/// again at 200, inside its 600 s backoff.
fn acquired_inside_the_quota_window() -> Observations {
    asked_again(100, 200, ZONE)
}

/// Allocation `a` of job `j` on host `h`, booked at 100, freed at `freed`.
fn row(j: u64, a: u8, h: u64, freed: Option<u64>) -> AllocationRow {
    AllocationRow {
        job: JobId::parse(&format!("sim-job-{j}")).expect("a job id"),
        allocation: AllocationId::from_bits([a; 16]),
        host: host(h),
        step: freed.map_or(AllocationStep::Booked, |_| AllocationStep::Freed),
        created_at: ms(100),
        end_reason: None,
        freed_at: freed.map(ms),
        booked: Booked::WholeHost,
    }
}

/// An acquire of the first provider at 200 while the registry held hosts 1 and 2 open,
/// host 1 holding a job booked at 100, and host 2 one booked at 100 and freed at
/// `freed_2` (none when `None`); the job tables read at `read`.
fn acquired_with_hosts(freed_2: Option<u64>, read: u64) -> Observations {
    let mut asked = seen(200, (SMALL, ZONE), "burst-1", accepted());
    asked.open_hosts = vec![host(1), host(2)];
    let observations = Observations::quiet()
        .at(ms(300))
        .with_cloud(first().with_seen(asked))
        .with_allocation_read(row(1, 1, 1, None), ms(read));
    match freed_2 {
        Some(freed) => observations.with_allocation_read(row(2, 2, 2, Some(freed)), ms(read)),
        None => observations,
    }
}

/// The first provider was asked at 200 while host 2 was open and held no job (the job
/// tables read at 250).
fn acquired_while_a_host_was_free() -> Observations {
    acquired_with_hosts(None, 250)
}

/// Asserts that an acquire of the second provider made while the first held none of its
/// machines and had not definitely refused its offer is flagged, by exact line, and that one
/// made while the first held its whole share, or had refused every offer, is not. Catches a
/// walk that skips the first provider, and a check that reads a provider at its share, or
/// one whose every offer was refused, as one that could serve.
#[test]
fn an_acquire_to_the_second_provider_while_the_first_could_serve_is_flagged() {
    assert_eq!(
        lines(&step_all(&second_asked_while_first_could_serve())),
        [
            "provider-order at 300: an acquire of offer lambda_cpu in zone zone-l at 200 \
             from provider lambda, while provider ec2 before it held 0 of its 2 machines and \
             its offer sim_small in zone zone-a had no definite refusal"
        ]
    );
    none(&step_all(&second_asked(2, true)));
    none(&step_all(&second_asked(0, false)));
}

/// Asserts that two machines of one acquire id are flagged, by exact line naming both,
/// and that two machines of two acquire ids, or a machine whose acquire id the case does
/// not give, are not. Catches a scaler that launches again under the same id and the cloud
/// that makes a second machine for it (design section 7, "Scaler double launch").
#[test]
fn a_second_machine_for_one_acquire_id_is_flagged() {
    assert_eq!(
        lines(&step_all(&two_machines_for_one_acquire())),
        [
            "no-second-launch-for-one-acquire at 300: the acquire id burst-1 has 2 machines: \
             m000 of provider ec2, m001 of provider ec2"
        ]
    );
    let distinct = first()
        .with_launched("fake-0001", host(1), "burst-1")
        .with_launched("fake-0002", host(2), "burst-2")
        .with_running("fake-0003", host(3));
    none(&step_all(
        &Observations::quiet().at(ms(300)).with_cloud(distinct),
    ));
}

/// Asserts that an acquire of an offer in a zone inside the `quota_backoff` of the cloud's
/// quota refusal of it is flagged, by exact line, and that one made at the very instant of
/// the refusal (an acquire of the same tick, decided before the answer came), one of
/// another zone, one at the end of the backoff and one after it, are not; and that a
/// checker that read the refusal at one step flags the acquire it reads at a later one.
/// Catches a pass-over that is not kept, kept for the wrong zone, or ended early.
#[test]
fn an_acquire_inside_the_quota_window_is_flagged() {
    assert_eq!(
        lines(&step_all(&acquired_inside_the_quota_window())),
        [
            "quota-pass-over-honoured at 300: an acquire of offer sim_small in zone zone-a \
             at 200, inside the 600s quota backoff of provider ec2 after its quota refusal \
             at 100"
        ]
    );
    none(&step_all(&asked_again(100, 100, ZONE)));
    none(&step_all(&asked_again(100, 200, "zone-b")));
    none(&step_all(&asked_again(100, 600_100, ZONE)));
    none(&step_all(&asked_again(100, 700_000, ZONE)));

    let quota = AcquireAnswer::Refused(RefusalCode::Quota);
    let refused = first().with_call(ms(100), acquire("burst-1", quota));
    let mut checker = Checker::new();
    let at_150 = Observations::quiet()
        .at(ms(150))
        .with_cloud(refused.clone());
    assert!(checker.step(&at_150, &Touched::nothing()).is_empty());
    let later = refused.with_call(ms(200), acquire("burst-2", accepted()));
    let at_300 = Observations::quiet().at(ms(300)).with_cloud(later);
    let found = checker.step(&at_300, &Touched::nothing());
    assert_eq!(flagged(&found), [Invariant::QuotaPassOverHonoured]);
}

/// Asserts that an acquire made while a registered host was open and held no job is
/// flagged once the job tables were read past it, by exact line, and that it is not judged
/// before such a read; that one whose open hosts all held a job, a job freed at the very
/// instant counting as held, is not; and that a host whose job was freed before the acquire
/// counts as free. Catches a scaler that launches before it places on a free host (design
/// 5.6, pass 1 before pass 2), and a check that judges an acquire on tables read before it.
#[test]
fn an_acquire_while_a_registered_host_was_free_is_flagged() {
    let flagged_line = "free-host-beats-acquire at 300: an acquire of offer sim_small in zone \
                        zone-a at 200 from provider ec2, while 1 registered hosts were open \
                        and held no job, host-2 first";
    assert_eq!(
        lines(&step_all(&acquired_while_a_host_was_free())),
        [flagged_line]
    );
    assert_eq!(
        lines(&step_all(&acquired_with_hosts(Some(150), 250))),
        [flagged_line]
    );
    none(&step_all(&acquired_with_hosts(Some(200), 250)));
    none(&step_all(&acquired_with_hosts(Some(250), 250)));

    let mut checker = Checker::new();
    let early = acquired_with_hosts(None, 150);
    assert!(checker.step(&early, &Touched::nothing()).is_empty());
    let read = early.with_allocation_read(row(1, 1, 1, None), ms(250));
    let found = checker.step(&read, &Touched::nothing());
    assert_eq!(flagged(&found), [Invariant::FreeHostBeatsAcquire]);
    assert!(checker.step(&read, &Touched::nothing()).is_empty());
}

/// Asserts that an acquire id a cloud answered with no answer, presented to that same cloud
/// again (the scaler settling it under its own id), is not flagged, nor is a deferred one
/// presented again to it, while a deferred one presented to another cloud is. Catches a
/// check that flags the retry the design asks for, and one that forgets a deferral.
#[test]
fn an_unanswered_acquire_presented_again_to_its_own_provider_is_not_flagged() {
    let again = |answer: AcquireAnswer, to_second: bool| {
        let first_ask = seen(100, (SMALL, ZONE), "burst-1", answer);
        let mut retry = seen(200, (SMALL, ZONE), "burst-1", accepted());
        let (first_cloud, second_cloud) = match to_second {
            true => {
                retry.earlier = vec![standing(2, true)];
                (first().with_seen(first_ask), second().with_seen(retry))
            }
            false => (first().with_seen(first_ask).with_seen(retry), second()),
        };
        Observations::quiet()
            .at(ms(300))
            .with_cloud(first_cloud)
            .with_cloud(second_cloud)
    };
    none(&step_all(&again(AcquireAnswer::Unknown, false)));
    let deferred = || AcquireAnswer::Deferred(Duration::from_secs(30));
    none(&step_all(&again(deferred(), false)));
    assert_eq!(
        flagged(&step_all(&again(deferred(), true))),
        [Invariant::NoSecondLaunchForOneAcquire]
    );
}
