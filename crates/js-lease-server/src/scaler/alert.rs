//! What the shell tells an operator: the things the scaler counts and never releases on its
//! own, the one it released-or-should-have and did not, and a machine it cannot terminate
//! because its token cannot be revoked.
//!
//! Each alert has a key that holds while the thing it names holds, so the shell says it
//! once and says it again only if it clears and comes back.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use js_scaler::{Input, Mark, Millis, Phase, Record, State};

/// The alerts that hold now, by key.
///
/// - An `Orphan` record: a machine of ours that no record knows. It is counted against the
///   caps and never released by the scaler; the operator releases it at the provider.
/// - A record of a pool the server no longer has: it stays counted and no step reconciles
///   it, so the operator releases the machine and removes the record.
/// - A silent host that books nothing and is still not marked releasing at twice `hold`
///   (the step drains and marks it at `hold`): something stops the release, and the
///   machine costs money.
/// - A host in `unrevoked`: its join token could not be revoked, so its machine is not
///   terminated (never while the token may work), and the revoke is tried again every
///   tick. Until it goes through the machine runs and is paid for.
pub(super) fn alerts(
    (state, input): (&State, &Input),
    now: Millis,
    hold: Duration,
    unrevoked: &BTreeSet<String>,
) -> BTreeMap<String, String> {
    let known: BTreeSet<&str> = input
        .pools
        .iter()
        .map(|pool| pool.settings.pool.as_str())
        .collect();
    let records = state.records.instances.iter();
    let orphans = records
        .clone()
        .filter(|record| record.phase == Phase::Orphan)
        .map(|record| alert("orphan", record, "a machine of ours that no record knows"));
    let strays = records
        .filter(|record| record.counts() & !known.contains(record.pool.as_str()))
        .map(|record| {
            alert(
                "pool",
                record,
                "a record of a pool this server does not have",
            )
        });
    let late = hold.saturating_mul(2);
    let stuck = state
        .silent
        .iter()
        .filter(|(_, since)| now.since(**since) >= late)
        .filter(|(name, _)| unmarked(input, name))
        .map(|(name, _)| {
            (
                format!("silent:{name}"),
                format!(
                    "host {name} has been silent past twice unknown_hold, books nothing and is not marked releasing; its machine is still held"
                ),
            )
        });
    let unrevoked = unrevoked.iter().map(|name| {
        (
            format!("revoke:{name}"),
            format!(
                "the join token of host {name} cannot be revoked, so its machine is not terminated and keeps running; the revoke is tried again every tick, so check the join token file"
            ),
        )
    });
    orphans
        .chain(strays)
        .chain(stuck)
        .chain(unrevoked)
        .collect()
}

/// Whether `input` shows the host `name` booking nothing and not marked releasing.
fn unmarked(input: &Input, name: &str) -> bool {
    input.hosts.iter().any(|host| {
        [
            host.id.as_str() == name,
            host.lease_given_up,
            host.mark != Some(Mark::Releasing),
        ]
        .into_iter()
        .all(|holds| holds)
    })
}

/// The key and the text of an alert of `kind` about `record`.
fn alert(kind: &str, record: &Record, what: &str) -> (String, String) {
    let cloud = record.cloud_id.as_deref().unwrap_or_default();
    let key = format!("{kind}:{}:{}:{cloud}", record.pool, record.name);
    let text = format!(
        "{what}: pool {:?}, name {:?}, provider id {cloud:?}, phase {:?}; release it at the provider",
        record.pool, record.name, record.phase
    );
    (key, text)
}
