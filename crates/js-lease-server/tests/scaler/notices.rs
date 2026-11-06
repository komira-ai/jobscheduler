//! The provider's notices through the scaler's shell: a reclaim cordons the host and has the
//! machine released at once, without waiting for a list that still shows it running. The
//! reader runs on a thread of its own; the test knows it is done with a notice when it asks
//! for the next one (or drops the stream), never by waiting for a time.

use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use js_elastic::{EventStream, Held, HostRef, HostStatus, ProviderEvent};
use js_scaler::{Phase, Records};

use crate::fakes::{Rig, live, record, records, rig, view};

/// A stream of notices the test sends, that tells the test each time the reader comes back
/// for the next one and when it drops the stream.
struct Paced {
    notices: Receiver<ProviderEvent>,
    back: Sender<()>,
}

impl Iterator for Paced {
    type Item = ProviderEvent;

    fn next(&mut self) -> Option<ProviderEvent> {
        let _ = self.back.send(());
        self.notices.recv().ok()
    }
}

impl Drop for Paced {
    fn drop(&mut self) {
        let _ = self.back.send(());
    }
}

/// The test's end of a [`Paced`] stream.
struct Provider {
    send: Option<Sender<ProviderEvent>>,
    back: Receiver<()>,
}

impl Provider {
    /// Sends `notice` and waits until the reader has acted on it.
    fn reclaims(&self, id: &str) {
        let notice = ProviderEvent::Reclaimed(HostRef(id.to_owned()));
        self.send.as_ref().unwrap().send(notice).unwrap();
        self.settled();
    }

    /// Waits for the reader to come back for a notice or drop the stream. The bound only
    /// keeps a failing test from hanging.
    fn settled(&self) {
        let back = self.back.recv_timeout(Duration::from_secs(30));
        back.expect("the reader never came back for a notice");
    }

    /// Ends the stream and waits until the reader has dropped it.
    fn ends(&mut self) {
        self.send = None;
        self.settled();
    }
}

/// Hands the rig's provider one more notice stream and returns the test's end of it.
fn stream(rig: &Rig) -> Provider {
    let (send, notices) = channel();
    let (back, settled) = channel();
    let paced: EventStream = Box::new(Paced { notices, back });
    rig.world().notices.push(paced);
    Provider {
        send: Some(send),
        back: settled,
    }
}

/// A rig whose records hold the registered machine `js-a` (provider id `i-9`), whose
/// registry holds its host, and whose provider lists it active: a list that does not show
/// a reclaim yet. The first tick has run and opened the stream; the log is empty.
async fn registered() -> (Rig, Provider) {
    let rig = rig(records(vec![record("js-a", Phase::Registered)]), live());
    rig.world().hosts = vec![view("js-a")];
    rig.world().machines = vec![Held {
        id: HostRef("i-9".to_owned()),
        acquire: "gpu-js-a".to_owned(),
        name: Some("js-a".to_owned()),
        offer: "a10".to_owned(),
        zone: Some("z1".to_owned()),
        status: HostStatus::Active,
    }];
    let provider = stream(&rig);
    rig.scaler.tick().await;
    provider.settled();
    assert!(rig.scaler.notices_open());
    rig.world().log.clear();
    (rig, provider)
}

fn failures(rig: &Rig) -> u32 {
    let state = rig.scaler.state();
    state
        .records
        .brake
        .get("gpu")
        .map_or(0, |brake| brake.failures)
}

/// Asserts that a reclaim notice for a registered machine, while the list still shows it
/// active and with no tick of the server's, cordons its host first and then has the machine
/// released through the step's route: the records are written, the host is marked ending,
/// the token revoked and the release sent; the brake counts one failure, as for a machine a
/// list shows preempted, and nothing is ignored. It catches a notice nobody reads (the host
/// stays open to work until a list catches up), a cordon after the release, and a reclaim
/// that skips the brake.
#[tokio::test]
async fn a_reclaim_cordons_the_host_and_releases_the_machine_at_once() {
    let (rig, provider) = registered().await;
    provider.reclaims("i-9");
    assert_eq!(
        rig.log(),
        [
            "Cordon:js-a",
            "save",
            "MarkEnding:js-a",
            "revoke:js-a",
            "release:js-a"
        ]
    );
    assert!(rig.world().hosts[0].cordon_until.is_some());
    assert_eq!(
        rig.scaler.state().records.instances[0].phase,
        Phase::Releasing
    );
    assert_eq!(failures(&rig), 1);
    assert_eq!(rig.scaler.reclaims_ignored(), 0);
}

