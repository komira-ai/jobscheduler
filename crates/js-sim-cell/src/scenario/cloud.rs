//! The statements about an elastic pool's cloud: [`super::Expect::pool_tripped`], read from
//! the server's scaler when judged, and [`super::Expect::machines_launched`], read from the
//! elastic fake.

use crate::cell::Cell;
use crate::server::ServerNode;

/// A statement about the cell's elastic pool's cloud.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum CloudFact {
    /// The brake of the pool reads tripped (`true`) or not.
    Tripped(String, bool),
    /// The cloud of the pool launched this many machines of the scheduler's.
    Launched(String, usize),
}

/// What a statement about a pool `got` reads when the cell's elastic pool is not that pool.
const NO_POOL: &str = "no-pool";

/// The statement, what it wanted and what it got, of `fact` over `cell` now. A pool that
/// is not the cell's elastic pool (or a cell with none) got `no-pool`.
pub(super) fn judged(fact: &CloudFact, cell: &Cell) -> [String; 3] {
    let (name, pool, wanted) = match fact {
        CloudFact::Tripped(pool, tripped) => ("pool-tripped", pool, tripped.to_string()),
        CloudFact::Launched(pool, count) => ("machines-launched", pool, count.to_string()),
    };
    let cloud = cell
        .cloud()
        .filter(|cloud| cloud.pool().name() == pool.as_str());
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
    });
    let got = got.unwrap_or_else(|| NO_POOL.to_owned());
    [format!("{name}({pool})"), wanted, got]
}
