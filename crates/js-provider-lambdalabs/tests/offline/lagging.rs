//! An API whose list lags its writes, and whose terminate answer names the wrong id.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use js_elastic::{Acquired, ElasticProvider, HostStatus, Reason, Released};
use js_provider_lambdalabs::{
    ApiError, Call, FakeLambda, Instance, InstanceId, InstanceTypeOffer, LambdaApi, LambdaProvider,
    LaunchRequest, ManualClock, OptionTable,
};

use crate::rig::{INTERVAL, TABLE, ask, host, id, live, offer};

/// The fake account behind a list that can hide every row, and a terminate that can name
/// another id and do nothing.
struct Lagging {
    fake: Arc<FakeLambda>,
    hide: AtomicBool,
    terminate_names: Mutex<Option<InstanceId>>,
}

impl LambdaApi for Lagging {
    fn instance_types(&self) -> Result<Vec<InstanceTypeOffer>, ApiError> {
        self.fake.instance_types()
    }
    fn list_instances(&self) -> Result<Vec<Instance>, ApiError> {
        let rows = self.fake.list_instances()?;
        match self.hide.load(Ordering::SeqCst) {
            true => Ok(Vec::new()),
            false => Ok(rows),
        }
    }
    fn get_instance(&self, id: &InstanceId) -> Result<Instance, ApiError> {
        self.fake.get_instance(id)
    }
    fn launch(&self, request: &LaunchRequest) -> Result<Vec<InstanceId>, ApiError> {
        self.fake.launch(request)
    }
    fn terminate(&self, id: &InstanceId) -> Result<Vec<InstanceId>, ApiError> {
        match self.terminate_names.lock().unwrap().clone() {
            Some(other) => Ok(vec![other]),
            None => self.fake.terminate(id),
        }
    }
}

fn rig(max_machines: usize) -> (Arc<Lagging>, Arc<ManualClock>, LambdaProvider) {
    let fake = Arc::new(FakeLambda::new());
    fake.set_offers(vec![offer(
        "gpu_1x_sample",
        75,
        1,
        js_provider_lambdalabs::Architecture::X86_64,
        &["region-a"],
    )]);
    let api = Arc::new(Lagging {
        fake,
        hide: AtomicBool::new(false),
        terminate_names: Mutex::new(None),
    });
    let clock = Arc::new(ManualClock::new());
    let provider = LambdaProvider::new(
        api.clone(),
        clock.clone(),
        OptionTable::parse(TABLE).unwrap(),
        live(max_machines),
    );
    (api, clock, provider)
}

#[test]
fn a_retry_after_a_lost_launch_answer_does_not_launch_again_while_the_list_lags() {
    let (api, clock, provider) = rig(5);
    api.hide.store(true, Ordering::SeqCst);
    api.fake
        .lose_response(Call::Launch, ApiError::Transport("reset".to_owned()));
    assert_eq!(provider.acquire(&ask("a")), Acquired::Unknown);
    assert_eq!(api.fake.launched(), 1, "the lost launch landed");
    // The interval has passed, the list still hides the instance: the retry waits for
    // the hold to end rather than launching a second instance for one acquire.
    clock.advance(INTERVAL);
    assert_eq!(
        provider.acquire(&ask("a")),
        Acquired::Deferred {
            wait: Duration::from_secs(600) - INTERVAL
        }
    );
    assert_eq!(api.fake.launched(), 1);
    assert_eq!(api.fake.count(Call::Launch), 1);
    // The list catches up: the retry finds the instance.
    api.hide.store(false, Ordering::SeqCst);
    match provider.acquire(&ask("a")) {
        Acquired::Accepted { held, .. } => assert_eq!(held.id, host("fake0001")),
        other => panic!("not accepted: {other:?}"),
    }
    assert_eq!(api.fake.launched(), 1);
}

