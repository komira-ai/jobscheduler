//! What a server needs of the registry to keep and read the hosts' `node/` rows in the job
//! store (design `multi-instance.md`, G5 and G7): the times that judge a row live, refresh it
//! and forget it, a pool's machine cap and booking rule, the session digest a hello wrote,
//! and the withdrawal of a hello that the rows refused after the registry admitted it.

use std::time::Duration;

use js_types::{HostId, Shape};

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

    /// Half the shortest grace of the pools this registry serves: how often a beat whose host's
    /// state stands still writes its row, so that the row of a host that beats is never older
    /// than its grace.
    pub fn refresh_every(&self) -> Duration {
        let ms = self.pools().map(PoolConfig::grace_ms).min().unwrap_or(0);
        Duration::from_millis(ms / 2)
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

    /// The SHA-256 of the session secret the last hello of `host` was given, when the registry
    /// holds the host and gave it one.
    pub fn session_digest(&self, host: &HostId) -> Option<[u8; 32]> {
        let state = self.lock();
        let record = state.records.get(host)?;
        record.session.as_ref().map(|digest| digest.bytes())
    }

    /// Takes back the hello of `host` that the registry admitted as its first (`epoch` 1), when
    /// something the registry does not judge refused it afterwards (the pool's cap counted over
    /// every server's rows): the record and its place in the host book go, and the placer is
    /// told the host is gone. `true` when it was taken back; a host whose record is of another
    /// epoch, or that the registry does not hold, is left as it is.
    pub fn withdraw_first_hello(&self, host: &HostId, epoch: u64) -> bool {
        let mut guard = self.lock();
        let first = guard
            .records
            .get(host)
            .is_some_and(|record| (record.epoch == epoch) & (epoch == 1));
        if !first {
            return false;
        }
        guard.book.remove_free(host);
        guard.records.remove(host);
        drop(guard);
        self.tell_gone(host);
        true
    }
}
