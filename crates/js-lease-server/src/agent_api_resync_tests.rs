//! A heartbeat whose taint commit loses to another server over the same store: the store holds
//! another taint than this server's registry does, so the conditional commit is refused, and
//! the registry takes the stored taint before the refusal is answered, so the agent's repeat
//! is judged on what the store holds. The rig is the one of the parent module.

use super::*;

/// The taint `run` reports: a residue of the check `daemonised-child`.
fn residue(run: &str) -> Value {
    json!({ "reason": "residue", "check": "daemonised-child", "run_id": run })
}

/// The taint [`residue`] names, as the registry holds it.
fn held_residue(run: &str) -> Taint {
    Taint {
        reason: "residue".to_owned(),
        check: Some("daemonised-child".to_owned()),
        run_id: Some(run.to_owned()),
    }
}

/// The heartbeat of `host` in the session `welcome` opened that says whether the agent holds a
/// taint, `tainted`, and reports none, sent to `rig`.
async fn says(
    rig: &Rig,
    host: &str,
    welcome: &Welcome,
    tainted: bool,
) -> Result<HeartbeatReply, Refused> {
    let beat = json!({
        "host_id": host,
        "epoch": welcome.epoch,
        "session": welcome.session,
        "phase": "serving",
        "tainted": tainted,
    });
    beat_as(&rig.agents, host, Bytes::from(beat.to_string())).await
}

/// Asserts that `failed` is the refusal of a taint commit: 503 `state-write-failed`.
fn assert_unsaved(failed: Result<HeartbeatReply, Refused>) {
    let failed = failed.expect_err("the commit is refused");
    assert_eq!(
        (failed.status.as_u16(), failed.error),
        (503, "state-write-failed")
    );
}

/// The field text that follows the line "the taint another server stored is taken into the
/// registry after a refused commit" in each line of `lines` that says it.
fn taken(lines: &Lines) -> Vec<String> {
    let message =
        "the taint another server stored is taken into the registry after a refused commit";
    let text = lines.take();
    let found = text.lines().filter_map(|line| line.split_once(message));
    found.map(|(_, fields)| fields.to_owned()).collect()
}

/// Two servers over one store whose host is known to both, as a start-first update leaves
/// them; the first has been told `run` as the host's taint, so the store holds it and the
/// second's registry holds none. Answers the first, the second and the welcome of each.
async fn first_stored(name: &str, run: &str) -> (Rig, Rig, Welcome, Welcome) {
    let first = rig(&format!("{name}_first"));
    let at_first = first.hello("host-a", "burst").await.unwrap();
    let second = Rig::second(&first, &format!("{name}_second"));
    let at_second = second.hello("host-a", "burst").await.unwrap();
    let told = first.beat("host-a", &at_first, residue(run));
    assert!(told.await.unwrap().taint_ack);
    assert_eq!(first.binding("host-a")["taint_run_id"], run);
    assert!(second.held("host-a").unwrap().taint().is_none());
    (first, second, at_first, at_second)
}

/// Asserts that a beat that says the agent is tainted, sent to a server whose registry holds no
/// taint for the host while the store holds one the other server stored, is refused 503
/// `state-write-failed` with nothing written, that the registry then holds the stored taint
/// (the host's listing and binding carry it, one line says it was taken, naming the host and the
/// reason), and that the repeat is answered with the stored taint as `host_taint` and no
/// acknowledgement, committing nothing. It catches a registry left behind the store, which
/// refuses every beat of the agent that reaches it the same way.
#[tokio::test]
async fn a_registry_behind_the_store_takes_the_stored_taint_when_its_commit_is_refused() {
    let (first, second, _, at_second) = first_stored("resync_taint", "r1").await;
    let before = first.commits();
    let stored = first.binding("host-a");

    let lines = Lines::default();
    let capture = capture(&lines);
    assert_unsaved(says(&second, "host-a", &at_second, true).await);
    let said = taken(&lines);
    assert_eq!(first.commits(), before, "nothing was written");
    assert_eq!(first.binding("host-a"), stored);
    let held = second.held("host-a").unwrap();
    assert_eq!(held.taint(), Some(&held_residue("r1")));
    assert_eq!(said, [" host=host-a reason=\"residue\""]);

    let reply = says(&second, "host-a", &at_second, true).await.unwrap();
    drop(capture);
    assert_eq!(reply.host_taint, Some(held_residue("r1")));
    assert!(!reply.taint_ack && !reply.taint_cleared_ack);
    assert_eq!(first.commits(), before, "the repeat writes nothing");
    assert_eq!(first.binding("host-a"), stored);
    assert!(taken(&lines).is_empty(), "the repeat takes nothing");
}