#[test]
fn an_unresolved_launch_of_another_acquire_still_counts_toward_max_machines() {
    let (api, clock, provider) = rig(1);
    api.hide.store(true, Ordering::SeqCst);
    api.fake
        .lose_response(Call::Launch, ApiError::Transport("reset".to_owned()));
    assert_eq!(provider.acquire(&ask("a")), Acquired::Unknown);
    clock.advance(INTERVAL);
    assert!(matches!(provider.acquire(&ask("b")), Acquired::Refused(_)));
    assert_eq!(api.fake.launched(), 1);
}

#[test]
fn a_terminate_answer_naming_another_id_is_not_a_confirmation() {
    let (api, _clock, provider) = rig(5);
    assert!(matches!(
        provider.acquire(&ask("a")),
        Acquired::Accepted { .. }
    ));
    *api.terminate_names.lock().unwrap() = Some(id("other0001"));
    // The answer lists a different id and the read-back still shows the instance active.
    assert_eq!(provider.release(&host("fake0001")), Released::Unknown);
    assert_eq!(api.fake.instances()[0].status, HostStatus::Booting);
}

#[test]
fn an_extra_whose_read_is_a_404_right_after_launch_is_reported_unconfirmed() {
    let (api, _clock, provider) = rig(5);
    api.fake.launch_creates(2);
    // The first read of the extra by id is a 404, as a lagging read would answer.
    api.fake.fail_next(
        Call::GetInstance,
        ApiError::Api {
            status: 404,
            code: js_provider_lambdalabs::ErrorCode::ObjectDoesNotExist,
            request_id: None,
        },
    );
    let Acquired::Accepted { extras, .. } = provider.acquire(&ask("a")) else {
        panic!("not accepted");
    };
    assert_eq!(extras.len(), 1);
    assert_eq!(extras[0].released, Released::Unknown);
    // Nothing was terminated: the extra is still there for a sweep to report.
    assert_eq!(api.fake.count(Call::Terminate), 0);
}

/// A rig whose list hides every row from before the first launch, which is answered.
fn hidden_rig(max_machines: usize) -> (Arc<Lagging>, Arc<ManualClock>, LambdaProvider) {
    let (api, clock, provider) = rig(max_machines);
    api.hide.store(true, Ordering::SeqCst);
    (api, clock, provider)
}

#[test]
fn an_answered_launch_the_list_hides_counts_toward_max_machines() {
    let (api, clock, provider) = hidden_rig(1);
    assert!(matches!(
        provider.acquire(&ask("a")),
        Acquired::Accepted { .. }
    ));
    // The interval passes at every try; the list still shows nothing.
    for _ in 0..3 {
        clock.advance(INTERVAL);
        let Acquired::Refused(refusal) = provider.acquire(&ask("b")) else {
            panic!("launched past the cap");
        };
        assert_eq!(
            refusal.reason,
            Reason::AtMaxMachines {
                held: 1,
                max_machines: 1
            }
        );
    }
    assert_eq!(api.fake.launched(), 1);
}

#[test]
fn a_retry_of_an_answered_acquire_gets_the_first_instance_and_launches_nothing() {
    let (api, clock, provider) = hidden_rig(5);
    let Acquired::Accepted { held: first, .. } = provider.acquire(&ask("a")) else {
        panic!("not accepted");
    };
    clock.advance(INTERVAL);
    let Acquired::Accepted {
        held: again,
        extras,
    } = provider.acquire(&ask("a"))
    else {
        panic!("not accepted");
    };
    assert_eq!(again, first);
    assert_eq!(again.id, host("fake0001"));
    assert!(extras.is_empty());
    assert_eq!(api.fake.launched(), 1);
    assert_eq!(api.fake.count(Call::Launch), 1);
}

