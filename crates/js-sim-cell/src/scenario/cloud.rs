//! The statements about an elastic pool's clouds: [`super::Expect::pool_tripped`], read from
//! the server's scaler when judged; [`super::Expect::machines_launched`], read from the
//! elastic fake; [`super::Expect::acquires`] and [`super::Expect::acquires_at_most`], read
//! from the recording wrapper; and [`super::Expect::machines_joined`], read from what the
//! cell observed of the cloud's machines.

use crate::cell::Cell;
use crate::elastic::{Cloud, CloudEvent};
use crate::server::ServerNode;

/// A statement about one of the cell's elastic pool's clouds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum CloudFact {
    /// The brake of the pool reads tripped (`true`) or not.
    Tripped(String, bool),
    /// The cloud of the pool launched this many machines of the scheduler's.
    Launched(String, usize),
    /// The cloud of the pool was asked for this many machines.
    Acquires(String, usize),
    /// The cloud of the pool was asked for at most this many machines.
    AcquiresAtMost(String, usize),
    /// This many of the machines the cloud of the pool launched joined.
    Joined(String, usize),
}

/// What a statement about a pool `got` reads when the cell has no cloud of that pool.
const NO_POOL: &str = "no-pool";

/// The statement, what it wanted and what it got, of `fact` over `cell` now. A pool the
/// cell has no cloud of (for [`CloudFact::Tripped`], a pool that is not the cell's first
/// cloud's) got `no-pool`.
pub(super) fn judged(fact: &CloudFact, cell: &Cell) -> [String; 3] {
    let (name, pool, wanted) = match fact {
        CloudFact::Tripped(pool, tripped) => ("pool-tripped", pool, tripped.to_string()),
        CloudFact::Launched(pool, count) => ("machines-launched", pool, count.to_string()),
        CloudFact::Acquires(pool, count) => ("acquires", pool, count.to_string()),
        CloudFact::AcquiresAtMost(pool, count) => ("acquires-at-most", pool, at_most(*count)),
        CloudFact::Joined(pool, count) => ("machines-joined", pool, count.to_string()),
    };
    let cloud = match fact {
        CloudFact::Tripped(..) => cell
            .cloud()
            .filter(|cloud| cloud.pool().name() == pool.as_str()),
        _ => cell.cloud_named(pool),
    };
    let got = cloud.map(|cloud| match fact {
        CloudFact::Tripped(..) => {
            let server = cell.server();
            let tripped = server.is_some_and(|server| ServerNode::brake_tripped(server, pool));
            tripped.to_string()
        }
        CloudFact::Launched(..) => {
            // A machine the scheduler launched carries the boot script it rendered; one
            // added as foreign carries none.
            let fake = cloud.fake();
            let ours = (0..fake.launched()).filter(|n| !fake.user_data(*n).is_empty());
            ours.count().to_string()
        }
        CloudFact::Acquires(..) => acquires(cloud).to_string(),
        CloudFact::AcquiresAtMost(_, count) => {
            let made = acquires(cloud);
            if made <= *count {
                at_most(*count)
            } else {
                made.to_string()
            }
        }
        CloudFact::Joined(..) => joined(cell, cloud).to_string(),
    });
    let got = got.unwrap_or_else(|| NO_POOL.to_owned());
    [format!("{name}({pool})"), wanted, got]
}

/// How a bound of `count` is written.
fn at_most(count: usize) -> String {
    format!("at most {count}")
}

/// How many acquires the recording wrapper of `cloud` holds.
fn acquires(cloud: &Cloud) -> usize {
    let calls = cloud.calls();
    let acquires = calls
        .iter()
        .filter(|call| matches!(call.event, CloudEvent::Acquire { .. }));
    acquires.count()
}

/// How many of the machines `cloud` launched joined, as the cell observed them.
fn joined(cell: &Cell, cloud: &Cloud) -> usize {
    let clouds = &cell.observations().clouds;
    let facts = clouds
        .iter()
        .find(|facts| facts.pool == cloud.pool().name());
    let machines = facts.into_iter().flat_map(|facts| facts.machines.values());
    machines.filter(|machine| machine.joined).count()
}
