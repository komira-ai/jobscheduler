//! What a server needs of the registry to keep and read the hosts' `node/` rows in the job
//! store (design `multi-instance.md`, G5 and G7): the times that judge a row live and forget
//! it, and a pool's machine cap and booking rule.

use std::time::Duration;

use js_types::Shape;

use super::Registry;
use crate::config::{PoolConfig, placement_shape};

impl Registry {
    /// Every pool this registry serves, the first first.
    fn pools(&self) -> impl Iterator<Item = &PoolConfig> {
        std::iter::once(&self.shared.first).chain(self.shared.more.iter())
    }

    /// The longest grace of the pools this registry serves: a host's row heard within it is
    /// live, whichever pool the host is in.
    pub fn longest_grace(&self) -> Duration {
        let ms = self.pools().map(PoolConfig::grace_ms).max().unwrap_or(0);
        Duration::from_millis(ms)
    }

    /// The longest grace plus the longest lost-host retention of the pools this registry
    /// serves: how long a host's row may stay silent before the row is forgotten, as the
    /// registry forgets a host lost past its retention.
    pub fn forget_after(&self) -> Duration {
        let retention = self.pools().map(PoolConfig::lost_retention_ms).max();
        let ms = retention.unwrap_or(0);
        self.longest_grace()
            .saturating_add(Duration::from_millis(ms))
    }

    /// The machine cap of the pool named `pool`, or `None` for a pool this registry does not
    /// serve.
    pub fn pool_cap(&self, pool: &str) -> Option<usize> {
        let at = self.pool_named(pool)?;
        Some(self.pool_at(at).max_machines())
    }

    /// The shape a job is placed against on a host of the pool named `pool` that the pool
    /// books as `booked` and whose hello claimed `claim`, as a host the registry holds is
    /// ([`crate::HostView::placement_shape`]); `None` for a pool this registry does not serve.
    pub fn placement_shape_in(
        &self,
        pool: &str,
        booked: &Shape,
        claim: Option<&Shape>,
    ) -> Option<Shape> {
        let at = self.pool_named(pool)?;
        let by_hello = self.pool_at(at).places_by_hello();
        Some(placement_shape(booked, claim, by_hello))
    }
}