#[test]
fn once_the_list_shows_the_instance_the_count_comes_from_the_list() {
    let (api, clock, provider) = hidden_rig(1);
    assert!(matches!(
        provider.acquire(&ask("a")),
        Acquired::Accepted { .. }
    ));
    api.hide.store(false, Ordering::SeqCst);
    clock.advance(INTERVAL);
    // One instance held, not two: the listed row and the answered launch are one.
    let Acquired::Refused(refusal) = provider.acquire(&ask("b")) else {
        panic!("launched past the cap");
    };
    assert_eq!(
        refusal.reason,
        Reason::AtMaxMachines {
            held: 1,
            max_machines: 1
        }
    );
    // The entry is gone: with the instance removed and the list hidden again, nothing
    // is counted and the next acquire launches.
    api.fake.remove_instance(&id("fake0001"));
    api.hide.store(true, Ordering::SeqCst);
    assert!(matches!(
        provider.acquire(&ask("b")),
        Acquired::Accepted { .. }
    ));
    assert_eq!(api.fake.launched(), 2);
}

/// The hold of the rigs: the default `unknown_hold`.
const HOLD: Duration = Duration::from_secs(600);

fn refused_at(provider: &LambdaProvider, held: usize, max_machines: usize) {
    let Acquired::Refused(refusal) = provider.acquire(&ask("b")) else {
        panic!("launched past the cap");
    };
    assert_eq!(refusal.reason, Reason::AtMaxMachines { held, max_machines });
}

#[test]
fn at_the_end_of_the_hold_a_found_instance_still_counts_and_the_cap_refuses() {
    let (api, clock, provider) = hidden_rig(1);
    assert!(matches!(
        provider.acquire(&ask("a")),
        Acquired::Accepted { .. }
    ));
    // Just inside the hold: counted, and nothing is read by id.
    clock.advance(HOLD - Duration::from_secs(1));
    refused_at(&provider, 1, 1);
    assert_eq!(api.fake.count(Call::GetInstance), 0);
    // The hold ends: the instance is read by id, found, and still counted.
    clock.advance(Duration::from_secs(1));
    refused_at(&provider, 1, 1);
    assert_eq!(api.fake.count(Call::GetInstance), 1);
    assert_eq!(api.fake.launched(), 1);
}

#[test]
fn a_probe_that_finds_the_instance_is_repeated_once_per_hold_not_per_tick() {
    let (api, clock, provider) = hidden_rig(1);
    assert!(matches!(
        provider.acquire(&ask("a")),
        Acquired::Accepted { .. }
    ));
    clock.advance(HOLD);
    refused_at(&provider, 1, 1);
    for _ in 0..5 {
        clock.advance(INTERVAL);
        refused_at(&provider, 1, 1);
    }
    assert_eq!(api.fake.count(Call::GetInstance), 1);
    // A hold after the probe, the next one is made.
    clock.advance(HOLD);
    refused_at(&provider, 1, 1);
    assert_eq!(api.fake.count(Call::GetInstance), 2);
}

#[test]
fn a_probe_that_fails_keeps_the_instance_counted_and_is_retried_after_a_hold() {
    let (api, clock, provider) = hidden_rig(1);
    assert!(matches!(
        provider.acquire(&ask("a")),
        Acquired::Accepted { .. }
    ));
    api.fake
        .fail_next(Call::GetInstance, ApiError::Transport("reset".to_owned()));
    clock.advance(HOLD);
    refused_at(&provider, 1, 1);
    clock.advance(INTERVAL);
    refused_at(&provider, 1, 1);
    assert_eq!(api.fake.count(Call::GetInstance), 1);
    // The next probe, a hold later, succeeds and finds it.
    clock.advance(HOLD);
    refused_at(&provider, 1, 1);
    assert_eq!(api.fake.count(Call::GetInstance), 2);
}

#[test]
fn a_404_while_the_list_cannot_be_read_is_not_trusted_and_the_instance_stays_counted() {
    let (api, clock, provider) = hidden_rig(1);
    assert!(matches!(
        provider.acquire(&ask("a")),
        Acquired::Accepted { .. }
    ));
    api.fake.remove_instance(&id("fake0001"));
    clock.advance(HOLD);
    // The launch's own list passes; the check that a list works fails.
    api.fake.pass_next(Call::ListInstances);
    api.fake
        .fail_next(Call::ListInstances, ApiError::Transport("reset".to_owned()));
    refused_at(&provider, 1, 1);
    assert_eq!(api.fake.launched(), 1);
}

