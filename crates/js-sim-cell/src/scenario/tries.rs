//! The expectations over a whole-host item's tries and its grant, judged over the cell's
//! observations: [`super::Expect::tries`], [`super::Expect::held_after_a_failed_try`],
//! [`super::Expect::kept_off_a_failed_host`] and
//! [`super::Expect::held_after_the_runs_on_its_host`].
//!
//! A try is a lease the client was answered pending under ([`ClientEvent::Pending`]).
//! Where the server booked it, and from when to when, is read from the bookings the
//! server's listing showed ([`Observations::bookings`]): a try that was booked, whose
//! booking ended and that the item was not granted is a failed try (its prepare failed,
//! by a refusal or the deadline, or the client let it go). Each judgement returns the
//! statement's name, what it wanted and what it got.

use std::collections::BTreeSet;
use std::time::Duration;

use js_sim::text::{list, time};
use js_types::LeaseId;
use js_types::sched::FarmTime;

use crate::cell::LISTING_EVERY;
use crate::observe::{Booking, ClientEvent, ItemId, ItemRecord, Observations};
use crate::text;

/// The leases `record` was answered pending under.
fn tries(record: &ItemRecord) -> BTreeSet<LeaseId> {
    let pending = record.heard.iter().filter_map(|heard| match heard.event {
        ClientEvent::Pending { lease } => Some(lease),
        _ => None,
    });
    pending.collect()
}

/// The lease `record` was granted: the one its run was queued on.
fn granted(record: &ItemRecord) -> Option<LeaseId> {
    let queued = |event: &ClientEvent| match event {
        ClientEvent::Queued { lease, .. } => Some(*lease),
        _ => None,
    };
    record.heard.iter().find_map(|heard| queued(&heard.event))
}

/// The bookings of `record`'s tries, in the order the listing showed them.
fn booked<'a>(
    observations: &'a Observations,
    record: &ItemRecord,
) -> impl Iterator<Item = &'a Booking> + Clone {
    let tries = tries(record);
    let bookings = observations.bookings.iter();
    bookings.filter(move |booking| tries.contains(&booking.lease))
}

/// The bookings of `record`'s failed tries, each with when the listing stopped showing
/// it.
fn failed<'a>(
    observations: &'a Observations,
    record: &ItemRecord,
) -> impl Iterator<Item = (&'a Booking, FarmTime)> {
    let granted = granted(record);
    booked(observations, record).filter_map(move |booking| {
        let ungranted = booking.until.filter(|_| Some(booking.lease) != granted);
        ungranted.map(|until| (booking, until))
    })
}

/// [`super::Expect::tries`]`(item, count)`: got is how many leases the item was
/// answered pending under.
pub(super) fn judged_tries(observations: &Observations, item: ItemId, count: usize) -> [String; 3] {
    let record = observations.items.get(&item);
    let got = record.map_or(0, |record| tries(record).len());
    let name = format!("tries({})", text::item(item));
    [name, count.to_string(), got.to_string()]
}

/// [`super::Expect::held_after_a_failed_try`]`(at_least)`: got is how many items were
/// granted a lease after a failed try, or `<at_least> or more` once there are as many.
pub(super) fn judged_held_after_failure(
    observations: &Observations,
    at_least: usize,
) -> [String; 3] {
    let after = observations.items.values().filter(|record| {
        let failed = failed(observations, record).next().is_some();
        failed & granted(record).is_some()
    });
    let count = after.count();
    let wanted = format!("{at_least} or more");
    let got = if count >= at_least {
        wanted.clone()
    } else {
        count.to_string()
    };
    ["held-after-a-failed-try".to_owned(), wanted, got]
}

/// [`super::Expect::kept_off_a_failed_host`]`(backoff)`: got is each try of an item
/// booked on a host where an earlier try of the same item failed, sooner than `backoff`
/// after the listing stopped showing the failed one. The listing is read at least once
/// every [`LISTING_EVERY`], and at every client call (where an acquire commits a try) and
/// poll (which follows each tick, where a waiting try commits and a deadline fails one),
/// so a try's booking is seen when it is made and its end at most that late: the bound
/// is `backoff` less [`LISTING_EVERY`].
pub(super) fn judged_kept_off(observations: &Observations, backoff: Duration) -> [String; 3] {
    let bound = backoff.saturating_sub(LISTING_EVERY);
    let mut close = Vec::new();
    for (item, record) in &observations.items {
        for (failed, ended) in failed(observations, record) {
            let soon = ended.saturating_add(bound);
            let back = booked(observations, record).filter(|booking| {
                (booking.host == failed.host) & (booking.from >= ended) & (booking.from < soon)
            });
            close.extend(back.map(|booking| {
                format!(
                    "{} booked lease {} on host {} at {}, its try there having failed at {}",
                    text::item(*item),
                    text::lease(booking.lease),
                    text::host(&booking.host),
                    time(booking.from),
                    time(ended)
                )
            }));
        }
    }
    let name = format!("kept-off-a-failed-host(backoff {})", backoff.as_millis());
    [name, "[]".to_owned(), list(&close, String::clone)]
}

