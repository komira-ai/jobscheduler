//! The taint a server holds for a host whose agent says it holds none (`tainted` false): a
//! taint stored from an earlier session, and read back at a restart or kept through a hello,
//! ends at the agent's first beat, committed first and logged. The rig is the one of the parent
//! module.

use super::*;

/// The taint a host reports: a residue of `kernel-module-changed`, left by the run `r1`.
fn residue() -> Value {
    json!({ "reason": "residue", "check": "kernel-module-changed", "run_id": "r1" })
}

/// The field text that follows `message` in each line of `lines` that says it.
fn said(lines: &Lines, message: &str) -> Vec<String> {
    let text = lines.take();
    let found = text.lines().filter_map(|line| line.split_once(message));
    found.map(|(_, fields)| fields.to_owned()).collect()
}

impl Rig {
    /// The heartbeat of `host` in the session `welcome` opened that says whether the agent holds
    /// a taint, `tainted`.
    async fn beat_saying(
        &self,
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
        beat_as(&self.agents, host, Bytes::from(beat.to_string())).await
    }

    /// The taint the registry's listing shows for `host`.
    fn listed_taint(&self, host: &str) -> Option<Taint> {
        let id = HostId::new(host).unwrap();
        let hosts = self.agents.registry.hosts();
        hosts.into_iter().find(|view| view.id == id)?.taint
    }
}

/// Asserts that the first beat of an agent that says it holds no taint ends the taint the
/// server stored from its earlier session, whether the server restarted over the store (the
/// taint is put back at the hello) or the agent said hello again to the same server with a new
/// epoch: the stored binding no longer holds the taint, the registry's listing and binding
/// hold none, the reply names no taint (and is no clear acknowledgement), and one line, "a
/// host's taint is cleared", says `by="agent-holds-none"` with the reason. It catches a taint
/// kept for good after the agent that made it holds none, which only the operator's call
/// could end.
#[tokio::test]
async fn an_agent_that_holds_no_taint_ends_the_taint_stored_from_its_earlier_session() {
    let first = rig("stale_taint_first");
    let welcome = first.hello("host-a", "burst").await.unwrap();
    let reply = first.beat("host-a", &welcome, residue());
    assert!(reply.await.unwrap().taint_ack);
    assert_eq!(first.binding("host-a")["taint_reason"], "residue");

    let second = Rig::second(&first, "stale_taint_second");
    let hello = second.hello("host-a", "burst").await.unwrap();
    assert_eq!(second.listed_taint("host-a").unwrap().reason, "residue");
    let held = second.beat_saying("host-a", &hello, true).await.unwrap();
    assert_eq!(
        held.host_taint.map(|taint| taint.reason).as_deref(),
        Some("residue")
    );
    assert_eq!(second.binding("host-a")["taint_reason"], "residue");

    // The agent restarts: its last session goes silent past the grace, so the hello is taken.
    second.clock.advance(Duration::from_secs(60));
    let again = second.hello("host-a", "burst").await.unwrap();
    assert_ne!(again.epoch, hello.epoch);
    let lines = Lines::default();
    let capture = capture(&lines);
    let reply = second.beat_saying("host-a", &again, false).await.unwrap();
    drop(capture);
    assert_eq!(reply.host_taint, None);
    assert!(!reply.taint_ack && !reply.taint_cleared_ack);
    for key in ["taint_reason", "taint_check", "taint_run_id"] {
        assert_eq!(second.binding("host-a")[key], Value::Null, "{key}");
    }
    assert!(second.held("host-a").unwrap().taint().is_none());
    assert_eq!(second.listed_taint("host-a"), None);
    let cleared = said(&lines, "a host's taint is cleared");
    assert_eq!(
        cleared,
        [" host=host-a reason=\"residue\" by=\"agent-holds-none\""]
    );
}

/// Asserts that the same server ends a stored taint when its agent says hello again with a new
/// epoch and then holds none: the hello keeps the taint, the first beat that says `tainted`
/// false commits the binding without it and ends it in the registry, one line says
/// `by="agent-holds-none"`, and a second such beat commits and logs nothing more. It catches
/// an end that waits for a restart of the server, and a line for a beat that changed nothing.
#[tokio::test]
async fn a_new_hello_keeps_the_taint_and_the_first_clean_beat_ends_it() {
    let rig = rig("stale_taint_hello");
    let welcome = rig.hello("host-a", "burst").await.unwrap();
    let reply = rig.beat("host-a", &welcome, residue());
    assert!(reply.await.unwrap().taint_ack);

    rig.clock.advance(Duration::from_secs(60));
    let again = rig.hello("host-a", "burst").await.unwrap();
    assert_ne!(again.epoch, welcome.epoch);
    assert_eq!(rig.listed_taint("host-a").unwrap().reason, "residue");
    let lines = Lines::default();
    let capture = capture(&lines);
    let before = rig.commits();
    let reply = rig.beat_saying("host-a", &again, false).await.unwrap();
    assert_eq!(reply.host_taint, None);
    assert_eq!(rig.commits(), before + 1, "one transaction");
    assert_eq!(rig.binding("host-a")["taint_reason"], Value::Null);
    assert_eq!(rig.listed_taint("host-a"), None);
    let quiet = rig.beat_saying("host-a", &again, false).await.unwrap();
    drop(capture);
    assert_eq!(quiet.host_taint, None);
    assert_eq!(rig.commits(), before + 1, "nothing more was written");
    let cleared = said(&lines, "a host's taint is cleared");
    assert_eq!(
        cleared,
        [" host=host-a reason=\"residue\" by=\"agent-holds-none\""]
    );
}

/// Asserts that a beat that says the agent holds no taint is answered with the error when the
/// store fails the commit, and keeps the taint, as the agent's report of a clear does: the
/// store, the registry's listing and binding hold the taint, nothing is logged as cleared, and
/// the same beat sent again once the store works ends it. It catches a taint forgotten in
/// memory before the store holds the clear (a restart would bring it back), and a line for a
/// clear that did not happen.
#[tokio::test]
async fn a_failed_commit_keeps_the_taint_an_agent_that_holds_none_would_end() {
    let rig = rig("stale_taint_fails");
    let welcome = rig.hello("host-a", "burst").await.unwrap();
    let reply = rig.beat("host-a", &welcome, residue());
    assert!(reply.await.unwrap().taint_ack);

    let lines = Lines::default();
    let capture = capture(&lines);
    rig.store.fail_next();
    let failed = rig.beat_saying("host-a", &welcome, false).await;
    let failed = failed.expect_err("the commit fails");
    assert_eq!(
        (failed.status.as_u16(), failed.error),
        (503, "state-write-failed")
    );
    assert_eq!(rig.binding("host-a")["taint_reason"], "residue");
    assert_eq!(rig.listed_taint("host-a").unwrap().reason, "residue");
    assert!(rig.held("host-a").unwrap().taint().is_some());
    assert!(said(&lines, "a host's taint is cleared").is_empty());

    let reply = rig.beat_saying("host-a", &welcome, false).await.unwrap();
    drop(capture);
    assert_eq!(reply.host_taint, None);
    assert_eq!(rig.binding("host-a")["taint_reason"], Value::Null);
    assert_eq!(rig.listed_taint("host-a"), None);
    let cleared = said(&lines, "a host's taint is cleared");
    assert_eq!(
        cleared,
        [" host=host-a reason=\"residue\" by=\"agent-holds-none\""]
    );
}
