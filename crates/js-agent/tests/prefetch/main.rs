//! The prefetcher: pulls run on a runtime of its own, and the caller's questions never
//! wait for one. The pull functions here are gated on a `Notify` the test releases, so no
//! test measures time; a guard thread ends the process if a test hangs.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use js_agent::lifecycle::{Prefetcher, Prepared, PullFuture};
use tokio::sync::Notify;

const A: &str =
    "registry.example/a@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const B: &str =
    "registry.example/b@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

/// Ends the process with a message if a test is still running after two minutes: a hang
/// guard, never a measurement.
fn guard() {
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(120));
        eprintln!("a prefetch test hung");
        std::process::exit(101);
    });
}

fn failed(text: &str) -> Prepared {
    Prepared::Failed(js_agent::lifecycle::RuntimeError::new(text))
}

fn boom() -> Result<(), String> {
    panic!("boom")
}

/// Two runs that name one image pull it once, however many ask while it is under way and
/// after: the pull function is called once, and every ask is answered at once, `Pending`
/// until the pull ends and `Ready` after.
#[test]
fn two_runs_naming_one_image_pull_it_once() {
    guard();
    let (pulls, gate) = (Arc::new(AtomicUsize::new(0)), Arc::new(Notify::new()));
    let (counted, held) = (Arc::clone(&pulls), Arc::clone(&gate));
    let prefetcher = Prefetcher::new(2, move |_| -> PullFuture {
        counted.fetch_add(1, Ordering::SeqCst);
        let held = Arc::clone(&held);
        Box::pin(async move {
            held.notified().await;
            Ok(())
        })
    })
    .expect("a runtime");
    assert_eq!(prefetcher.prepare(A), Prepared::Pending);
    assert_eq!(prefetcher.prepare(A), Prepared::Pending);
    assert_eq!(pulls.load(Ordering::SeqCst), 1);
    gate.notify_one();
    assert_eq!(prefetcher.wait(A), Prepared::Ready);
    assert_eq!(prefetcher.prepare(A), Prepared::Ready);
    assert_eq!(prefetcher.wait(A), Prepared::Ready);
    assert_eq!(pulls.load(Ordering::SeqCst), 1);
}

/// A pull under way blocks neither the caller nor the pull of another image: with the
/// first held, the second is fetched and answered `Ready`, and the first is still
/// `Pending` until it is released.
#[test]
fn a_pull_under_way_blocks_neither_the_caller_nor_another_image() {
    guard();
    let gate = Arc::new(Notify::new());
    let held = Arc::clone(&gate);
    let prefetcher = Prefetcher::new(2, move |image| -> PullFuture {
        let held = Arc::clone(&held);
        Box::pin(async move {
            if image == A {
                held.notified().await;
            }
            Ok(())
        })
    })
    .expect("a runtime");
    assert_eq!(prefetcher.prepare(A), Prepared::Pending);
    assert_eq!(prefetcher.wait(B), Prepared::Ready);
    assert_eq!(prefetcher.prepare(A), Prepared::Pending);
    gate.notify_one();
    assert_eq!(prefetcher.wait(A), Prepared::Ready);
}

/// A failed pull is reported once, with its reason, to the call that finds it; the next
/// call for the image starts a new pull, which can succeed.
#[test]
fn a_failed_pull_is_reported_once_and_the_next_call_pulls_again() {
    guard();
    let pulls = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&pulls);
    let prefetcher = Prefetcher::new(1, move |_| -> PullFuture {
        let first = counted.fetch_add(1, Ordering::SeqCst) == 0;
        Box::pin(async move {
            if first {
                Err("registry down".to_owned())
            } else {
                Ok(())
            }
        })
    })
    .expect("a runtime");
    assert_eq!(prefetcher.wait(A), failed("registry down"));
    assert_eq!(pulls.load(Ordering::SeqCst), 1);
    assert_eq!(prefetcher.wait(A), Prepared::Ready);
    assert_eq!(pulls.load(Ordering::SeqCst), 2);
}

/// A pull that panics is a failed pull, not an image that stays `Pending` for ever.
#[test]
fn a_pull_that_panics_is_a_failed_pull() {
    guard();
    let prefetcher =
        Prefetcher::new(1, |_| -> PullFuture { Box::pin(async { boom() }) }).expect("a runtime");
    match prefetcher.wait(A) {
        Prepared::Failed(why) => assert!(why.to_string().contains("ended abnormally"), "{why}"),
        other => panic!("{other:?}"),
    }
}
