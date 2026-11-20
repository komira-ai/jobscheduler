//! The host's binding row `hosts/binding/<host>` over servers that share one store and hold the
//! job tables: a hello is judged on the row it read and writes it at the version it read, a
//! save never overwrites a taint another server stored, and the cordon, the uncordon and the
//! clear of a taint take the host from the rows. Every interleaving is a hook of the store, and
//! nothing waits on time.

use js_agent_wire::Taint;
use js_state::{Cond, Txn, Versioned};
use js_types::cordon::{Asker, Cordon};

use super::*;
use crate::agents::CordonFault;

/// The taint a host reports.
fn residue() -> Value {
    json!({ "reason": "residue", "check": "tmp-files", "run_id": "r1" })
}

/// The binding row of `host`, as the store holds it.
fn row(world: &World, host: &str) -> Option<Versioned> {
    world
        .store
        .inner()
        .get(&format!("hosts/binding/{host}"))
        .unwrap()
}

/// The binding of `host` as JSON; `null` when the row is absent.
fn stored(world: &World, host: &str) -> Value {
    row(world, host).map_or(Value::Null, |row| {
        serde_json::from_slice(&row.value).unwrap()
    })
}

/// Has another server write the binding row of `host` just before the next commit: bound to
/// `credential`, with the taint `residue` when `tainted`.
fn rival_writes(world: &World, host: &str, credential: &Credential, tainted: bool) {
    let mut value = json!({ "version": 1, "credential": credential.to_hex() });
    if tainted {
        value["taint_reason"] = json!("residue");
    }
    let put = Txn::new().put(
        format!("hosts/binding/{host}"),
        value.to_string(),
        Cond::Any,
    );
    let store = world.store.inner();
    world.store.before_next(move || {
        store.txn(put).unwrap().unwrap();
    });
}

/// Asserts that a taint one server stored in a binding another server holds survives the other's
/// tick and its save. Server B holds `host-a` (its hello wrote the binding), server A stores a
/// taint for it, and a cordon row placed through the store is mirrored on B, so B's registry
/// holds a binding the store does not: B's save would have put that binding, without the taint,
/// at the version B read. The first save loses that version, leaves the row as it is (the taint
/// stays and the cordon is not written), answers that the store does not hold what the registry
/// has bound, and B's registry takes the taint; the next save writes the cordon with the taint
/// kept. It catches a save that puts at `Cond::Any` or at a version it did not read, which erases
/// the taint the other server stored (a tainted host offered jobs again).
#[tokio::test]
async fn a_taint_stored_through_one_server_survives_another_servers_save() {
    let world = World::new();
    let a = world.server("binding_taint_a", Kind::KEPT);
    let b = world.server("binding_taint_b", Kind::KEPT);
    let session = b.join("host-a").await;
    let reported = a.beat_with(&session, Some(1), Vec::new(), json!({ "taint": residue() }));
    assert!(reported.await.unwrap().taint_ack);
    assert_eq!(stored(&world, "host-a")["taint_reason"], "residue");
    let host = id("host-a");
    assert!(
        b.agents
            .registry
            .own_host(&host)
            .unwrap()
            .host
            .taint
            .is_none()
    );
    world
        .jobs
        .cordon_host(&host, Duration::from_secs(120), &Asker::Unnamed)
        .unwrap();

    let version = row(&world, "host-a").unwrap().version;
    b.agents.refresh().await;
    let kept = row(&world, "host-a").unwrap();
    assert_eq!(kept.version, version, "the row was not written over");
    assert_eq!(stored(&world, "host-a")["cordon_until_ms"], Value::Null);
    let taint = b.agents.registry.own_host(&host).unwrap().host.taint;
    assert_eq!(taint.map(|taint| taint.reason), Some("residue".to_owned()));

    assert!(b.agents.save_bindings().await, "the next save writes");
    let written = stored(&world, "host-a");
    assert_eq!(written["taint_reason"], "residue");
    assert_ne!(written["cordon_until_ms"], Value::Null);
}

/// Asserts that a hello refused for `host-bound` writes nothing: server A binds `host-a` to the
/// pool's credential, and a hello of the host to server B with a one-time token (another
/// credential) is judged on the binding row B read, before the token is used: 403 `host-bound`,
/// no commit at all, the binding row at the version it had, and the token unused. It catches a
/// hello that uses the token or writes its binding before the registry has judged it (and then
/// has to undo them, when it can).
#[tokio::test]
async fn a_hello_refused_for_host_bound_leaves_the_binding_and_the_token_alone() {
    let world = World::new();
    let a = world.server("binding_bound_a", Kind::KEPT);
    let b = world.server("binding_bound_b", Kind::KEPT);
    a.join("host-a").await;
    let minted = b
        .agents
        .joins
        .mint(
            InstanceIdentity {
                provider: "lambdalabs".into(),
                instance_id: "i-1".into(),
            },
            Duration::from_secs(600),
        )
        .unwrap();
    let who = Credential::of_token(&minted.token);
    let version = row(&world, "host-a").unwrap().version;
    let commits = world.commits();

    let body = Ok(hello_body("host-a"));
    let refused = serve_hello(&b.agents, who.clone(), Some(who.clone()), body, &|| ()).await;
    assert_eq!(said(&refused.unwrap_err()), (403, "host-bound"));
    assert_eq!(world.commits(), commits, "nothing was committed");
    assert_eq!(row(&world, "host-a").unwrap().version, version);
    let unused = b.agents.joins.serves(&who, "host-a");
    assert_eq!(unused, Err(crate::join_tokens::JoinRefusal::NotAdmitted));
}