/// Asserts that a beat that says the agent holds no taint, sent to a server whose registry
/// still holds the taint that the other server's clear ended in the store, is refused 503
/// `state-write-failed` with nothing written, that the registry then holds none (the listing
/// and binding carry none, one line says the taint was taken, naming the host only), and that
/// the repeat is answered with no taint, committing nothing. It catches a registry left holding
/// a taint the store no longer does, which refuses every clean beat that reaches it.
#[tokio::test]
async fn a_registry_behind_the_store_ends_the_taint_the_store_no_longer_holds() {
    let first = rig("resync_end_first");
    let at_first = first.hello("host-a", "burst").await.unwrap();
    let told = first.beat("host-a", &at_first, residue("r1"));
    assert!(told.await.unwrap().taint_ack);
    let second = Rig::second(&first, "resync_end_second");
    let at_second = second.hello("host-a", "burst").await.unwrap();
    let held = second.held("host-a").unwrap();
    assert_eq!(held.taint(), Some(&held_residue("r1")));
    let ended = says(&first, "host-a", &at_first, false).await.unwrap();
    assert_eq!(ended.host_taint, None);
    assert_eq!(first.binding("host-a")["taint_reason"], Value::Null);
    assert!(second.held("host-a").unwrap().taint().is_some());
    let (before, stored) = (first.commits(), first.binding("host-a"));

    let lines = Lines::default();
    let capture = capture(&lines);
    assert_unsaved(says(&second, "host-a", &at_second, false).await);
    let said = taken(&lines);
    assert_eq!(first.commits(), before, "nothing was written");
    assert!(second.held("host-a").unwrap().taint().is_none());
    assert_eq!(said.len(), 1, "{said:?}");
    assert!(said[0].contains("host=host-a") && !said[0].contains("reason"));

    let reply = says(&second, "host-a", &at_second, false).await.unwrap();
    drop(capture);
    assert_eq!(reply.host_taint, None);
    assert_eq!(first.commits(), before, "the repeat writes nothing");
    assert_eq!(first.binding("host-a"), stored);
    assert!(taken(&lines).is_empty(), "the repeat takes nothing");
}

/// Asserts that a read of the store that fails after the commit was refused leaves the registry
/// as it was: the beat is refused, the registry still holds no taint and nothing says one was
/// taken; and that the next beat, with the store working, is refused again and takes the taint,
/// and the one after it is answered. It catches a resync that guesses from a store it could not
/// read, and one that is not tried again at the next beat.
#[tokio::test]
async fn a_failed_read_of_the_store_leaves_the_registry_as_it_was() {
    let (first, second, _, at_second) = first_stored("resync_read", "r1").await;

    let lines = Lines::default();
    let capture = capture(&lines);
    first.store.fail_read_after(1);
    assert_unsaved(says(&second, "host-a", &at_second, true).await);
    assert!(second.held("host-a").unwrap().taint().is_none());
    assert!(taken(&lines).is_empty());

    assert_unsaved(says(&second, "host-a", &at_second, true).await);
    assert_eq!(taken(&lines).len(), 1);
    let reply = says(&second, "host-a", &at_second, true).await.unwrap();
    drop(capture);
    assert_eq!(reply.host_taint, Some(held_residue("r1")));
}

/// Asserts that a store that holds no row of the host leaves the registry as it was: the
/// beat that brings a taint is refused, the registry still holds no taint and nothing says
/// one was taken. It catches a resync that makes up a taint, or an end, for a host the store
/// does not know.
#[tokio::test]
async fn a_host_the_store_holds_no_row_of_keeps_the_registry_as_it_was() {
    let (first, second, _, at_second) = first_stored("resync_row", "r1").await;
    let gone = Txn::new().delete("hosts/binding/host-a", Cond::Any);
    first.store.inner().txn(gone).unwrap().unwrap();
    assert_eq!(first.binding("host-a"), Value::Null);

    let lines = Lines::default();
    let capture = capture(&lines);
    assert_unsaved(says(&second, "host-a", &at_second, true).await);
    drop(capture);
    assert!(second.held("host-a").unwrap().taint().is_none());
    assert!(taken(&lines).is_empty());
}