/// Asserts that the same notice twice cordons once, revokes once, and counts once; and that
/// the server's next tick, whose list now shows the machine preempted, does not count it
/// again. It catches a repeat that is acted on twice.
#[tokio::test]
async fn a_repeated_reclaim_is_acted_on_once() {
    let (rig, provider) = registered().await;
    provider.reclaims("i-9");
    provider.reclaims("i-9");
    rig.world().machines[0].status = HostStatus::Preempted;
    rig.scaler.tick().await;
    let log = rig.log();
    let count = |entry: &str| log.iter().filter(|each| *each == entry).count();
    assert_eq!(count("Cordon:js-a"), 1, "{log:?}");
    assert_eq!(count("revoke:js-a"), 1, "{log:?}");
    assert_eq!(failures(&rig), 1);
    assert_eq!(rig.scaler.reclaims_ignored(), 0);
}

/// Asserts that a notice naming no machine of the records changes nothing: no call is made,
/// the host is not cordoned, the machine stays registered, and the notice is counted as
/// ignored. It catches a notice that cordons or releases by a wrong mapping, and one that
/// panics on an id it does not know.
#[tokio::test]
async fn a_reclaim_of_an_unknown_machine_is_ignored_and_counted() {
    let (rig, provider) = registered().await;
    provider.reclaims("i-404");
    assert!(rig.log().is_empty(), "{:?}", rig.log());
    assert_eq!(rig.world().hosts[0].cordon_until, None);
    assert_eq!(
        rig.scaler.state().records.instances[0].phase,
        Phase::Registered
    );
    assert_eq!(rig.scaler.reclaims_ignored(), 1);
    assert_eq!(failures(&rig), 0);
}

/// Asserts that a booting machine named by a notice is released with no cordon (it has no
/// host to cordon yet). It catches a cordon asked for a host the registry does not hold.
#[tokio::test]
async fn a_reclaimed_booting_machine_is_released_without_a_cordon() {
    let rig = rig(records(vec![record("js-b", Phase::Booting)]), live());
    let provider = stream(&rig);
    rig.scaler.tick().await;
    provider.settled();
    rig.world().log.clear();
    provider.reclaims("i-9");
    let log = rig.log();
    assert!(
        !log.iter().any(|each| each.starts_with("Cordon")),
        "{log:?}"
    );
    assert!(log.contains(&"revoke:js-b".to_owned()), "{log:?}");
    assert_eq!(
        rig.scaler.state().records.instances[0].phase,
        Phase::Releasing
    );
}

/// Asserts that a reclaim acted on while the records cannot be written is kept: the host is
/// cordoned, the failed write leaves the machine registered and releases nothing, a repeat
/// while it is pending does nothing more, and the next tick that can write releases it. It
/// catches a reclaim dropped by a failed write, and a pending repeat that cordons again.
#[tokio::test]
async fn a_reclaim_is_kept_until_its_records_are_written() {
    let (rig, provider) = registered().await;
    rig.world().save_ok = false;
    provider.reclaims("i-9");
    assert_eq!(rig.log(), ["Cordon:js-a", "save"]);
    assert_eq!(
        rig.scaler.state().records.instances[0].phase,
        Phase::Registered
    );
    provider.reclaims("i-9");
    assert_eq!(rig.log(), ["Cordon:js-a", "save"], "a pending repeat");
    rig.world().save_ok = true;
    rig.scaler.tick().await;
    assert!(
        rig.log().contains(&"release:js-a".to_owned()),
        "{:?}",
        rig.log()
    );
    assert_eq!(
        rig.scaler.state().records.instances[0].phase,
        Phase::Releasing
    );
}

