//! The hang guard of a test that waits for several tasks says which ones it waits for.

use std::time::Duration;

use crate::support::guard_tasks;

/// Asserts that when the guard's bound is hit, the panic names the labelled tasks that
/// have not finished and does not name the one that has. Time is paused, so the bound
/// passes without waiting. It catches a guard that reports only that it timed out.
#[tokio::test(start_paused = true)]
async fn a_guard_that_fires_names_the_unfinished_tasks() {
    let done = tokio::spawn(async {});
    let stuck = tokio::spawn(std::future::pending::<()>());
    let also = tokio::spawn(async { tokio::time::sleep(Duration::from_secs(3600)).await });
    let waiting = tokio::spawn(guard_tasks(vec![
        ("finished-one", done),
        ("stuck-one", stuck),
        ("slow-one", also),
    ]));
    let payload = waiting.await.unwrap_err().into_panic();
    let message = payload.downcast_ref::<String>().unwrap();
    assert!(message.contains("stuck-one"), "{message}");
    assert!(message.contains("slow-one"), "{message}");
    assert!(!message.contains("finished-one"), "{message}");
}

/// Asserts that tasks that all finish pass the guard, and a task that panicked fails it
/// with that task's own panic.
#[tokio::test]
async fn finished_tasks_pass_and_a_failed_task_fails_the_guard() {
    let tasks = vec![
        ("one", tokio::spawn(async {})),
        ("two", tokio::spawn(async {})),
    ];
    guard_tasks(tasks).await;
    let failing = tokio::spawn(guard_tasks(vec![(
        "failing",
        tokio::spawn(async { panic!("task blew up") }),
    )]));
    assert!(failing.await.unwrap_err().is_panic());
}
