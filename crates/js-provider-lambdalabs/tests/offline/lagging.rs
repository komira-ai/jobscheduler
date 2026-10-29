//! An API whose list lags its writes, and whose terminate answer names the wrong id.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use js_provider_lambdalabs::{
    Acquired, ApiError, Call, Elastic, FakeLambda, Instance, InstanceId, InstanceTypeOffer,
    LambdaApi, LambdaProvider, LaunchRequest, ManualClock, OptionTable, Reason, Released, Status,
};

use crate::rig::{INTERVAL, TABLE, ask, id, live, offer};

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
        Acquired::Accepted { held, .. } => assert_eq!(held.id, id("fake0001")),
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
    assert_eq!(provider.release(&id("fake0001")), Released::Unknown);
    assert_eq!(api.fake.instances()[0].status, Status::Booting);
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
    assert_eq!(again.id, id("fake0001"));
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

#[test]
fn after_the_hold_an_instance_the_list_still_hides_is_no_longer_counted() {
    let (api, clock, provider) = hidden_rig(1);
    assert!(matches!(
        provider.acquire(&ask("a")),
        Acquired::Accepted { .. }
    ));
    // Just inside the hold: still counted.
    clock.advance(Duration::from_secs(599));
    assert!(matches!(provider.acquire(&ask("b")), Acquired::Refused(_)));
    // The hold ends: the list is trusted again, as for an unknown launch. The retry of
    // the same acquire launches, and so does the one for another id once it is due.
    clock.advance(Duration::from_secs(1));
    assert!(matches!(
        provider.acquire(&ask("a")),
        Acquired::Accepted { .. }
    ));
    assert_eq!(api.fake.launched(), 2);
}
