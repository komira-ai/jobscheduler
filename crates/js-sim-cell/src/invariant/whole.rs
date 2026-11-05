//! The whole-host invariants: [`Invariant::WholeHostIsExclusive`],
//! [`Invariant::HeldMeansReady`], [`Invariant::DrainKillsNothing`] and
//! [`Invariant::MarkedHostsServeNoOrdinaryStart`], checked by the [`Checker`] at every
//! step under the module's cost rule: an item is read when the step touched it, a host
//! when the step touched its agent, an item placed there or the server's listing, and
//! each ready report, start and stop once, when it is appended.
//!
//! What is a whole host comes from what the client asked for ([`crate::Ask::whole_host`]),
//! never from the server's listing; the listing is read for what the server says it
//! does to a host (what it books there, which request it sets the host aside for, and
//! the tail it set it aside on).

use js_sim::text::time;
use js_types::HostId;

use super::{Checker, Found, Invariant};
use crate::observe::{ClientEvent, ItemId, ItemRecord, Observations, StopSeen};
use crate::text;

impl Checker {
    /// Reads the ready reports appended since the last step: the first time each host
    /// reported each lease.
    pub(super) fn read_ready(&mut self, observations: &Observations) {
        for report in observations.ready.iter().skip(self.ready_read) {
            let key = (report.host.clone(), report.lease.clone());
            self.ready.entry(key).or_insert(report.at);
        }
        self.ready_read = observations.ready.len();
    }

    /// Indexes one touched item's run and, for an item that did not ask for a whole
    /// host, its lease; and checks `HeldMeansReady` for a whole-host item: every `Placed`
    /// (the grant of the lease the item last heard queued) follows a ready report of that
    /// host for exactly that lease.
    pub(super) fn whole_item(&mut self, item: ItemId, record: &ItemRecord, found: &mut Found) {
        let mut lease = None;
        for heard in &record.heard {
            if let ClientEvent::Queued { lease: queued, run } = &heard.event {
                self.runs.insert(run.clone(), item);
                if !record.ask.whole_host {
                    self.ordinary.insert(*queued);
                }
                lease = Some(*queued);
            }
            if !record.ask.whole_host {
                continue;
            }
            if let (ClientEvent::Placed { host }, Some(lease)) = (&heard.event, lease) {
                let key = (host.clone(), lease.to_string());
                let reported = self.ready.get(&key);
                if reported.is_none_or(|at| *at > heard.at) {
                    let detail = format!(
                        "whole-host {} was granted lease {} on host {} at {} before the \
                         host's agent reported ready for that lease",
                        text::item(item),
                        text::lease(lease),
                        text::host(host),
                        time(heard.at)
                    );
                    found.flag(Invariant::HeldMeansReady, detail);
                }
            }
        }
    }

    /// `WholeHostIsExclusive` on one host: no two whole-host items hold it at once (by
    /// the client's facts), and at a step that re-read the server's listing, the listing
    /// books no lease there but the leases of the items that hold it.
    pub(super) fn exclusive(
        &self,
        observations: &Observations,
        host: &HostId,
        server_read: bool,
        found: &mut Found,
    ) {
        let Some(holders) = self.held.get(host) else {
            return;
        };
        let mut each = holders.iter();
        let Some((item, held)) = each.next() else {
            return;
        };
        if let Some((other, also)) = each.next() {
            let detail = format!(
                "host {} is held by whole-host {} with lease {} and by whole-host {} with \
                 lease {}",
                text::host(host),
                text::item(*item),
                text::lease(*held),
                text::item(*other),
                text::lease(*also)
            );
            found.flag(Invariant::WholeHostIsExclusive, detail);
        }
        let listed = observations.server.hosts.get(host).filter(|_| server_read);
        let booked = listed.into_iter().flat_map(|listed| &listed.leases);
        for lease in booked.filter(|lease| !holders.values().any(|own| own == *lease)) {
            let detail = format!(
                "host {} books lease {} while whole-host {} holds lease {} there",
                text::host(host),
                text::lease(*lease),
                text::item(*item),
                text::lease(*held)
            );
            found.flag(Invariant::WholeHostIsExclusive, detail);
        }
    }

    /// `MarkedHostsServeNoOrdinaryStart` over the starts appended since the last step: a
    /// start of an ordinary item's lease on a host the listing sets aside for a request
    /// is backfill inside the host's tail, or a violation.
    pub(super) fn read_starts(&mut self, observations: &Observations, found: &mut Found) {
        for start in observations.starts.iter().skip(self.starts_read) {
            let Some(listed) = observations.server.hosts.get(&start.host) else {
                continue;
            };
            let Some(request) = &listed.reserved_for else {
                continue;
            };
            if !self.ordinary.contains(&start.lease) {
                continue;
            }
            let ends = start.asked_at.saturating_add(start.wall);
            if listed.tail_end.is_some_and(|tail| ends <= tail) {
                continue;
            }
            let tail = listed.tail_end.map_or_else(|| "unknown".to_owned(), time);
            let detail = format!(
                "host {}, set aside for request {request} with its tail ending at {tail}, \
                 was sent a start of {} of ordinary lease {} at {} that may run until {}",
                text::host(&start.host),
                text::run(&start.run),
                text::lease(start.lease),
                time(start.at),
                time(ends)
            );
            found.flag(Invariant::MarkedHostsServeNoOrdinaryStart, detail);
        }
        self.starts_read = observations.starts.len();
    }

    /// `DrainKillsNothing` over the stops appended since the last step: a stop or cancel
    /// of a running run on a host the listing sets aside for a request is one the run's
    /// client asked for, or one of a run past its own wall limit, or a violation.
    pub(super) fn read_stops(&mut self, observations: &Observations, found: &mut Found) {
        for stop in observations.stops.iter().skip(self.stops_read) {
            let Some(listed) = observations.server.hosts.get(&stop.host) else {
                continue;
            };
            let Some(request) = &listed.reserved_for else {
                continue;
            };
            if self.asked_to_stop(observations, stop) {
                continue;
            }
            let detail = format!(
                "host {}, set aside for request {request}, was sent a {} of {} of lease {} \
                 at {} that its client did not ask for",
                text::host(&stop.host),
                stop.command.name(),
                text::run(&stop.run),
                text::lease(stop.lease),
                time(stop.at)
            );
            found.flag(Invariant::DrainKillsNothing, detail);
        }
        self.stops_read = observations.stops.len();
    }

    /// Whether `stop` is one the run's own client asked for by the time it arrived (it
    /// asked to cancel the item, or sent the release of its lease), or ends a run whose
    /// own wall limit had run out.
    fn asked_to_stop(&self, observations: &Observations, stop: &StopSeen) -> bool {
        let past_wall = stop.at >= stop.wall_ends_at;
        let item = self.runs.get(&stop.run);
        let cancelled = item.and_then(|item| observations.cancels.get(item));
        let cancelled = cancelled.is_some_and(|at| *at <= stop.at);
        let record = item.and_then(|item| observations.items.get(item));
        let heard = record.into_iter().flat_map(|record| &record.heard);
        let released = heard
            .filter(|heard| heard.at <= stop.at)
            .any(|heard| heard.event == ClientEvent::ReleaseSent);
        past_wall | cancelled | released
    }
}