/// [`super::Expect::held_after_the_runs_on_its_host`]`(item)`: got is each run launched
/// on the item's host before its grant that had not completed by then, or `not held`
/// for an item never granted.
pub(super) fn judged_held_after_runs(observations: &Observations, item: ItemId) -> [String; 3] {
    let record = observations.items.get(&item);
    let heard = record.into_iter().flat_map(|record| &record.heard);
    let mut placed = heard.filter_map(|heard| match &heard.event {
        ClientEvent::Placed { host } => Some((host, heard.at)),
        _ => None,
    });
    let got = match placed.next() {
        None => "not held".to_owned(),
        Some((host, at)) => {
            let ended = |run: &crate::observe::RunId| {
                let mut completions = observations.completions.iter();
                completions.any(|done| (done.run == *run) & (done.at <= at))
            };
            let before = observations.ledger.iter();
            let before = before.filter(|launch| (launch.host == *host) & (launch.at <= at));
            let open: Vec<String> = before
                .filter(|launch| !ended(&launch.run))
                .map(|launch| text::run(&launch.run))
                .collect();
            list(&open, String::clone)
        }
    };
    let name = format!("held-after-the-runs-on-its-host({})", text::item(item));
    [name, "[]".to_owned(), got]
}

#[cfg(test)]
mod tests {
    use js_types::HostId;

    use super::*;
    use crate::ledger::Completion;
    use crate::observe::RunId;

    fn ms(n: u64) -> FarmTime {
        FarmTime::from_millis(n)
    }

    fn host(name: &str) -> HostId {
        HostId::new(name).expect("a host id")
    }

    fn lease(sequence: u64) -> LeaseId {
        LeaseId::new(1, sequence)
    }

    fn booking(on: &str, sequence: u64, from: u64, until: Option<u64>) -> Booking {
        Booking {
            host: host(on),
            lease: lease(sequence),
            from: ms(from),
            until: until.map(ms),
        }
    }

    fn pending(sequence: u64) -> ClientEvent {
        ClientEvent::Pending {
            lease: lease(sequence),
        }
    }

    fn queued(sequence: u64) -> ClientEvent {
        ClientEvent::Queued {
            lease: lease(sequence),
            run: RunId::new(format!("r{sequence}")),
        }
    }

    /// An item, item 0, that tried leases 1.1 on `h000` (booked from 5 000 to 65 000, its
    /// prepare failed) and 1.2 on `h001` (booked from 65 000 to 125 000, failed too), and
    /// was granted lease 1.3 on `h000` (booked from `back` on, never ended); with item 1,
    /// which tried and was granted lease 1.4 on `h002` at once, and an ordinary item 2
    /// that heard nothing of a try.
    fn history(back: u64) -> Observations {
        let mut seen = Observations::quiet()
            .with_event(ItemId(0), ms(5_000), pending(1))
            .with_event(ItemId(0), ms(65_000), pending(2))
            .with_event(ItemId(0), ms(125_000), pending(3))
            .with_event(ItemId(0), ms(back + 10_000), queued(3))
            .with_event(
                ItemId(0),
                ms(back + 10_000),
                ClientEvent::Placed { host: host("h000") },
            )
            .with_event(ItemId(1), ms(5_000), pending(4))
            .with_event(ItemId(1), ms(15_000), queued(4))
            .with_event(ItemId(2), ms(5_000), queued(5));
        seen.bookings = vec![
            booking("h000", 1, 5_000, Some(65_000)),
            booking("h002", 4, 5_000, None),
            booking("h001", 2, 65_000, Some(125_000)),
            booking("h000", 3, back, None),
        ];
        seen
    }

    /// Asserts the count of tries: three for item 0, one for item 1, none for an item
    /// that heard no try and for an item the client never had; and each statement's
    /// name. Catches a count of the pending answers rather than of their leases, and a
    /// count that takes the granted lease alone.
    #[test]
    fn the_tries_of_an_item_are_the_leases_it_was_answered_pending_under() {
        let mut seen = history(300_000);
        seen = seen.with_event(ItemId(0), ms(125_000), pending(3));
        let tries = |item, count| judged_tries(&seen, ItemId(item), count);
        assert_eq!(tries(0, 3), ["tries(item0)", "3", "3"]);
        assert_eq!(tries(1, 3), ["tries(item1)", "3", "1"]);
        assert_eq!(tries(2, 0), ["tries(item2)", "0", "0"]);
        assert_eq!(tries(9, 1), ["tries(item9)", "1", "0"]);
    }

