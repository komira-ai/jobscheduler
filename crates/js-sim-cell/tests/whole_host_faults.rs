//! The whole-host fault family: the ways the whole-host handshake fails, over the
//! server's whole path (its handlers, the registry and its marks, the placement, and
//! agents that answer the handshake): hosts silent on a prepare, refusals for each
//! reason, a drain behind busy hosts, a ready report racing the prepare deadline, an
//! agent that restarts while its host is committed, a cancelled request, and a held
//! lease whose agent restarts. Each runs over [`SEEDS`] seeds under every invariant of
//! `Invariant::ALL`, once per value of the server's `--placement`
//! (`Placement::value_variants()`), so that a placement that moves the whole-host path
//! onto another engine is held to the registry's answer.
//!
//! Each states only what the server does today and the design also promises: none pins
//! the order the wait list serves (design section 8, row 11), when a flapping agent's
//! deadline moves (row 26), or anything section 8 marks Open. What is a whole host comes
//! from what the client asked for, never from the server's listing.
//!
//! The scenarios and their sizes are in `cell/whole_host_faults.rs` and, for the
//! cancelled request, which the replay tests also pin, `cell/whole_host_cancel.rs`.

#[path = "cell/whole_host_cancel.rs"]
mod whole_host_cancel;
#[path = "cell/whole_host_faults.rs"]
mod whole_host_faults;

use clap::ValueEnum;
use js_lease_server::Placement;
use js_sim_cell::actions::REFUSAL_REASONS;

/// Every scenario of the family runs over this many seeds under each placement.
const SEEDS: u64 = 16;

/// Runs `scenario`'s builder under every placement the server accepts, over [`SEEDS`]
/// seeds each.
fn under_every_placement(scenario: impl Fn(Placement) -> js_sim_cell::Scenario) {
    for placement in Placement::value_variants() {
        scenario(*placement).run_seeds(SEEDS);
    }
}

/// Ten hosts, three silent on every prepare: a request committed on a silent host fails
/// at the prepare deadline and is held on a host that reported ready, the failed host is
/// left out of its tries for the backoff, and a request that meets a silent host at
/// every try is refused after `max_replace` (design section 5.4; section 7, the prepare
/// deadline).
#[test]
fn a_silent_host_fails_the_prepare_and_the_request_goes_to_another_host() {
    under_every_placement(whole_host_faults::silent_hosts);
}

/// A refused prepare, for each reason the wire has (`REFUSAL_REASONS`): the request goes
/// to another host and is held there, the refusing host is left out of its tries, and
/// only `host-unsafe` takes the host out of service (design section 5.4, the refusal;
/// no row of section 7 or 8 names it).
#[test]
fn a_refused_prepare_goes_elsewhere_for_each_refusal_reason() {
    for reason in REFUSAL_REASONS {
        under_every_placement(|placement| whole_host_faults::a_refusal(placement, reason));
    }
}

/// Every host busy with a 300 s run when a whole-host request arrives: it waits, is held
/// only after the runs on its host ended, and no run is cut (design section 6.4, "drain
/// never cuts work"; no row of section 7 or 8 names it).
#[test]
fn drain_waits_for_a_busy_host_and_cuts_no_run() {
    under_every_placement(whole_host_faults::a_drain);
}

/// The first host's agent reports ready from five seconds before the prepare deadline to
/// five after it, a scenario for each second: the lease is held on that host or the
/// request goes to another, never both, and no host books two leases (design section 7,
/// "Whole-host ready report races the prepare deadline").
#[test]
fn a_ready_report_racing_the_prepare_deadline_never_double_books() {
    for step in whole_host_faults::RACE_SWEEP {
        let delay = whole_host_faults::race_delay(step);
        under_every_placement(|placement| whole_host_faults::a_race(placement, delay));
    }
}

/// The agent of a committed host started again three times before it reports ready:
/// invariants only (design section 7, "Flapping agent during whole-host commit").
#[test]
fn an_agent_that_restarts_while_the_host_is_committed_does_not_double_book() {
    under_every_placement(whole_host_faults::restarts);
}

/// A whole-host request cancelled while queued and one cancelled while marked: both
/// lose their place and mark at once, and the host serves again (design section 8, row
/// 39, queued and marked).
#[test]
fn a_cancelled_whole_host_request_drops_its_mark_and_the_host_serves_again() {
    under_every_placement(whole_host_cancel::a_cancelled_request);
}

/// A held whole-host lease whose host's agent is started again: the host is not booked
/// to a second lease (design section 8, row 25, as far as the lease front reaches it).
#[test]
fn a_whole_host_lease_whose_agent_restarts_after_it_is_held() {
    under_every_placement(whole_host_faults::held_lease_restart);
}