/// Asserts that a provider that sends no notices is asked once, starts no reader and is not
/// asked again, and that the scaler ticks on as before. It catches a scaler that needs a
/// stream, and one that asks again at every tick.
#[tokio::test]
async fn a_provider_that_sends_no_notices_is_asked_once() {
    let rig = rig(Records::default(), live());
    rig.scaler.tick().await;
    rig.scaler.tick().await;
    assert_eq!(rig.world().notices_asked, 1);
    assert!(!rig.scaler.notices_open());
    assert_eq!(rig.scaler.reclaims_ignored(), 0);
}

/// Asserts that a stream that ends is closed and opened again at the next tick, and that a
/// notice on the new stream is acted on. It catches a scaler that stops reading after a
/// stream ends, and one that crashes on it.
#[tokio::test]
async fn a_stream_that_ends_is_opened_again_at_the_next_tick() {
    let (rig, mut provider) = registered().await;
    provider.ends();
    assert!(!rig.scaler.notices_open());
    let again = stream(&rig);
    rig.scaler.tick().await;
    again.settled();
    assert!(rig.scaler.notices_open());
    assert_eq!(rig.world().notices_asked, 2);
    again.reclaims("i-9");
    assert!(
        rig.log().contains(&"Cordon:js-a".to_owned()),
        "{:?}",
        rig.log()
    );
}

/// Asserts that the reader ends once the scaler is dropped: a notice that comes after it
/// is not acted on and the reader drops the stream, and a stream that ends after it is
/// dropped as well. It catches a reader that keeps the scaler alive, or that acts for a
/// scaler that is gone.
#[tokio::test]
async fn the_reader_ends_with_the_scaler() {
    let (rig, provider) = registered().await;
    let world = rig.world.clone();
    drop(rig);
    let notice = ProviderEvent::Reclaimed(HostRef("i-9".to_owned()));
    provider.send.as_ref().unwrap().send(notice).unwrap();
    provider.settled();
    assert!(world.lock().unwrap().log.is_empty());

    let (rig, mut provider) = registered().await;
    drop(rig);
    provider.ends();
}

/// Asserts that a reader whose tick panics (the provider's list panics) closes the stream,
/// and that the next tick opens a new one whose notices are acted on. It catches a reader
/// that dies and leaves the stream counted open, which would end the notices for good.
#[tokio::test]
async fn a_reader_whose_tick_panics_is_replaced_at_the_next_tick() {
    let (rig, provider) = registered().await;
    rig.world().list_panics = true;
    let notice = ProviderEvent::Reclaimed(HostRef("i-9".to_owned()));
    provider.send.as_ref().unwrap().send(notice).unwrap();
    provider.settled();
    assert!(!rig.scaler.notices_open());
    rig.world().list_panics = false;
    let again = stream(&rig);
    rig.scaler.tick().await;
    again.settled();
    assert!(rig.scaler.notices_open());
    again.reclaims("i-9");
    assert!(
        rig.log().contains(&"revoke:js-a".to_owned()),
        "{:?}",
        rig.log()
    );
}

/// Asserts that a notice is matched only to a record of the scaler's own pool: a record of
/// another pool (one taken out of the file, whose records the scaler still holds) with the
/// same provider id is not cordoned or released, and the notice is counted as ignored. It
/// catches a mapping that crosses pools.
#[tokio::test]
async fn a_reclaim_is_matched_only_in_the_scalers_pool() {
    let mut other = record("js-o", Phase::Registered);
    other.pool = "old".to_owned();
    let rig = rig(records(vec![other]), live());
    rig.world().hosts = vec![view("js-o")];
    let provider = stream(&rig);
    rig.scaler.tick().await;
    provider.settled();
    rig.world().log.clear();
    provider.reclaims("i-9");
    assert!(rig.log().is_empty(), "{:?}", rig.log());
    assert_eq!(rig.scaler.reclaims_ignored(), 1);
    assert_eq!(rig.world().hosts[0].cordon_until, None);
}
