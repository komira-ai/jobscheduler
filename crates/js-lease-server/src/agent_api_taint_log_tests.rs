//! The log lines a host's taint leaves over a server whose bindings are kept in the state
//! store: stored from a heartbeat, cleared by the agent's heartbeat or by the operator, and
//! put back at the hello of a host the registry does not know. The rig is the one of the
//! parent module.

use super::*;

/// The taint a host reports: a residue of `check`, left by the run `run`.
fn residue(check: &str, run: &str) -> Value {
    json!({ "reason": "residue", "check": check, "run_id": run })
}

/// The field text that follows `message` in each line of `lines` that says it.
fn said(lines: &Lines, message: &str) -> Vec<String> {
    let text = lines.take();
    let found = text.lines().filter_map(|line| line.split_once(message));
    found.map(|(_, fields)| fields.to_owned()).collect()
}

/// Asserts that a heartbeat's taint is logged once, as info, when it is committed, with the
/// host, reason, check and run id; and that a heartbeat whose commit fails logs no such line
/// (the taint is not stored). It catches a taint stored without a word, which leaves an
/// operator no way to tell from the log why a host takes no job, and a line logged before
/// the commit that may then fail.
#[tokio::test]
async fn a_heartbeat_taint_is_logged_when_it_is_committed() {
    let rig = rig("taint_log_stored");
    let welcome = rig.pooled("host-a", "burst").await.unwrap();
    let taint = residue("tmp-files", "r1");
    rig.store.fail_next();
    let lines = Lines::default();
    let capture = capture(&lines);
    let failed = rig.beat((&rig.pool, None), "host-a", &welcome, taint.clone());
    assert!(failed.await.is_err());
    let message = "a host's taint is stored from its heartbeat";
    let none = said(&lines, message);
    assert!(none.is_empty(), "{none:?}");
    let reply = rig.beat((&rig.pool, None), "host-a", &welcome, taint).await;
    assert!(reply.unwrap().taint_ack);
    drop(capture);
    let stored = said(&lines, message);
    let fields = " host=host-a reason=\"residue\" check=\"tmp-files\" run_id=\"r1\"";
    assert_eq!(stored, [fields]);
}

/// Asserts that a heartbeat that reports its taint cleared is logged once, as info, after the
/// clear is committed, with the host, the reason and `by=agent`. It catches a clear that
/// leaves no line, so that a host that takes jobs again cannot be told from one that never
/// had a taint.
#[tokio::test]
async fn a_taint_the_agent_clears_is_logged_with_its_reason() {
    let rig = rig("taint_log_agent_clear");
    let welcome = rig.pooled("host-a", "burst").await.unwrap();
    let taint = residue("tmp-files", "r1");
    let reply = rig.beat((&rig.pool, None), "host-a", &welcome, taint.clone());
    assert!(reply.await.unwrap().taint_ack);
    let lines = Lines::default();
    let capture = capture(&lines);
    let cleared = rig.beat_clearing("host-a", &welcome, (taint, Value::Null));
    assert!(cleared.await.unwrap().taint_cleared_ack);
    drop(capture);
    let cleared = said(&lines, "a host's taint is cleared");
    assert_eq!(cleared, [" host=host-a reason=\"residue\" by=\"agent\""]);
}

/// Asserts that the operator's clear of a host's taint is logged once, as info, with the
/// host, the reason and `by=operator`, and that a clear of a host that has no taint logs
/// nothing. It catches an operator's clear that leaves no line, and a line for a clear that
/// changed nothing.
#[tokio::test]
async fn a_taint_the_operator_clears_is_logged_with_its_reason() {
    let rig = rig("taint_log_operator_clear");
    let welcome = rig.pooled("host-a", "burst").await.unwrap();
    let taint = residue("tmp-files", "r1");
    let reply = rig.beat((&rig.pool, None), "host-a", &welcome, taint);
    assert!(reply.await.unwrap().taint_ack);
    let host = HostId::new("host-a").unwrap();
    let lines = Lines::default();
    let capture = capture(&lines);
    assert!(rig.agents.clear_taint(&host).await.unwrap().is_some());
    assert_eq!(rig.agents.clear_taint(&host).await.unwrap(), None);
    drop(capture);
    let cleared = said(&lines, "a host's taint is cleared");
    assert_eq!(cleared, [" host=host-a reason=\"residue\" by=\"operator\""]);
}

/// Asserts that the hello of a host the registry does not know, whose stored binding holds a
/// taint and a cordon, logs once, as info, what is put back (host, `when=hello`, the taint's
/// reason, check and run id, and the cordon's end), and that the hello of a host with a clean
/// stored binding logs no such line. It catches a taint put back from another server's
/// store without a word.
#[tokio::test]
async fn a_taint_put_back_at_a_hello_is_logged() {
    let first = rig("taint_log_hello_first");
    let welcome = first.pooled("host-a", "burst").await.unwrap();
    let taint = residue("tmp-files", "r1");
    let reply = first.beat((&first.pool, None), "host-a", &welcome, taint);
    assert!(reply.await.unwrap().taint_ack);
    let host = HostId::new("host-a").unwrap();
    let lease = Duration::from_secs(3600);
    let cordoned = first.agents.cordon(&host, lease, Asker::Unnamed).await;
    cordoned.unwrap();
    first.pooled("host-b", "burst").await.unwrap();

    let second = Rig::second(&first, "taint_log_hello_second");
    let lines = Lines::default();
    let capture = capture(&lines);
    second.pooled("host-a", "burst").await.unwrap();
    second.pooled("host-b", "burst").await.unwrap();
    drop(capture);
    let until = second.held("host-a").unwrap().cordon_until_ms().unwrap();
    let put = said(&lines, "a host's stored taint or cordon is put back");
    let fields = " host=host-a when=\"hello\" reason=\"residue\" check=\"tmp-files\" run_id=\"r1\"";
    assert_eq!(put, [format!("{fields} cordon_until_ms={until}")]);
}
