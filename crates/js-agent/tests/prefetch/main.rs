//! The prefetcher: pulls run on a runtime of its own, and the caller's questions never
//! wait for one. The pull functions here are gated on a `Notify` the test releases, so no
//! test measures time; a guard thread ends the process if a test hangs.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use js_agent::lifecycle::{Prefetcher, Prepared, PullFuture, Ticket};
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
    let Prepared::Pending(ticket) = prefetcher.prepare(A, None) else {
        panic!("not pending");
    };
    assert_eq!(prefetcher.prepare(A, None), Prepared::Pending(ticket));
    assert_eq!(
        prefetcher.prepare(A, Some(ticket)),
        Prepared::Pending(ticket)
    );
    assert_eq!(pulls.load(Ordering::SeqCst), 1);
    gate.notify_one();
    assert_eq!(prefetcher.wait(A), Prepared::Ready);
    assert_eq!(prefetcher.prepare(A, None), Prepared::Ready);
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
    let Prepared::Pending(ticket) = prefetcher.prepare(A, None) else {
        panic!("not pending");
    };
    assert_eq!(prefetcher.wait(B), Prepared::Ready);
    assert_eq!(prefetcher.prepare(A, None), Prepared::Pending(ticket));
    gate.notify_one();
    assert_eq!(prefetcher.wait(A), Prepared::Ready);
}

/// A failed pull is reported to every call that names its ticket (every run that waited on
/// it), not once: two waiters both hear `Failed`, with the reason. A call that names no
/// ticket starts a new pull, which can succeed.
#[test]
fn a_failed_pull_is_reported_to_every_waiter_and_a_new_call_pulls_again() {
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
    let Prepared::Pending(ticket) = prefetcher.prepare(A, None) else {
        panic!("not pending");
    };
    assert_eq!(prefetcher.wait(A), failed("registry down"));
    assert_eq!(prefetcher.prepare(A, Some(ticket)), failed("registry down"));
    assert_eq!(prefetcher.prepare(A, Some(ticket)), failed("registry down"));
    assert_eq!(pulls.load(Ordering::SeqCst), 1);
    assert_eq!(prefetcher.wait(A), Prepared::Ready);
    assert_eq!(pulls.load(Ordering::SeqCst), 2);
}

/// A failure is not reported to a ticket of another pull: a caller that names an older
/// ticket than the failed pull's starts a new pull.
#[test]
fn a_failure_is_reported_only_to_the_ticket_of_the_pull_that_failed() {
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
    // The ticket of some other pull: not the failed one's.
    assert!(matches!(
        prefetcher.prepare(A, Some(Ticket(u64::MAX))),
        Prepared::Pending(_)
    ));
    assert_eq!(pulls.load(Ordering::SeqCst), 2);
}

/// `forget` makes the next call pull again, whether the image was pulled or is still being
/// pulled; the end of a pull that was let go is ignored. One worker thread runs the pulls
/// in order, and the test hears that the let-go pull's future has ended (it sends from its
/// last line) and then makes one more round trip through that thread, so the let-go pull's
/// result has been offered before the check.
#[test]
fn a_forgotten_image_is_pulled_again_and_a_pull_let_go_is_ignored() {
    guard();
    let (pulls, gate) = (Arc::new(AtomicUsize::new(0)), Arc::new(Notify::new()));
    let (ended, ended_rx) = std::sync::mpsc::channel();
    let (counted, held) = (Arc::clone(&pulls), Arc::clone(&gate));
    let prefetcher = Prefetcher::new(1, move |_| -> PullFuture {
        let first = counted.fetch_add(1, Ordering::SeqCst) == 0;
        let (held, ended) = (Arc::clone(&held), ended.clone());
        Box::pin(async move {
            if first {
                held.notified().await;
                ended.send(()).expect("the test listens");
                Err("late and wrong".to_owned())
            } else {
                Ok(())
            }
        })
    })
    .expect("a runtime");
    assert!(matches!(prefetcher.prepare(A, None), Prepared::Pending(_)));
    prefetcher.forget(A);
    // The second pull answers at once; the first, let go, ends later and must not undo it.
    assert_eq!(prefetcher.wait(A), Prepared::Ready);
    gate.notify_one();
    ended_rx.recv().expect("the let-go pull ends");
    assert_eq!(prefetcher.wait(B), Prepared::Ready);
    assert_eq!(prefetcher.prepare(A, None), Prepared::Ready);
    prefetcher.forget(A);
    assert!(matches!(prefetcher.prepare(A, None), Prepared::Pending(_)));
    assert_eq!(pulls.load(Ordering::SeqCst), 4);
}

/// A prefetcher needs a thread to pull on: none is an error, not a panic.
#[test]
fn no_worker_thread_is_an_error() {
    let refused = Prefetcher::new(0, |_| -> PullFuture { Box::pin(async { Ok(()) }) })
        .err()
        .expect("refused");
    assert_eq!(refused.kind(), std::io::ErrorKind::InvalidInput);
}

/// Dropping the prefetcher inside an async task is fine: it stops its runtime without
/// waiting (tokio panics when a runtime is dropped there).
#[tokio::test]
async fn it_can_be_dropped_inside_an_async_task() {
    let prefetcher = Prefetcher::new(1, |_| -> PullFuture {
        Box::pin(async {
            std::future::pending::<()>().await;
            Ok(())
        })
    })
    .expect("a runtime");
    assert!(matches!(prefetcher.prepare(A, None), Prepared::Pending(_)));
    drop(prefetcher);
}

/// A pull function that panics when called leaves the image as it was: the panic reaches
/// the caller, and the next call, whose function works, pulls it.
#[test]
fn a_pull_function_that_panics_leaves_no_trace() {
    guard();
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&calls);
    let prefetcher = Prefetcher::new(1, move |_| -> PullFuture {
        assert!(
            counted.fetch_add(1, Ordering::SeqCst) > 0,
            "the first call panics"
        );
        Box::pin(async { Ok(()) })
    })
    .expect("a runtime");
    let first =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| prefetcher.prepare(A, None)));
    assert!(first.is_err());
    assert_eq!(prefetcher.wait(A), Prepared::Ready);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
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
