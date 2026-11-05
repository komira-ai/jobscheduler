//! [`Invariant::NoResultLost`], an end check over facts the cell owns: the completions
//! and verdicts of its ledger, the processes it killed, and what the client heard.

use std::collections::BTreeMap;

use js_sim::text::time;
use js_types::sched::FarmTime;

use super::{Found, Invariant};
use crate::ledger::{Acked, Completion};
use crate::observe::{
    ClientEvent, Heard, HowEnded, ItemId, ItemRecord, Observations, RunId, Verdict,
};
use crate::text;

/// Flags every way `observations` lose a result or record one twice: (a), (b) and (c) of
/// [`Invariant::NoResultLost`].
pub(super) fn check(observations: &Observations, found: &mut Found) {
    accepted_once(observations, found);
    let items = queued_runs(observations);
    for completion in &observations.completions {
        completed(observations, &items, completion, found);
    }
    for (run, (item, record)) in &items {
        let succeeded = terminal(record).is_some_and(|ended| ended.0 == HowEnded::Succeeded);
        let ran = observations.completions.iter().any(|c| &c.run == *run);
        if succeeded && !ran {
            let detail = format!(
                "{} succeeded and no agent completed its {}",
                text::item(*item),
                text::run(run)
            );
            found.flag(Invariant::NoResultLost, detail);
        }
    }
}

/// (a): no run has two `Accepted` verdicts among those the agents read.
fn accepted_once(observations: &Observations, found: &mut Found) {
    let mut first: BTreeMap<&RunId, &Acked> = BTreeMap::new();
    let accepted = observations.verdicts.iter();
    for again in accepted.filter(|acked| acked.verdict == Verdict::Accepted) {
        let Some(once) = first.insert(&again.run, again) else {
            continue;
        };
        let detail = format!(
            "{} was accepted twice: on host {} at {} and on host {} at {}",
            text::run(&again.run),
            text::host(&once.host),
            time(once.at),
            text::host(&again.host),
            time(again.at)
        );
        found.flag(Invariant::NoResultLost, detail);
    }
}

/// (b) for one completion: its client ends `Succeeded`, unless the client ended before
/// it or the agent died before it read a verdict on it.
fn completed(
    observations: &Observations,
    items: &BTreeMap<&RunId, (ItemId, &ItemRecord)>,
    completion: &Completion,
    found: &mut Found,
) {
    let Some((item, record)) = items.get(&completion.run) else {
        return;
    };
    let ended = terminal(record);
    let before = ended.is_some_and(|(_, at)| at < completion.at);
    if before || killed_before_verdict(observations, completion) {
        return;
    }
    let how = match ended {
        Some((HowEnded::Succeeded, _)) => return,
        Some((how, _)) => format!("ended {}", how.name()),
        None => "has no terminal answer".to_owned(),
    };
    let detail = format!(
        "{} completed on host {} at {} and {} {how}",
        text::run(&completion.run),
        text::host(&completion.host),
        time(completion.at),
        text::item(*item)
    );
    found.flag(Invariant::NoResultLost, detail);
}

/// Whether the cell killed the completing agent's process at or after the completion and
/// before the first verdict that host's agent read on the run at or after it (any kill
/// from the completion on, when it read none).
fn killed_before_verdict(observations: &Observations, completion: &Completion) -> bool {
    let after = |at: FarmTime| at >= completion.at;
    let verdicts = observations.verdicts.iter();
    let judged = verdicts
        .filter(|acked| acked.run == completion.run && acked.host == completion.host)
        .map(|acked| acked.at)
        .find(|at| after(*at));
    let kills = observations.kills.iter();
    let mut died = kills.filter(|kill| kill.host == completion.host && after(kill.at));
    died.any(|kill| judged.is_none_or(|judged| kill.at < judged))
}

/// The run each item queued, with the item: what the client learned from
/// [`ClientEvent::Queued`].
fn queued_runs(observations: &Observations) -> BTreeMap<&RunId, (ItemId, &ItemRecord)> {
    let mut runs = BTreeMap::new();
    for (item, record) in &observations.items {
        for heard in &record.heard {
            if let ClientEvent::Queued { run, .. } = &heard.event {
                runs.insert(run, (*item, record));
            }
        }
    }
    runs
}

/// The item's first terminal answer and when it was heard.
fn terminal(record: &ItemRecord) -> Option<(HowEnded, FarmTime)> {
    record
        .heard
        .iter()
        .find_map(|heard: &Heard| match heard.event {
            ClientEvent::Terminal { how } => Some((how, heard.at)),
            _ => None,
        })
}
