//! What a server needs of the registry to keep and read the hosts' `node/` rows in the job
//! store (design `multi-instance.md`, G5 and G7): the times that judge a row live and forget
//! it, a pool's machine cap and booking rule, and the forgetting of a host whose row the
//! operator retired.

use std::time::Duration;

use js_types::{HostId, Shape};

use super::{HostState, Registry, State};
use crate::config::PoolConfig;

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

    /// Forgets `host` now, binding included, as the housekeeping forgets a host lost past its
    /// pool's retention, without waiting the retention out: for the operator's retirement of a
    /// host id, made once the server has judged that the host holds nothing. The host no longer
    /// counts toward its pool's `max_machines`, its binding leaves [`Registry::bindings`] (so
    /// the next save of the bindings deletes it), and the placer is told it is gone. A live host
    /// (heard within its pool's grace) is kept. Answers whether a host was forgotten: `false`,
    /// with nothing changed, for a live host and for one the registry does not hold.
    pub fn forget_host(&self, host: &HostId) -> bool {
        let now = self.shared.clock.now();
        let mut guard = self.lock_at(now);
        let State {
            book,
            records,
            ending,
            ..
        } = &mut *guard;
        let lost = records.get(host).is_some_and(|record| {
            record.state(now, self.pool_of(record).grace_ms()) != HostState::Live
        });
        if !lost {
            return false;
        }
        book.remove_free(host);
        records.remove(host);
        ending.remove(host);
        self.tell_gone(host);
        true
    }

    /// The shape a job is placed against on a host of the pool named `pool` whose row books
    /// it as `booked` and whose hello claimed `claim`, as a host the registry holds is
    /// ([`crate::HostView::placement_shape`]): what the claim reports, or the row's numbers
    /// when it keeps no claim; `None` for a pool this registry does not serve.
    pub fn placement_shape_in(
        &self,
        pool: &str,
        booked: &Shape,
        claim: Option<&Shape>,
    ) -> Option<Shape> {
        let config = self.pool_at(self.pool_named(pool)?);
        Some(config.row_shape(booked, claim))
    }
}
