//! The expectation that hosts launched work after a time: [`super::Expect::launched_on`].

use js_sim::text::list;
use js_types::sched::FarmTime;

use crate::bus::agent_name;
use crate::ledger::Ledger;

/// The statement, what it wanted and what it got, of
/// [`super::Expect::launched_on`]`(agents, after)` over `ledger`: wanted is the hosts of
/// `agents`, got is those of them whose agents launched a run at or after `after`, by
/// the cell's launch ledger, each in index order.
pub(super) fn judged(agents: &[usize], after: FarmTime, ledger: &Ledger) -> [String; 3] {
    let launches = ledger.launches();
    let launched = |name: &&String| {
        let mut late = launches.iter().filter(|launch| launch.at >= after);
        late.any(|launch| launch.host.as_str() == name.as_str())
    };
    let wanted: Vec<String> = agents
        .iter()
        .map(|i| agent_name(*i).as_str().to_owned())
        .collect();
    let got: Vec<String> = wanted.iter().filter(launched).cloned().collect();
    let name = format!("launched-on(after {})", after.as_millis());
    [
        name,
        list(&wanted, String::clone),
        list(&got, String::clone),
    ]
}