#[test]
fn a_trusted_404_at_the_end_of_the_hold_drops_the_instance_and_frees_the_cap() {
    let (api, clock, provider) = hidden_rig(1);
    assert!(matches!(
        provider.acquire(&ask("a")),
        Acquired::Accepted { .. }
    ));
    api.fake.remove_instance(&id("fake0001"));
    clock.advance(HOLD);
    assert!(matches!(
        provider.acquire(&ask("b")),
        Acquired::Accepted { .. }
    ));
    assert_eq!(api.fake.launched(), 2);
}

#[test]
fn a_probe_that_finds_the_instance_terminated_drops_it() {
    let (api, clock, provider) = hidden_rig(1);
    assert!(matches!(
        provider.acquire(&ask("a")),
        Acquired::Accepted { .. }
    ));
    api.fake.set_status(&id("fake0001"), HostStatus::Terminated);
    clock.advance(HOLD);
    assert!(matches!(
        provider.acquire(&ask("b")),
        Acquired::Accepted { .. }
    ));
}

#[test]
fn a_listed_row_without_our_marks_does_not_end_the_count() {
    let (api, clock, provider) = hidden_rig(1);
    assert!(matches!(
        provider.acquire(&ask("a")),
        Acquired::Accepted { .. }
    ));
    let original = api.fake.instances()[0].clone();
    // The list shows the row, stripped of tags and name: foreign to the live set.
    api.fake.update_instance(&id("fake0001"), &|row| {
        row.tags.clear();
        row.name = None;
    });
    api.hide.store(false, Ordering::SeqCst);
    clock.advance(INTERVAL);
    refused_at(&provider, 1, 1);
    // The marks come back and the list shows the row as ours: the count comes from the
    // list, and once the instance is removed the cap is free.
    api.fake
        .update_instance(&id("fake0001"), &|row| *row = original.clone());
    clock.advance(INTERVAL);
    refused_at(&provider, 1, 1);
    api.fake.remove_instance(&id("fake0001"));
    api.hide.store(true, Ordering::SeqCst);
    assert!(matches!(
        provider.acquire(&ask("b")),
        Acquired::Accepted { .. }
    ));
}

#[test]
fn an_unlisted_answered_instance_and_a_listed_one_of_another_acquire_both_count() {
    let (api, clock, provider) = hidden_rig(2);
    assert!(matches!(
        provider.acquire(&ask("a")),
        Acquired::Accepted { .. }
    ));
    let mut other = api.fake.instances()[0].clone();
    // The first row is shown stripped of its marks; a second row of ours is listed.
    api.fake.update_instance(&id("fake0001"), &|row| {
        row.tags.clear();
        row.name = None;
    });
    other.id = id("fake0099");
    api.fake.add_instance(other);
    api.hide.store(false, Ordering::SeqCst);
    clock.advance(INTERVAL);
    refused_at(&provider, 2, 2);
}

#[test]
fn every_id_a_multi_instance_launch_returns_counts_and_a_retry_gets_the_first() {
    let (api, clock, provider) = hidden_rig(2);
    // The launch asks for one instance; an answer with two is an anomaly the provider
    // releases the extra of and counts both.
    api.fake.launch_creates(2);
    let Acquired::Accepted { held, extras } = provider.acquire(&ask("a")) else {
        panic!("not accepted");
    };
    assert_eq!(held.id, host("fake0001"));
    assert_eq!(extras.len(), 1);
    assert_eq!(extras[0].id, host("fake0002"));
    clock.advance(INTERVAL);
    refused_at(&provider, 2, 2);
    // The retry is answered with the first instance and no extras.
    let Acquired::Accepted {
        held: again,
        extras,
    } = provider.acquire(&ask("a"))
    else {
        panic!("not accepted");
    };
    assert_eq!(again.id, host("fake0001"));
    assert!(extras.is_empty());
    assert_eq!(api.fake.count(Call::Launch), 1);
    // The extra is read by id with the first once the hold ends, and still counted.
    clock.advance(HOLD);
    refused_at(&provider, 2, 2);
}