/// Asserts that a hello whose binding write loses the row's version is refused and writes
/// nothing. Server B holds `host-a` with a cordon its registry holds and the row does not, so
/// its hello after a silence writes the binding at the version it read; another server writes
/// the row (with a taint) before that commit: 503 `state-write-failed`, the row as the other
/// server wrote it, B's session unchanged. The agent's next hello reads the row again, takes the
/// taint and is admitted, writing the binding over the new version with the taint kept. It
/// catches a hello that writes at `Cond::Any` over a row another server wrote since it was
/// read, and a refusal that leaves the session half taken.
#[tokio::test]
async fn a_hello_whose_binding_write_loses_the_version_is_refused_and_applies_nothing() {
    let world = World::new();
    let b = world.server("binding_version_b", Kind::KEPT);
    let session = b.join("host-a").await;
    world.advance(Duration::from_secs(600));
    let host = id("host-a");
    let cordon = Cordon {
        until: Duration::from_secs(900),
        holder: None,
    };
    b.agents.registry.set_cordon(&host, Some(&cordon)).unwrap();
    rival_writes(&world, "host-a", &b.who, true);
    let commits = world.commits();

    let refused = b.hello("host-a").await.unwrap_err();
    assert_eq!(said(&refused), (503, "state-write-failed"));
    assert_eq!(
        world.commits(),
        commits + 1,
        "the other server's write only"
    );
    assert_eq!(stored(&world, "host-a")["cordon_until_ms"], Value::Null);
    assert_eq!(stored(&world, "host-a")["taint_reason"], "residue");
    assert_eq!(b.holds("host-a"), Some(session.epoch));

    let welcome = b.hello("host-a").await.unwrap();
    assert_eq!(welcome.epoch, session.epoch + 1);
    let written = stored(&world, "host-a");
    assert_eq!(written["taint_reason"], "residue");
    assert_ne!(written["cordon_until_ms"], Value::Null);
}

/// Asserts that the first hello of a host writes its binding at `Cond::Absent`: another server's
/// first hello of the same host commits its binding between this hello's read and its write, and
/// this hello is refused 503 `state-write-failed` with the other's row as it was, instead of
/// overwriting it. The agent's next hello reads that row and is admitted. It catches a first
/// hello that puts at `Cond::Any`, so that of two servers that bind one host id the later one
/// wins silently.
#[tokio::test]
async fn the_first_hello_of_a_host_never_overwrites_a_binding_written_in_between() {
    let world = World::new();
    let b = world.server("binding_absent_b", Kind::KEPT);
    rival_writes(&world, "host-a", &b.who, false);
    let commits = world.commits();

    let refused = b.hello("host-a").await.unwrap_err();
    assert_eq!(said(&refused), (503, "state-write-failed"));
    assert_eq!(
        world.commits(),
        commits + 1,
        "the other server's write only"
    );
    let rival = row(&world, "host-a").unwrap();
    assert_eq!(b.holds("host-a"), None);

    b.hello("host-a")
        .await
        .expect("the next hello reads the row");
    assert!(row(&world, "host-a").unwrap().version > rival.version);
    assert_eq!(b.holds("host-a"), Some(1));
}

/// Asserts that a cordon, an uncordon and the clear of a taint on a server that does not hold the
/// host are served from the rows: `host-a` said hello to server A, which stored a taint for it,
/// and server B answers the cordon, the uncordon and the clear of the taint (the taint comes
/// back, and the row loses it), while a host no row and no binding knows is `NotHeld` on all
/// three, and a read of the rows that fails is `Unsaved`. It catches a route that answers a host
/// another server serves as unknown (the operator could not cordon a host whose agent says hello
/// to another server), and one that makes up a host.
#[tokio::test]
async fn cordon_uncordon_and_clear_taint_take_the_host_from_the_rows() {
    let world = World::new();
    let a = world.server("binding_rows_a", Kind::KEPT);
    let b = world.server("binding_rows_b", Kind::KEPT);
    let session = a.join("host-a").await;
    let reported = a.beat_with(&session, Some(1), Vec::new(), json!({ "taint": residue() }));
    assert!(reported.await.unwrap().taint_ack);
    let (host, ghost) = (id("host-a"), id("ghost"));
    let lease = Duration::from_secs(60);
    assert_eq!(b.holds("host-a"), None);

    let cordoned = b.agents.cordon(&host, lease, Asker::Unnamed).await.unwrap();
    assert!(cordoned.placed);
    assert_ne!(stored(&world, "host-a")["cordon_until_ms"], Value::Null);
    assert_eq!(b.holds("host-a"), Some(session.epoch));
    assert!(b.agents.uncordon(&host, None).await.unwrap().lifted);
    assert_eq!(stored(&world, "host-a")["cordon_until_ms"], Value::Null);

    let cleared = b.agents.clear_taint(&host).await.unwrap();
    assert_eq!(
        cleared.map(|taint: Taint| taint.reason),
        Some("residue".into())
    );
    assert_eq!(stored(&world, "host-a")["taint_reason"], Value::Null);

    let unknown = b.agents.cordon(&ghost, lease, Asker::Unnamed).await;
    assert_eq!(unknown.err(), Some(CordonFault::NotHeld));
    let unknown = b.agents.uncordon(&ghost, None).await;
    assert_eq!(unknown.err(), Some(CordonFault::NotHeld));
    assert_eq!(
        b.agents.clear_taint(&ghost).await,
        Err(CordonFault::NotHeld)
    );

    let c = world.server("binding_rows_c", Kind::KEPT);
    world.store.fail_read_after(0);
    let unread = c.agents.cordon(&host, lease, Asker::Unnamed).await;
    assert_eq!(unread.err(), Some(CordonFault::Unsaved));
}
