//! The hello and the beat of a server that holds the job tables, against the host's `node/`
//! row in the store they share: the pool's cap counted over every server's rows, the withdrawal
//! of a hello the rows refused, and the refresh a beat makes. Two servers share one store and
//! one manual clock; nothing sleeps.

use js_agent_wire::{InstanceIdentity, Phase, Welcome};
use js_registry::{Credential, WholeNodeConfig};
use js_types::HostId;
use serde_json::json;

use super::job_rig::{FakeAgent, Rig, START_MS, whole_over, whole_rig};
use super::{Bytes, Refused, rows, serve_hello};
use crate::api::rig::{Lines, process_sink};
use crate::join_tokens::JoinRefusal;

/// The ttl of a one-time token.
const TTL: std::time::Duration = std::time::Duration::from_secs(600);

fn rules() -> WholeNodeConfig {
    WholeNodeConfig::dedicated(2)
}

fn host(name: &str) -> HostId {
    HostId::new(name).unwrap()
}

/// A server of a four-host pool, and a second server over its tables, as another process.
pub(crate) fn two_servers(name: &str) -> (Rig, Rig) {
    let first = whole_rig(&format!("{name}_first"), rules(), None);
    let second = whole_over(
        &format!("{name}_second"),
        (rules(), None),
        Some(&first),
        START_MS,
    );
    (first, second)
}

/// The hello of `name` through the route of `rig`'s server, as an agent of the pool; with
/// `token`, a one-time token for the instance `i-1`, which the hello names.
pub(crate) async fn hello(
    rig: &Rig,
    name: &str,
    token: Option<&Credential>,
) -> Result<Welcome, Refused> {
    let who = rig.agents.tokens.identify(&"j".repeat(40)).unwrap();
    let mut said = json!({
        "host_id": name,
        "pool": "burst",
        "agent_version": "0.1.0",
        "shape": { "cpu_millis": 8000, "memory_bytes": 17179869184u64, "gpus": 0 },
        "capabilities": { "per-job": "true", "arch": "x86_64" },
        "phase": "serving",
    });
    if token.is_some() {
        said["instance"] = json!({ "provider": "burst", "instance_id": "i-1" });
    }
    let body = Ok(Bytes::from(said.to_string()));
    let who = token.cloned().unwrap_or(who);
    let sent = serve_hello(&rig.agents, who, token.cloned(), body, &|| ());
    sent.await.map(|axum::Json(welcome)| welcome)
}

/// The status and error code of a refused hello; panics when the hello was admitted.
pub(crate) fn refusal(answer: Result<Welcome, Refused>) -> (u16, &'static str) {
    let refused = answer.expect_err("the hello was refused");
    (refused.status.as_u16(), refused.error)
}

/// A one-time token for the instance `i-1` of `rig`'s server, as the credential it presents.
fn token(rig: &Rig) -> Credential {
    let instance = InstanceIdentity {
        provider: "burst".to_owned(),
        instance_id: "i-1".to_owned(),
    };
    let minted = rig.agents.joins.mint(instance, TTL).unwrap();
    Credential::of_token(&minted.token)
}

/// Asserts that a first hello into a pool already full over the rows of both servers is
/// refused 403 `pool-full` by a server whose registry holds no host: the registry no longer
/// holds the host, no row is written, the one-time token's use is given back (the token serves
/// no host), and the pool's four rows stand. It catches a refusal that leaves the registry
/// admission behind (a host placed on that no server's row counts), or the token consumed.
#[tokio::test]
async fn a_first_hello_into_a_pool_full_over_the_rows_is_refused_and_withdrawn() {
    let (mut first, second) = two_servers("rows_full");
    for name in ["host-1", "host-2", "host-3", "host-4"] {
        first.join(name, false).await;
    }
    let who = token(&second);
    let refused = hello(&second, "host-5", Some(&who)).await;
    let refused = refused.expect_err("the pool is full over the rows");
    assert_eq!((refused.status.as_u16(), refused.error), (403, "pool-full"));
    assert!(second.agents.registry.own_host(&host("host-5")).is_none());
    assert_eq!(second.jobs.read_node(&host("host-5")).unwrap(), None);
    assert_eq!(second.jobs.nodes().unwrap().1.len(), 4);
    let unused = second.agents.joins.serves(&who, "host-5");
    assert_eq!(unused, Err(JoinRefusal::NotAdmitted));
}

