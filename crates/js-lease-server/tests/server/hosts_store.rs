//! The host bindings and the join tokens kept in the state store (`--store`; SQLite with the
//! dev flag here, and one restart over Postgres in `store_postgres.rs`): what a restart reads
//! back, that nothing is written beside the lease file, and that a stored value that cannot
//! be read as written refuses the start.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use js_lease_server::channel_ticker;
use js_state::sqlite::{Options, SqliteStore};
use js_state::{Cond, Store, SystemClock, Txn};
use serde_json::{Value, json};

use crate::agents_support::{
    JOIN, JOIN_B, agent_args, entry, hello_with, hosts, post, restarted, session, start_agents_in,
};
use crate::config_support::clock;
use crate::joins::{MINT, code, hello, minted};
use crate::store_support::sqlite_db;
use crate::support::{
    Harness, Kept, Scratch, T0, config, kept_bindings, kept_joins, request, start,
};

/// The file `suffix` names beside `h`'s lease file.
fn beside(h: &Harness, suffix: &str) -> PathBuf {
    let mut path = h.scratch.state().into_os_string();
    path.push(suffix);
    PathBuf::from(path)
}

/// The host `host`'s own reading of itself, with its one-time token `token`.
async fn own(h: &Harness, host: &str, token: &str) -> Value {
    let read = request(
        h.addr(),
        "GET",
        &format!("/v1/hosts/{host}"),
        Some(token),
        None,
    )
    .await;
    assert_eq!(read.status, 200, "{}", read.body);
    read.json()
}

/// Asserts that a restart over the store reads back each host's binding, taint and
/// cordon: a hello for either host with another credential is refused 403 `host-bound`, the
/// taint is still shown, the cordon is still in force with its end, and on the injected
/// clock it lapses at that end; the host that registered with a one-time token goes on with
/// it. No `.agents` or `.joins` file is written, before or after. It catches a binding, a
/// taint or a cordon that lives only in memory or only in a file, and a store mode that
/// writes the files as well.
#[tokio::test]
async fn bindings_taints_and_cordons_survive_a_restart_from_the_store() {
    let first = start_agents_in("hosts_store_bindings", false, Kept::Store).await;
    let tainted = session(&hello_with(&first, JOIN, "burst-1").await, "burst-1");
    let mut beat = tainted.beat();
    beat["taint"] = json!({ "reason": "residue", "check": "tmp" });
    let reply = post(&first, "/v1/agent/heartbeat", Some(JOIN), &beat).await;
    assert_eq!(reply.json()["taint_ack"], true, "{}", reply.body);
    let token = minted(&first, "i-2").await;
    let admitted = hello(&first, &token, "burst-2", "i-2").await;
    assert_eq!(admitted.status, 200, "{}", admitted.body);
    let lease = json!({ "lease_secs": 120 });
    let cordoned = post(&first, "/v1/hosts/burst-2:cordon", Some(&token), &lease).await;
    assert_eq!(cordoned.status, 200, "{}", cordoned.body);
    let kept = kept_bindings(&first);
    assert!(kept.contains("\"taint_reason\":\"residue\""), "{kept}");
    assert!(
        kept.contains(&format!("\"cordon_until_ms\":{}", T0 + 120_000)),
        "{kept}"
    );

    let h = restarted(first, false).await;
    assert_eq!(Kept::of(&h), Kept::Store);
    let other = hello_with(&h, JOIN_B, "burst-1").await;
    assert_eq!((other.status, code(&other)), (403, "host-bound".to_owned()));
    let other = hello_with(&h, JOIN, "burst-2").await;
    assert_eq!((other.status, code(&other)), (403, "host-bound".to_owned()));
    let listing = hosts(&h).await;
    assert_eq!(entry(&listing, "burst-1")["taint"]["reason"], "residue");
    let read = own(&h, "burst-2", &token).await;
    assert_eq!(read["cordon"]["expires_at_ms"], T0 + 120_000);
    h.clock.advance(Duration::from_secs(119));
    assert!(own(&h, "burst-2", &token).await.get("cordon").is_some());
    h.clock.advance(Duration::from_secs(1));
    assert!(own(&h, "burst-2", &token).await.get("cordon").is_none());
    let back = hello(&h, &token, "burst-2", "i-2").await;
    assert_eq!(back.status, 200, "{}", back.body);
    for suffix in [".agents", ".joins"] {
        assert!(!beside(&h, suffix).exists(), "{suffix}");
    }
}

/// Asserts that a restart over the store reads back the join tokens by their digests: an
/// unused token registers its host, a used one is refused for another host as used
/// (403 `join-token-used`), and a revoked one is unknown (401). No stored value, binding or
/// token row, holds the text of any token. It catches a token given back by a restart, a
/// revoke that comes back, and a store that keeps the secret instead of its digest.
#[tokio::test]
async fn join_token_digests_survive_a_restart() {
    let first = start_agents_in("hosts_store_joins", false, Kept::Store).await;
    let unused = minted(&first, "i-1").await;
    let used = minted(&first, "i-2").await;
    let revoked = minted(&first, "i-3").await;
    assert_eq!(hello(&first, &used, "burst-2", "i-2").await.status, 200);
    let body = json!({ "provider": "lambdalabs", "instance_id": "i-3" });
    assert_eq!(first.call("DELETE", MINT, Some(&body)).await.status, 204);

    let h = restarted(first, false).await;
    let stored = format!("{}{}", kept_joins(&h), kept_bindings(&h));
    for token in [&unused, &used, &revoked] {
        assert!(!stored.contains(token.as_str()), "{stored}");
    }
    let taken = hello(&h, &used, "burst-3", "i-2").await;
    assert_eq!(
        (taken.status, code(&taken)),
        (403, "join-token-used".to_owned())
    );
    let gone = hello(&h, &revoked, "burst-3", "i-3").await;
    assert_eq!(gone.status, 401);
    let admitted = hello(&h, &unused, "burst-1", "i-1").await;
    assert_eq!(admitted.status, 200, "{}", admitted.body);
    for suffix in [".agents", ".joins"] {
        assert!(!beside(&h, suffix).exists(), "{suffix}");
    }
}

/// Asserts that a start whose store holds a binding, or a join token row, that cannot be
/// read as written is refused naming `--store`, with the key and what is wrong, as a damaged
/// agent state or join token file is refused naming `--state-file`. It catches a start that
/// reads a damaged store as fewer bindings (any join token could then take a booked host's
/// id) or fewer tokens (a used token given back).
#[tokio::test]
async fn a_stored_host_that_cannot_be_read_refuses_the_start() {
    let cases = [
        ("binding", "hosts/binding/burst-1"),
        ("join", "hosts/join/k"),
    ];
    for (name, key) in cases {
        let scratch = Scratch::new(&format!("hosts_store_damaged_{name}"));
        {
            let clock = Arc::new(SystemClock::default());
            let db = sqlite_db(&scratch);
            let store = SqliteStore::open(&db, clock, Options::default()).unwrap();
            let damaged = Txn::new().put(key, "{}", Cond::Any);
            store.txn(damaged).unwrap().unwrap();
        }
        let args = Kept::Store.args(&scratch, agent_args(&scratch, false));
        let (_ticks, ticker) = channel_ticker();
        let started = start(config(args).unwrap(), clock(), ticker).await;
        let refused = started.err().unwrap();
        assert_eq!(refused.flag(), "--store");
        let reason = format!("{key} does not hold this key's fields");
        assert_eq!(refused.reason(), reason);
    }
}