    /// Asserts that item 0, granted after two failed tries, counts as held after a
    /// failed try and item 1, granted at its first, does not: one or more is met, two or
    /// more is not and says it got one; and that an item that failed a try and was never
    /// granted does not count. Catches a count of items with any try, and one of items
    /// that failed whatever became of them.
    #[test]
    fn an_item_granted_after_a_failed_try_is_counted_once() {
        let seen = history(300_000);
        let held = ["held-after-a-failed-try".to_owned(), "1 or more".to_owned()];
        let one = judged_held_after_failure(&seen, 1);
        assert_eq!(one, [held[0].clone(), held[1].clone(), held[1].clone()]);
        let two = judged_held_after_failure(&seen, 2);
        assert_eq!(two, ["held-after-a-failed-try", "2 or more", "1"]);
        let mut never = Observations::quiet().with_event(ItemId(0), ms(5_000), pending(1));
        never.bookings = vec![booking("h000", 1, 5_000, Some(65_000))];
        assert_eq!(judged_held_after_failure(&never, 1)[2], "0");
    }

    /// Asserts that a try booked on the host of the item's failed try 300 s or more,
    /// less one listing read, after the failure meets a backoff of 300 s, and that one
    /// booked there sooner is named with its item, lease, host and both times; that a
    /// try on another host, the failed try itself and another item's booking are never
    /// named; and that a backoff shorter than the gap is met. Catches a check that
    /// counts the failed booking against itself, one that compares hosts of different
    /// items, and one that reads the granted try as failed.
    #[test]
    fn a_try_booked_again_on_a_failed_host_within_the_backoff_is_named() {
        let backoff = Duration::from_secs(300);
        let name = "kept-off-a-failed-host(backoff 300000)";
        let late = history(364_000);
        assert_eq!(judged_kept_off(&late, backoff), [name, "[]", "[]"]);
        let soon = history(200_000);
        let named = "[item0 booked lease 1.3 on host h000 at 200000, its try there having \
                     failed at 65000]";
        assert_eq!(judged_kept_off(&soon, backoff), [name, "[]", named]);
        let short = Duration::from_secs(120);
        let met = judged_kept_off(&soon, short);
        assert_eq!(met[1], met[2]);
    }

    /// Asserts that an item granted on `h000` after the run launched there before
    /// completed meets the statement; that a run launched there that had not completed
    /// by the grant, or completed only after it, is named; that runs on other hosts and
    /// runs launched after the grant are not; and that an item never granted got `not
    /// held`. Catches a check that reads every host's runs, one that takes a completion
    /// after the grant as before it, and one that passes an item that was never held.
    #[test]
    fn a_grant_before_a_run_on_its_host_ended_is_named() {
        let placed = ClientEvent::Placed { host: host("h000") };
        let base = Observations::quiet()
            .with_event(ItemId(0), ms(90_000), pending(1))
            .with_event(ItemId(0), ms(100_000), placed)
            .with_launch(RunId::new("a"), host("h000"), ms(20_000))
            .with_launch(RunId::new("b"), host("h001"), ms(20_000))
            .with_launch(RunId::new("c"), host("h000"), ms(110_000));
        let name = "held-after-the-runs-on-its-host(item0)";
        let ended = |at: u64| Completion {
            run: RunId::new("a"),
            host: host("h000"),
            at: ms(at),
        };
        let mut done = base.clone();
        done.completions = vec![ended(90_000)];
        assert_eq!(judged_held_after_runs(&done, ItemId(0)), [name, "[]", "[]"]);
        assert_eq!(
            judged_held_after_runs(&base, ItemId(0)),
            [name, "[]", "[run \"a\"]"]
        );
        let mut later = base;
        later.completions = vec![ended(100_001)];
        assert_eq!(judged_held_after_runs(&later, ItemId(0))[2], "[run \"a\"]");
        let never = judged_held_after_runs(&Observations::quiet(), ItemId(3));
        assert_eq!(
            never,
            ["held-after-the-runs-on-its-host(item3)", "[]", "not held"]
        );
    }

    /// Asserts how the listing's reads make the bookings: a lease first shown is
    /// appended from that read, one no longer shown is ended at that read, one still
    /// shown is left as it is, and a lease moved to another host ends on the first and
    /// starts on the second. Catches a booking ended at the wrong read, one appended
    /// again at every read, and one ended on the wrong host.
    #[test]
    fn each_read_of_the_listing_starts_and_ends_the_bookings_it_changes() {
        use crate::observe::{ServerHost, ServerView};
        let view = |listed: Vec<(&str, Vec<u64>)>| {
            ServerView::from_listing(listed.into_iter().map(|(name, leases)| {
                let entry = ServerHost {
                    leases: leases.into_iter().map(lease).collect(),
                    ..ServerHost::default()
                };
                (host(name), entry)
            }))
        };
        let mut seen = Observations::quiet();
        seen.read_listing(view(vec![("h000", vec![1]), ("h001", vec![])]), ms(1_000));
        seen.read_listing(view(vec![("h000", vec![1]), ("h001", vec![2])]), ms(2_000));
        seen.read_listing(
            view(vec![("h000", vec![]), ("h001", vec![2, 1])]),
            ms(3_000),
        );
        assert_eq!(
            seen.bookings,
            [
                booking("h000", 1, 1_000, Some(3_000)),
                booking("h001", 2, 2_000, None),
                booking("h001", 1, 3_000, None),
            ]
        );
        assert_eq!(
            seen.server,
            view(vec![("h000", vec![]), ("h001", vec![2, 1])])
        );
    }
}