/// Asserts that a host the registry already held is not counted against the cap: with the
/// pool full, the hello of one of its four hosts that fell silent again is admitted at epoch 2 and its row
/// moves to epoch 2, which the server notes. It catches a cap applied to a host that is not new
/// to the pool, which would refuse a live host's restart.
#[tokio::test]
async fn a_known_hosts_hello_is_not_capped() {
    let (mut first, _second) = two_servers("rows_known");
    for name in ["host-1", "host-2", "host-3", "host-4"] {
        first.join(name, false).await;
    }
    first.advance(first.agents.registry.longest_grace() * 2);
    let again = hello(&first, "host-1", None).await.unwrap();
    assert_eq!(again.epoch, 2);
    let row = first.jobs.read_node(&host("host-1")).unwrap().unwrap();
    assert_eq!(row.record.epoch, 2);
    assert_eq!(first.agents.node_epoch(&host("host-1")), Some(2));
}

/// Asserts that a hello whose row cannot be written is answered 503 `state-write-failed`, the
/// registry no longer holds the host, no row is there, and the agent's next hello is admitted.
/// It catches a host admitted by the registry without a row (placed on by this server alone),
/// and a failure answered as admitted.
#[tokio::test]
async fn a_hello_whose_row_cannot_be_written_is_refused_and_withdrawn() {
    let first = whole_rig("rows_unwritten", rules(), None);
    first.store.fail_read_after(0);
    let refused = hello(&first, "host-1", None).await;
    let refused = refused.expect_err("the row cannot be written");
    assert_eq!(
        (refused.status.as_u16(), refused.error),
        (503, "state-write-failed")
    );
    assert!(first.agents.registry.own_host(&host("host-1")).is_none());
    assert_eq!(first.jobs.read_node(&host("host-1")).unwrap(), None);
    let admitted = hello(&first, "host-1", None).await.unwrap();
    assert_eq!(admitted.epoch, 1);
    assert!(first.jobs.read_node(&host("host-1")).unwrap().is_some());
}

/// Asserts that a welcome for a host the registry does not hold writes no row and is passed
/// on: the registry owns the hello's judgment, and the row records only what it holds. It
/// catches a row written for a host this server does not hold.
#[tokio::test]
async fn a_welcome_for_a_host_the_registry_does_not_hold_writes_no_row() {
    let (first, _) = two_servers("rows_unheld_first");
    let welcome = hello(&first, "host-1", None).await.unwrap();
    let other = whole_rig("rows_unheld_other", rules(), None);
    let passed = rows::written_row(&other.agents, &host("host-1"), welcome).await;
    assert!(passed.is_ok());
    assert_eq!(other.jobs.read_node(&host("host-1")).unwrap(), None);
}

/// Asserts that a beat after another server's hello took the row (a later epoch) stops this
/// server refreshing: it noted epoch 1, the row is at epoch 2 after the other hello, and the
/// beat leaves the row as the other server wrote it and drops the note. It catches a beat that
/// writes over another server's session.
#[tokio::test]
async fn a_beat_after_another_servers_hello_leaves_the_row_alone() {
    let (mut first, second) = two_servers("rows_stale");
    first.join("host-1", false).await;
    assert_eq!(first.agents.node_epoch(&host("host-1")), Some(1));
    hello(&second, "host-1", None).await.unwrap();
    let taken = first.jobs.read_node(&host("host-1")).unwrap().unwrap();
    assert_eq!(taken.record.epoch, 2);
    let agent = FakeAgent::new("host-1");
    first.beat(&agent).await.unwrap();
    assert_eq!(first.agents.node_epoch(&host("host-1")), None);
    let after = first.jobs.read_node(&host("host-1")).unwrap().unwrap();
    assert_eq!(after, taken);
}

/// Asserts that a refresh the store fails is logged, naming the host, and the next beat writes
/// the row. It catches a failed refresh that is silent, or one that drops the row's epoch so
/// that the host is never refreshed again.
#[tokio::test]
async fn a_failed_refresh_is_logged_and_the_next_beat_writes_it() {
    process_sink();
    let lines = Lines::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(lines.clone())
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let mut rig = whole_rig("rows_refresh_fails", rules(), None);
    rig.join("host-1", false).await;
    let name = host("host-1");
    rig.store.fail_read_after(0);
    rows::beat_row(&rig.agents, &name, Phase::Serving);
    let text = lines.take();
    assert!(
        text.contains("the host's row was not refreshed; the next beat writes it"),
        "{text}"
    );
    assert_eq!(rig.agents.node_epoch(&name), Some(1));
    rig.advance(rig.agents.registry.refresh_every());
    let before = rig.jobs.read_node(&name).unwrap().unwrap();
    rows::beat_row(&rig.agents, &name, Phase::Serving);
    let after = rig.jobs.read_node(&name).unwrap().unwrap();
    assert!(after.record.heard_at > before.record.heard_at);
}
