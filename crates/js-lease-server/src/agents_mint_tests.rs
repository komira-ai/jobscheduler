//! `Agents::mint_for` and `Agents::revoke_for`: the in-process form of the join token
//! routes, which the scaler calls for a machine it launches and releases.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use js_elastic::{AcquireId, SchedulerId};
use js_lease::{ManualClock, Timestamp};
use js_registry::{PoolConfig, SequentialSessions};
use js_types::{ResourceClass, Resources, Shape};

use super::*;
use crate::join_tokens::{Admission, JoinRefusal};

const T0: u64 = 5_000_000;

fn agents(name: &str) -> (Agents, ManualClock) {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/agents_mint_unit")
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let clock = ManualClock::new(Timestamp::from_millis(T0));
    let shared = SharedClock::new(clock.clone());
    let resources = Resources::new(8000, 17179869184, 1, Some("nvidia-a100".to_string())).unwrap();
    let shape = Shape::new(resources, BTreeMap::new());
    let pool = PoolConfig::new(pool_id(), ResourceClass::gpu_host(), shape, 4);
    let registry = Registry::with_sessions(pool, clock.clone(), SequentialSessions::new());
    (Agents::over(registry, &shared, &dir), clock)
}

fn pool_id() -> ProviderId {
    ProviderId::new("burst").unwrap()
}

/// The name of the host launched for `acquire`.
fn name(acquire: &str) -> HostName {
    let scheduler = SchedulerId::new("sched").unwrap();
    HostName::derive(&scheduler, &AcquireId::new(acquire).unwrap())
}

/// The identity a launched host puts in its hello: `<pool>:<name>`.
fn identity(name: &HostName) -> InstanceIdentity {
    InstanceIdentity {
        provider: "burst".to_string(),
        instance_id: name.to_string(),
    }
}

/// A token minted for `pool:name` is bound to exactly that instance: it admits a hello
/// that names it, refuses one that names another name or another pool as
/// `join-token-instance` (and stays unused for the right one), expires `ttl` after the
/// mint, and is not shown anywhere but in the answer. It catches a token bound to the wrong
/// pair (the pool and the name swapped, or the name left out), and a lifetime that is not
/// the one asked for.
#[test]
fn a_token_minted_for_a_pool_and_name_is_bound_to_that_instance() {
    let (agents, clock) = agents("bound");
    let launched = name("acquire-1");
    let minted = agents
        .mint_for(&pool_id(), &launched, Duration::from_secs(600))
        .unwrap();
    assert_eq!(minted.expires_at_ms, T0 + 600_000);
    let who = Credential::of_token(&minted.token);
    let other_name = identity(&name("acquire-2"));
    let other_pool = InstanceIdentity {
        provider: "other".to_string(),
        ..identity(&launched)
    };
    for wrong in [other_name, other_pool] {
        let refused = agents.joins.admit(&who, "host-a", Some(&wrong));
        assert_eq!(refused, Err(JoinRefusal::WrongInstance));
    }
    let admitted = agents
        .joins
        .admit(&who, "host-a", Some(&identity(&launched)));
    assert_eq!(admitted, Ok(Admission::Fresh));

    let late = name("acquire-3");
    let minted = agents
        .mint_for(&pool_id(), &late, Duration::from_secs(60))
        .unwrap();
    let who = Credential::of_token(&minted.token);
    clock.advance(Duration::from_secs(61));
    let expired = agents.joins.admit(&who, "host-b", Some(&identity(&late)));
    assert_eq!(expired, Err(JoinRefusal::Expired));
}

/// A second mint for the same pool and name replaces the first unused token, and leaves the
/// token of another name alone. It is the property that makes a re-mint after a launch that
/// may have gone out lock the booted host out, which is why the scaler mints once per
/// launch.
#[test]
fn a_second_mint_for_a_name_replaces_the_first() {
    let (agents, _) = agents("replace");
    let (one, two) = (name("acquire-1"), name("acquire-2"));
    let ttl = Duration::from_secs(600);
    let first = agents.mint_for(&pool_id(), &one, ttl).unwrap();
    let other = agents.mint_for(&pool_id(), &two, ttl).unwrap();
    let second = agents.mint_for(&pool_id(), &one, ttl).unwrap();
    assert!(agents.joins.identify(&first.token).is_none());
    assert!(agents.joins.identify(&second.token).is_some());
    assert!(agents.joins.identify(&other.token).is_some());
}

/// `revoke_for` drops everything bound to the host, the host credential included: a host
/// that registered and was given a credential (its token retired) is unknown after the
/// revoke, by the digest the routes authenticate with, serves no heartbeat, and the file
/// keeps only the other host's row. A repeat drops nothing and succeeds. It catches a
/// revoke that handles unused tokens only, which would leave a released host able to
/// heartbeat and say hello with its credential.
#[test]
fn revoke_for_drops_the_token_and_the_host_credential() {
    let (agents, _) = agents("revoke");
    let (gone, kept) = (name("acquire-1"), name("acquire-2"));
    let ttl = Duration::from_secs(600);
    let minted = agents.mint_for(&pool_id(), &gone, ttl).unwrap();
    let token = Credential::of_token(&minted.token);
    let admitted = agents
        .joins
        .admit_with(&token, "host-a", Some(&identity(&gone)), true)
        .unwrap();
    let secret = admitted.issued.unwrap();
    let credential = Credential::of_token(secret.expose());
    assert_eq!(agents.joins.serves(&credential, "host-a"), Ok(()));
    let spared = agents.mint_for(&pool_id(), &kept, ttl).unwrap();

    assert_eq!(agents.revoke_for(&pool_id(), &gone), Ok(1));
    assert!(agents.joins.identify(secret.expose()).is_none());
    assert!(agents.joins.identify(&minted.token).is_none());
    assert_eq!(
        agents.joins.serves(&credential, "host-a"),
        Err(JoinRefusal::NotAdmitted)
    );
    assert!(agents.joins.identify(&spared.token).is_some());
    let file = std::fs::read_to_string(agents_file("revoke")).unwrap();
    assert_eq!(file.matches("[[join]]").count(), 1);
    assert!(!file.contains(gone.as_str()));
    assert_eq!(agents.revoke_for(&pool_id(), &gone), Ok(0));
}

/// The join token file of the agents made by `agents(name)`.
fn agents_file(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/agents_mint_unit")
        .join(name)
        .join("joins")
}

/// A revoke names the pool as well as the host: the same name under another pool keeps its
/// token. It catches a revoke keyed by the name alone.
#[test]
fn revoke_for_is_scoped_to_the_pool() {
    let (agents, _) = agents("scoped");
    let launched = name("acquire-1");
    let ttl = Duration::from_secs(600);
    let minted = agents.mint_for(&pool_id(), &launched, ttl).unwrap();
    let elsewhere = ProviderId::new("elsewhere").unwrap();
    assert_eq!(agents.revoke_for(&elsewhere, &launched), Ok(0));
    assert!(agents.joins.identify(&minted.token).is_some());
    assert_eq!(agents.revoke_for(&pool_id(), &launched), Ok(1));
    assert!(agents.joins.identify(&minted.token).is_none());
}
