//! The pool's cap of a server that holds the job tables, counted over every server's rows in
//! the store they share: a hello of a host new to a full pool is refused before anything of it
//! is written, a known host is not counted, and a pool whose rows cannot be read refuses the
//! hello with an error the agent retries. Two servers share one store and one manual clock;
//! nothing sleeps.

use js_agent_wire::Welcome;
use js_registry::WholeNodeConfig;
use js_types::HostId;
use serde_json::json;

use super::job_rig::{Rig, START_MS, whole_over, whole_rig};
use super::{Bytes, Refused, serve_hello};
use crate::api::rig::{Lines, process_sink};

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

/// The hello of `name` through the route of `rig`'s server, as an agent of the pool with the
/// certificate of `name`.
pub(crate) async fn hello(rig: &Rig, name: &str) -> Result<Welcome, Refused> {
    let capabilities = json!({ "per-job": "true", "arch": "x86_64" });
    let said = json!({
        "host_id": name,
        "pool": "burst",
        "agent_version": "0.1.0",
        "shape": { "cpu_millis": 8000, "memory_bytes": 17179869184u64, "gpus": 0 },
        "capabilities": capabilities,
        "phase": "serving",
    });
    let body = Ok(Bytes::from(said.to_string()));
    let sent = serve_hello(&rig.agents, name, body, &|| ());
    sent.await.map(|axum::Json(welcome)| welcome)
}

/// The status and error code of a refused hello; panics when the hello was admitted.
pub(crate) fn refusal(answer: Result<Welcome, Refused>) -> (u16, &'static str) {
    let refused = answer.expect_err("the hello was refused");
    (refused.status.as_u16(), refused.error)
}

/// Asserts that a first hello into a pool already full over the rows of both servers is
/// refused 403 `pool-full` by a server whose registry holds no host, before anything of the
/// hello is written: the registry holds no host of that name, no row is written, and the pool's
/// four rows stand. It catches a cap counted over this server's own hosts, and a refusal that
/// leaves the registry's admission behind.
#[tokio::test]
async fn a_first_hello_into_a_pool_full_over_the_rows_is_refused_before_anything_is_written() {
    let (mut first, second) = two_servers("rows_full");
    for name in ["host-1", "host-2", "host-3", "host-4"] {
        first.join(name, false).await;
    }
    let commits = second.commits();
    let refused = hello(&second, "host-5").await;
    assert_eq!(refusal(refused), (403, "pool-full"));
    assert!(second.agents.registry.own_host(&host("host-5")).is_none());
    assert_eq!(second.jobs.read_node(&host("host-5")).unwrap(), None);
    assert_eq!(second.jobs.nodes().unwrap().1.len(), 4);
    assert_eq!(second.commits(), commits, "nothing was written");
}

/// Asserts that a host that has a row is not counted against the cap: with the pool full, the
/// hello of one of its four hosts that fell silent is admitted at epoch 2 and its row moves to
/// epoch 2. It catches a cap applied to a host that is not new to the pool, which would refuse
/// a live host's restart.
#[tokio::test]
async fn a_known_hosts_hello_is_not_capped() {
    let (mut first, _second) = two_servers("rows_known");
    for name in ["host-1", "host-2", "host-3", "host-4"] {
        first.join(name, false).await;
    }
    first.advance(first.agents.registry.longest_grace() * 2);
    let again = hello(&first, "host-1").await.unwrap();
    assert_eq!(again.epoch, 2);
    let row = first.jobs.read_node(&host("host-1")).unwrap().unwrap();
    assert_eq!(row.record.epoch, 2);
}

/// Asserts that a hello judged against a pool whose rows cannot be read is answered 503
/// `state-write-failed`, logged as such, with no host held and no row written, and that the
/// agent's next hello is admitted: the read that fails is found by failing each read of the
/// hello in turn until the pool's count is the one that fails. It catches a hello admitted on
/// less than the rows (past the cap), and a failed read answered as a full pool, which the
/// agent takes as fatal.
#[tokio::test]
async fn a_pool_whose_rows_cannot_be_read_refuses_the_hello_until_they_can() {
    process_sink();
    let lines = Lines::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(lines.clone())
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let first = whole_rig("rows_unread_pool", rules(), None);
    let mut found = false;
    for reads in 0..32 {
        first.store.fail_read_after(reads);
        let answer = hello(&first, "host-1").await;
        first.store.fail_read_after(usize::MAX);
        if !lines.take().contains("its pool's rows could not be read") {
            continue;
        }
        assert_eq!(refusal(answer), (503, "state-write-failed"));
        assert!(first.agents.registry.own_host(&host("host-1")).is_none());
        assert_eq!(first.jobs.read_node(&host("host-1")).unwrap(), None);
        found = true;
        break;
    }
    assert!(found, "no read of the hello was the pool's count");
    let admitted = hello(&first, "host-1").await.unwrap();
    assert_eq!(admitted.epoch, 1);
}
