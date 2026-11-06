//! The always-on invariants of the cell, over hand-built observations.
//!
//! Each invariant has negative cases that it must flag, alone, and correct siblings it
//! must not flag, so a check that flags nothing fails the first and a check that flags
//! everything fails the second. [`NEGATIVE_CASES`] and the whole-host module's table
//! list every negative case; a new invariant without one fails
//! `every_invariant_has_a_negative_case_in_the_table`.

use std::collections::BTreeSet;
use std::time::Duration;

use js_sim_cell::{
    AgentTruth, Ask, Checked, Checker, ClientEvent, Heard, HowEnded, Invariant, ItemId, Launch,
    NotSettled, Observations, RunId, Running, ServerHost, Touched, Verdict, Violation,
};
use js_types::sched::FarmTime;
use js_types::{HostId, LeaseId, Resources};

// The boundary cases, a module of this test binary (not a test target of its own).
#[path = "invariants/boundaries.rs"]
mod boundaries;
// The invariants of the fault family, a module of this test binary too.
#[path = "invariants/faults.rs"]
mod faults;
// The whole-host invariants' cases, likewise, and the job invariants'.
#[path = "invariants/jobs.rs"]
mod jobs;
#[path = "invariants/whole_host.rs"]
mod whole_host;

const GIB: u64 = 1 << 30;

/// The quiet time every end check here waits for.
const SETTLE: Duration = Duration::from_secs(1);

/// The virtual time of every end case: past `SETTLE` from a run quiet since zero.
const END: u64 = 5_000;

fn host(n: u64) -> HostId {
    HostId::new(&format!("host-{n}")).expect("host id")
}

fn lease(n: u64) -> LeaseId {
    LeaseId::new(1, n)
}

fn run(n: u64) -> RunId {
    RunId::new(format!("run-{n}"))
}

fn ms(n: u64) -> FarmTime {
    FarmTime::from_millis(n)
}

fn res(cpu_millis: u64, memory_bytes: u64) -> Resources {
    Resources::new(cpu_millis, memory_bytes, 0, None).expect("resources")
}

fn res_gpus(cpu_millis: u64, memory_bytes: u64, gpus: u64) -> Resources {
    Resources::new(cpu_millis, memory_bytes, gpus, Some("gpu-a".to_owned())).expect("resources")
}

/// Every host here: 4 cores, 8 GiB, one GPU.
fn capacity() -> Resources {
    res_gpus(4_000, 8 * GIB, 1)
}

fn share(cpu_millis: u64) -> Ask {
    Ask {
        whole_host: false,
        resources: res(cpu_millis, GIB),
        gives_up_at: ms(60_000),
    }
}

fn whole() -> Ask {
    Ask {
        whole_host: true,
        resources: capacity(),
        gives_up_at: ms(60_000),
    }
}

fn queued(n: u64) -> ClientEvent {
    ClientEvent::Queued {
        lease: lease(n),
        run: run(n),
    }
}

fn placed(n: u64) -> ClientEvent {
    ClientEvent::Placed { host: host(n) }
}

fn ended(how: HowEnded) -> ClientEvent {
    ClientEvent::Terminal { how }
}

/// A fresh checker's step over everything in `observations`.
fn step_all(observations: &Observations) -> Vec<Violation> {
    Checker::new().step(observations, &Touched::everything_in(observations))
}

/// The end check, which must run.
fn end(observations: &Observations) -> Vec<Violation> {
    Checker::end(observations, SETTLE).expect("the case is settled")
}

/// The invariants named in `found`, each once.
fn flagged(found: &[Violation]) -> Vec<Invariant> {
    let names: BTreeSet<Invariant> = found.iter().map(|v| v.invariant).collect();
    names.into_iter().collect()
}

/// Asserts that `found` is empty, printing its lines if not.
fn none(found: &[Violation]) {
    assert_eq!(lines(found), Vec::<String>::new());
}

/// Each violation's one line.
fn lines(found: &[Violation]) -> Vec<String> {
    found.iter().map(ToString::to_string).collect()
}

// ---- The negative cases, one function each, gathered in NEGATIVE_CASES. ----

/// Host 0 runs two runs needing 5 cores of its 4.
fn past_capacity() -> Observations {
    two_runs(res(2_000, GIB), res(3_000, GIB))
}

/// Host 0 at virtual time 300 running run 0 and run 1, of leases 0 and 1.
fn two_runs(first: Resources, second: Resources) -> Observations {
    let agent = AgentTruth::new(capacity())
        .with_run(run(0), lease(0), first)
        .with_run(run(1), lease(1), second);
    Observations::quiet().at(ms(300)).with_agent(host(0), agent)
}

/// Run 0 runs on host 0 and on host 1 at once.
fn one_run_on_two_agents() -> Observations {
    let agent = || AgentTruth::new(capacity()).with_run(run(0), lease(0), res(1_000, GIB));
    Observations::quiet()
        .at(ms(300))
        .with_agent(host(0), agent())
        .with_agent(host(1), agent())
}

/// Whole-host item 0 holds lease 0 on host 0, by what the client heard, while host 0
/// runs run 1 of lease 1.
fn whole_host_beside_another_lease() -> Observations {
    whole_item_queued()
        .with_event(ItemId(0), ms(250), placed(0))
        .with_agent(host(0), runs_lease_one())
}

/// Whole-host item 0, submitted and queued with lease 0, at virtual time 500; host 0's
/// agent reported ready for lease 0 at 150.
fn whole_item_queued() -> Observations {
    Observations::quiet()
        .at(ms(500))
        .with_item(ItemId(0), whole())
        .with_event(ItemId(0), ms(100), ClientEvent::Submitted)
        .with_ready(host(0), "1-0", ms(150))
        .with_event(ItemId(0), ms(200), queued(0))
}

/// An agent running run 1 of lease 1.
fn runs_lease_one() -> AgentTruth {
    AgentTruth::new(capacity()).with_run(run(1), lease(1), res(1_000, GIB))
}

/// Whole-host item 0's lease 0 is booked on host 0 by the server's listing (the client
/// has not yet heard where it was placed) while host 0 runs run 1 of lease 1.
fn whole_host_listed_beside_another_lease() -> Observations {
    whole_item_queued()
        .with_booking(host(0), lease(0))
        .with_agent(host(0), runs_lease_one())
}

/// Run 0 launched on host 0, then on host 1.
fn launched_twice() -> Observations {
    Observations::quiet()
        .at(ms(300))
        .with_launch(run(0), host(0), ms(100))
        .with_launch(run(0), host(1), ms(200))
}

/// Run 0 launched on host 0, and again on host 0 by the process that replaced the
/// first: the replacement holds only its own launch.
fn launched_again_by_a_replacement() -> Observations {
    let replacement = AgentTruth::new(capacity()).with_run(run(0), lease(0), res(1_000, GIB));
    Observations::quiet()
        .at(ms(600))
        .with_launch(run(0), host(0), ms(100))
        .with_agent(host(0), replacement)
        .with_launch(run(0), host(0), ms(500))
}

/// Item 0 through to its first terminal answer, `how`, at virtual time 1000; its run
/// completed at 900 when it succeeded.
fn item_ended(how: HowEnded) -> Observations {
    let observations = Observations::quiet()
        .at(ms(1_500))
        .with_item(ItemId(0), share(1_000))
        .with_event(ItemId(0), ms(100), ClientEvent::Submitted)
        .with_event(ItemId(0), ms(200), queued(0))
        .with_event(ItemId(0), ms(300), placed(0))
        .with_event(ItemId(0), ms(400), ClientEvent::Started)
        .with_event(ItemId(0), ms(1_000), ended(how));
    let done = (how == HowEnded::Succeeded).then(|| (run(0), host(0), ms(900)));
    done.into_iter()
        .fold(observations, |o, (r, h, at)| o.with_completion(r, h, at))
}

/// Item 0 succeeded, then heard it was lost.
fn second_terminal() -> Observations {
    item_ended(HowEnded::Succeeded).with_event(ItemId(0), ms(1_200), ended(HowEnded::Lost))
}

/// Item 0 failed, then heard it started.
fn started_after_terminal() -> Observations {
    item_ended(HowEnded::Failed).with_event(ItemId(0), ms(1_100), ClientEvent::Started)
}

/// Item 0 succeeded and never released its lease.
fn lease_held_at_end() -> Observations {
    item_ended(HowEnded::Succeeded).at(ms(END))
}

/// Item 0 was submitted and never heard of again.
fn unfinished_at_end() -> Observations {
    Observations::quiet()
        .at(ms(END))
        .with_item(ItemId(0), share(1_000))
        .with_event(ItemId(0), ms(100), ClientEvent::Submitted)
}

/// Item 0 ended and released, but host 0 still books lease 0 and still runs its run.
fn host_booked_at_end() -> Observations {
    released(HowEnded::Succeeded)
        .with_booking(host(0), lease(0))
        .with_agent(host(0), runs_lease_zero())
}

/// Item 0 ended `how` and released its lease, at the end time.
fn released(how: HowEnded) -> Observations {
    item_ended(how)
        .with_event(ItemId(0), ms(1_050), ClientEvent::ReleaseSent)
        .with_event(ItemId(0), ms(1_100), ClientEvent::Released)
        .at(ms(END))
}

/// An agent running run 0 of lease 0.
fn runs_lease_zero() -> AgentTruth {
    AgentTruth::new(capacity()).with_run(run(0), lease(0), res(1_000, GIB))
}

/// Host 0 books lease 0 and its agent runs nothing.
fn booking_no_agent_holds() -> Observations {
    Observations::quiet()
        .at(ms(END))
        .with_booking(host(0), lease(0))
        .with_agent(host(0), AgentTruth::new(capacity()))
}

/// Host 0's agent runs run 0 of lease 0 and the server lists host 0 with no booking.
fn run_nothing_books() -> Observations {
    let mut observations = Observations::quiet()
        .at(ms(END))
        .with_agent(host(0), runs_lease_zero());
    observations
        .server
        .hosts
        .insert(host(0), ServerHost::default());
    observations
}

/// One row of [`NEGATIVE_CASES`]: an invariant and a case that must trip it alone.
type NegativeCase = (Invariant, fn() -> Observations);

/// Every negative case and the one invariant it must trip. A step case is checked by a
/// fresh checker's step over everything in it; an end case by that step and the end
/// check.
const NEGATIVE_CASES: &[NegativeCase] = &[
    (Invariant::NoDoubleBooking, past_capacity),
    (Invariant::NoDoubleBooking, one_run_on_two_agents),
    (Invariant::NoDoubleBooking, whole_host_beside_another_lease),
    (
        Invariant::NoDoubleBooking,
        whole_host_listed_beside_another_lease,
    ),
    (Invariant::AtMostOneLaunch, launched_twice),
    (Invariant::AtMostOneLaunch, launched_again_by_a_replacement),
    (Invariant::OneTerminalAnswer, second_terminal),
    (Invariant::OneTerminalAnswer, started_after_terminal),
    (Invariant::SettlesWhenQuiet, lease_held_at_end),
    (Invariant::SettlesWhenQuiet, unfinished_at_end),
    (Invariant::SettlesWhenQuiet, host_booked_at_end),
    (Invariant::BookingsMatchAgents, booking_no_agent_holds),
    (Invariant::BookingsMatchAgents, run_nothing_books),
    (Invariant::NoDoubleBooking, faults::lease_on_two_hosts),
    (Invariant::NoDoubleBooking, faults::host_listed_twice),
    (Invariant::NoLeaseOnALostHost, faults::placed_on_a_lost_host),
    (Invariant::NoResultLost, faults::lost_result),
    (Invariant::NoResultLost, faults::result_accepted_twice),
    (Invariant::NoResultLost, faults::success_never_produced),
];

/// What a case of the table is flagged for: the step check over everything, and the end
/// check too for an end invariant's case.
fn verdict(invariant: Invariant, observations: &Observations) -> Vec<Violation> {
    let mut found = step_all(observations);
    if invariant.checked() == Checked::AtEnd {
        found.extend(end(observations));
    }
    found
}

// ---- The tests. ----

/// A cell under test: the observations, one checker following them, and everything it
/// flagged.
struct Cell {
    observations: Observations,
    checker: Checker,
    found: Vec<Violation>,
}

impl Cell {
    /// Moves to virtual time `at`, applies `change`, and checks the step it touched.
    fn step(&mut self, at: u64, touched: Touched, change: impl FnOnce(&mut Observations)) {
        self.observations.at = ms(at);
        change(&mut self.observations);
        let found = self.checker.step(&self.observations, &touched);
        self.found.extend(found);
    }
}

fn hear(o: &mut Observations, item: u32, event: ClientEvent) {
    let heard = Heard { at: o.at, event };
    o.items.entry(ItemId(item)).or_default().heard.push(heard);
}

/// Host `h`'s agent launches item `n`'s run (run `n`, lease `n`) with the item's asked
/// resources, and the ledger records it.
fn start(o: &mut Observations, h: u64, n: u64) {
    let item = ItemId(u32::try_from(n).expect("item"));
    let resources = o.items[&item].ask.resources.clone();
    let running = Running {
        lease: lease(n),
        resources,
    };
    let agent = o.agents.get_mut(&host(h)).expect("agent");
    agent.running.insert(run(n), running);
    let launch = Launch {
        run: run(n),
        host: host(h),
        at: o.at,
    };
    o.ledger.push(launch);
}

/// Host `h`'s agent completes run `n`, and the ledger records the completion.
fn finish(o: &mut Observations, h: u64, n: u64) {
    let at = o.at;
    let agent = o.agents.get_mut(&host(h)).expect("agent");
    agent.running.remove(&run(n));
    agent.completed.insert(run(n), at);
    *o = std::mem::take(o).with_completion(run(n), host(h), at);
}

/// The server acknowledges run `n`'s result on host `h`.
fn ack(o: &mut Observations, h: u64, n: u64, verdict: Verdict) {
    let agent = o.agents.get_mut(&host(h)).expect("agent");
    agent.acknowledged.insert(run(n), verdict);
}

fn book(o: &mut Observations, h: u64, n: u64) {
    let listed = o.server.hosts.get_mut(&host(h)).expect("listed");
    listed.leases.push(lease(n));
}

fn unbook(o: &mut Observations, h: u64, n: u64) {
    let listed = o.server.hosts.get_mut(&host(h)).expect("listed");
    listed.leases.retain(|l| *l != lease(n));
}

/// A step that touched `items`.
fn items(items: &[u32]) -> Touched {
    let touched = Touched::nothing();
    items.iter().fold(touched, |t, i| t.item(ItemId(*i)))
}

/// Asserts that a correct history of 3 agents and 6 items passes every step check, step
/// by step with only what each step touched, and the end check. Item 0 and item 1 share
/// host 0 exactly to its capacity; whole-host item 2 holds host 1, and item 3 is placed
/// there after item 2 released it; item 4 gives up while queued; host 2's agent dies
/// under item 5's run and is replaced, and item 5 is lost. Catches an invariant that is
/// over-strict: one that flags a correct history.
#[test]
fn a_clean_history_passes_every_invariant() {
    let mut cell = Cell {
        observations: Observations::quiet(),
        checker: Checker::new(),
        found: Vec::new(),
    };
    let agents = Touched::nothing()
        .agent(host(0))
        .agent(host(1))
        .agent(host(2));
    cell.step(0, agents.server(), |o| {
        for h in 0..3 {
            o.agents.insert(host(h), AgentTruth::new(capacity()));
            o.server.hosts.insert(host(h), ServerHost::default());
        }
    });
    cell.step(100, items(&[0, 1, 2, 3, 4, 5]), |o| {
        let asks = [
            share(1_000),
            share(3_000),
            whole(),
            share(2_000),
            share(1_000),
            share(1_000),
        ];
        for (i, ask) in (0..).zip(asks) {
            o.items.entry(ItemId(i)).or_default().ask = ask;
            hear(o, i, ClientEvent::Submitted);
        }
    });
    // The server books item 2's whole-host lease on host 1 before the client learns
    // where it was placed, and host 1's agent reports ready for it.
    cell.step(200, items(&[0, 1, 2, 4, 5]).server(), |o| {
        *o = std::mem::take(o).with_ready(host(1), "1-2", ms(200));
        for (i, h) in [(0, 0), (1, 0), (5, 2)] {
            hear(o, i, queued(u64::from(i)));
            hear(o, i, placed(h));
            book(o, h, u64::from(i));
        }
        hear(o, 2, queued(2));
        book(o, 1, 2);
        hear(o, 4, queued(4));
    });
    cell.step(250, items(&[2]), |o| hear(o, 2, placed(1)));
    let agents = Touched::nothing()
        .agent(host(0))
        .agent(host(1))
        .agent(host(2));
    cell.step(300, agents, |o| {
        start(o, 0, 0);
        start(o, 0, 1);
        start(o, 1, 2);
        start(o, 2, 5);
    });
    cell.step(400, items(&[0, 1, 2, 5]).agent(host(0)), |o| {
        for i in [0, 1, 2, 5] {
            hear(o, i, ClientEvent::Started);
        }
    });
    cell.step(1_000, items(&[0]).agent(host(0)), |o| {
        finish(o, 0, 0);
        hear(o, 0, ended(HowEnded::Succeeded));
    });
    cell.step(1_100, items(&[0]).agent(host(0)).server(), |o| {
        ack(o, 0, 0, Verdict::Accepted);
        hear(o, 0, ClientEvent::ReleaseSent);
        hear(o, 0, ClientEvent::Released);
        unbook(o, 0, 0);
    });
    cell.step(1_500, items(&[2]).agent(host(1)), |o| {
        finish(o, 1, 2);
        hear(o, 2, ended(HowEnded::Succeeded));
    });
    cell.step(1_600, items(&[2]).agent(host(1)).server(), |o| {
        ack(o, 1, 2, Verdict::Accepted);
        hear(o, 2, ClientEvent::ReleaseSent);
        hear(o, 2, ClientEvent::Released);
        unbook(o, 1, 2);
    });
    cell.step(1_700, items(&[3]).server(), |o| {
        hear(o, 3, queued(3));
        hear(o, 3, placed(1));
        book(o, 1, 3);
    });
    cell.step(1_800, items(&[3]).agent(host(1)), |o| {
        start(o, 1, 3);
        hear(o, 3, ClientEvent::Started);
    });
    // Host 2's agent process dies; a replacement comes up holding nothing.
    cell.step(1_900, Touched::nothing().agent(host(2)), |o| {
        o.agents.remove(&host(2));
    });
    cell.step(2_000, items(&[5]).agent(host(2)).server(), |o| {
        o.agents.insert(host(2), AgentTruth::new(capacity()));
        hear(o, 5, ended(HowEnded::Lost));
        hear(o, 5, ClientEvent::ReleaseSent);
        hear(o, 5, ClientEvent::Released);
        unbook(o, 2, 5);
    });
    cell.step(2_100, items(&[4]), |o| {
        hear(o, 4, ended(HowEnded::GaveUp));
        hear(o, 4, ClientEvent::ReleaseSent);
        hear(o, 4, ClientEvent::Released);
    });
    let rest = items(&[1, 3]).agent(host(0)).agent(host(1)).server();
    cell.step(2_500, rest, |o| {
        finish(o, 0, 1);
        // Item 3's run fails: it ends with no completion.
        let failing = o.agents.get_mut(&host(1)).expect("agent");
        failing.running.remove(&run(3));
        hear(o, 1, ended(HowEnded::Succeeded));
        hear(o, 3, ended(HowEnded::Failed));
        ack(o, 0, 1, Verdict::Accepted);
        ack(o, 1, 3, Verdict::Duplicate);
        hear(o, 1, ClientEvent::ReleaseSent);
        hear(o, 1, ClientEvent::Released);
        hear(o, 3, ClientEvent::ReleaseSent);
        hear(o, 3, ClientEvent::Released);
        unbook(o, 0, 1);
        unbook(o, 1, 3);
    });
    assert_eq!(lines(&cell.found), Vec::<String>::new());
    assert_eq!(lines(&step_all(&cell.observations)), Vec::<String>::new());
    assert_eq!(lines(&end(&cell.observations)), Vec::<String>::new());
}

/// Asserts that runs needing more than their host's capacity are flagged as
/// `NoDoubleBooking`, on each axis alone, with the line naming the host, its runs, what
/// they need, the capacity and the axis. Catches a capacity check that is missing, that
/// compares only one axis, or that sums only the newest run.
#[test]
fn a_run_past_the_capacity_of_its_host_is_flagged_as_double_booking() {
    assert_eq!(
        lines(&step_all(&past_capacity())),
        [
            "no-double-booking at 300: host host-0 runs [run \"run-0\"; run \"run-1\"] needing \
             res(cpu=5000 mem=2147483648 gpus=0) beyond its capacity \
             res(cpu=4000 mem=8589934592 gpus=1) (over on cpu)"
        ]
    );
    let memory = two_runs(res(1_000, GIB), res(1_000, 8 * GIB));
    let gpus = two_runs(res_gpus(1_000, GIB, 1), res_gpus(1_000, GIB, 1));
    for (axis, case) in [("mem", memory), ("gpus", gpus)] {
        let found = step_all(&case);
        assert_eq!(flagged(&found), [Invariant::NoDoubleBooking], "{axis}");
        assert!(
            found[0].detail.ends_with(&format!("(over on {axis})")),
            "{}",
            found[0]
        );
    }
}

/// Asserts that runs needing exactly their host's capacity on every axis are not
/// flagged. Catches a capacity check that compares with `>=`.
#[test]
fn a_run_exactly_at_capacity_is_not_flagged() {
    let case = two_runs(res(1_000, 2 * GIB), res_gpus(3_000, 6 * GIB, 1));
    assert_eq!(lines(&step_all(&case)), Vec::<String>::new());
}

/// Asserts that one run on two agents at the same time is flagged, both when one step
/// shows both agents and when the second agent starts it at a later step that touches
/// only the second agent. Catches a check that compares only the agents a step touched
/// with each other, losing the run seen on an agent at an earlier step.
#[test]
fn one_run_on_two_agents_at_once_is_flagged() {
    assert_eq!(
        lines(&step_all(&one_run_on_two_agents())),
        ["no-double-booking at 300: run \"run-0\" runs on host host-0 and on host host-1"]
    );
    let mut checker = Checker::new();
    let first = Observations::quiet()
        .at(ms(100))
        .with_agent(host(0), runs_lease_zero());
    let touched = Touched::nothing().agent(host(0));
    none(&checker.step(&first, &touched));
    let second = first.at(ms(200)).with_agent(host(1), runs_lease_zero());
    let found = checker.step(&second, &Touched::nothing().agent(host(1)));
    assert_eq!(
        lines(&found),
        ["no-double-booking at 200: run \"run-0\" runs on host host-0 and on host host-1"]
    );
}

/// Asserts that a run that ended on one agent and then runs on another is not flagged
/// by `NoDoubleBooking`, although the step at which the first agent stopped it was
/// never checked (the checker's memory of the first agent is stale). Catches a check
/// that trusts its index of runs without asking whether the first agent still runs it.
/// (A second launch of one run is `AtMostOneLaunch`'s; this history has no ledger.)
#[test]
fn a_run_that_moved_after_the_first_ended_is_not_flagged() {
    let mut checker = Checker::new();
    let first = Observations::quiet()
        .at(ms(100))
        .with_agent(host(0), runs_lease_zero());
    none(&checker.step(&first, &Touched::nothing().agent(host(0))));
    let ended = first.with_agent(host(0), AgentTruth::new(capacity()));
    let moved = ended.at(ms(300)).with_agent(host(1), runs_lease_zero());
    none(&checker.step(&moved, &Touched::nothing().agent(host(1))));
}

/// Asserts that a run of another lease beside a whole-host item's lease is flagged,
/// whether the client heard the item placed on the host or only the server's listing
/// books it there; and that the listing is read for it only at a step that touched the
/// server. Catches a whole-host check that reads the server's word for what is
/// whole-host, one that ignores either source of the hold, and one that reads the
/// listing at every step.
#[test]
fn a_whole_host_item_beside_another_leases_run_is_flagged() {
    let expected = "no-double-booking at 500: host host-0 runs run \"run-1\" of lease 1.1 \
                    while whole-host item0 holds lease 1.0 there";
    assert_eq!(
        lines(&step_all(&whole_host_beside_another_lease())),
        [expected]
    );
    let listed = whole_host_listed_beside_another_lease();
    assert_eq!(lines(&step_all(&listed)), [expected]);
    let agent_only = Touched::nothing().item(ItemId(0)).agent(host(0));
    none(&Checker::new().step(&listed, &agent_only));
    // A share item's lease listed on the host is no hold.
    let mut share_item = listed;
    share_item.items.get_mut(&ItemId(0)).expect("item").ask = share(1_000);
    none(&step_all(&share_item));
}

/// Asserts that once the whole-host item released its lease, another lease's run on
/// the host is not flagged, step by step: the item holds the host with its own run, the
/// run ends and the item releases, then a run of lease 1 starts there. Catches a hold
/// that outlives the client's release, and a check that flags the whole-host item's
/// own run.
#[test]
fn a_second_lease_on_a_host_after_the_whole_host_item_released_is_not_flagged() {
    let mut checker = Checker::new();
    let held = whole_host_beside_another_lease().with_agent(host(0), runs_lease_zero());
    let everything = Touched::everything_in(&held);
    none(&checker.step(&held, &everything));
    let released = held
        .with_event(ItemId(0), ms(600), ended(HowEnded::Succeeded))
        .with_event(ItemId(0), ms(650), ClientEvent::ReleaseSent)
        .with_event(ItemId(0), ms(700), ClientEvent::Released)
        .with_agent(host(0), AgentTruth::new(capacity()))
        .at(ms(700));
    none(&checker.step(&released, &everything));
    let next = released
        .with_booking(host(0), lease(1))
        .with_agent(host(0), runs_lease_one())
        .at(ms(800));
    none(&checker.step(&next, &everything));
}

/// The line of host 0 running run 1 of lease 1 beside whole-host item 0's lease 0, at
/// virtual time `at`.
fn whole_conflict(at: u64) -> String {
    format!(
        "no-double-booking at {at}: host host-0 runs run \"run-1\" of lease 1.1 while \
         whole-host item0 holds lease 1.0 there"
    )
}

/// Asserts that a whole-host item whose run succeeded but which sent no release still
/// holds its host by the client's facts: another lease's run there is flagged at a step
/// that touched only the agent. Catches a hold that ends at the terminal answer, which
/// would leave an unreleased whole-host lease unguarded between listing reads.
#[test]
fn a_whole_host_item_that_ended_without_a_release_still_holds_its_host() {
    let mut checker = Checker::new();
    let ended = whole_item_queued()
        .with_event(ItemId(0), ms(250), placed(0))
        .with_event(ItemId(0), ms(400), ended(HowEnded::Succeeded));
    none(&checker.step(&ended, &Touched::nothing().item(ItemId(0))));
    let beside = ended.with_agent(host(0), runs_lease_one());
    let found = checker.step(&beside, &Touched::nothing().agent(host(0)));
    assert_eq!(lines(&found), [whole_conflict(500)]);
}

/// Asserts that a whole-host item stops holding its host, by the client's facts, once
/// the client sent its release (whose answer a fault dropped) or heard that the server
/// ended the run (lost, forgotten): another lease placed on the freed host is then not
/// flagged. Asserts too that while the server's listing still books the item's lease,
/// another lease's run there is flagged at a step that re-read the listing. Catches a
/// hold that waits for a release answer a fault can drop (a false failure), and one
/// that ends on the server's side too.
#[test]
fn a_whole_host_item_with_a_lost_release_answer_frees_its_host() {
    let placed_item = whole_item_queued().with_event(ItemId(0), ms(250), placed(0));
    let ends = [
        ClientEvent::ReleaseSent,
        ended(HowEnded::Lost),
        ended(HowEnded::Forgotten),
    ];
    for event in ends {
        let mut checker = Checker::new();
        let freed = placed_item
            .clone()
            .with_event(ItemId(0), ms(400), event)
            .with_booking(host(0), lease(1));
        none(&checker.step(&freed, &Touched::nothing().item(ItemId(0))));
        let next = freed.with_agent(host(0), runs_lease_one());
        none(&checker.step(&next, &Touched::nothing().agent(host(0))));
        let still_listed = next.with_booking(host(0), lease(0));
        let found = checker.step(&still_listed, &Touched::nothing().server());
        assert_eq!(lines(&found), [whole_conflict(500)]);
    }
}

/// Asserts that a whole-host conflict is found at the step that reveals it, even when
/// that step touched only the item (the client learned the placement) or only the
/// server (the listing now books the item's lease), and the conflicting agent was
/// checked at an earlier step; and that a placement or a booking on another host is
/// not flagged. Catches a check that looks only at the agents a step touched.
#[test]
fn a_whole_host_conflict_revealed_by_an_item_or_a_listing_alone_is_flagged() {
    let busy = whole_item_queued().with_agent(host(0), runs_lease_one());
    let first = Touched::nothing().item(ItemId(0)).agent(host(0));

    let mut checker = Checker::new();
    none(&checker.step(&busy, &first));
    let placed_here = busy.clone().with_event(ItemId(0), ms(250), placed(0));
    let found = checker.step(&placed_here, &Touched::nothing().item(ItemId(0)));
    assert_eq!(lines(&found), [whole_conflict(500)]);

    let mut checker = Checker::new();
    none(&checker.step(&busy, &first));
    let placed_there = busy.clone().with_ready(host(1), "1-0", ms(240));
    let placed_there = placed_there.with_event(ItemId(0), ms(250), placed(1));
    none(&checker.step(&placed_there, &Touched::nothing().item(ItemId(0))));

    let mut checker = Checker::new();
    none(&checker.step(&busy, &first));
    let listed_here = busy.clone().with_booking(host(0), lease(0));
    let found = checker.step(&listed_here, &Touched::nothing().server());
    assert_eq!(lines(&found), [whole_conflict(500)]);
    let mut unlisted = listed_here;
    unlisted.server.hosts.clear();
    none(&checker.step(&unlisted, &Touched::nothing().server()));

    let mut checker = Checker::new();
    none(&checker.step(&busy, &first));
    let listed_there = busy.with_booking(host(1), lease(0));
    none(&checker.step(&listed_there, &Touched::nothing().server()));
}

/// Asserts that a run launched on two hosts is flagged as `AtMostOneLaunch`, with the
/// line naming both launches. Catches a ledger check that is missing, or that counts
/// launches per host.
#[test]
fn a_second_launch_of_one_run_is_flagged() {
    assert_eq!(
        lines(&step_all(&launched_twice())),
        [
            "at-most-one-launch at 300: run \"run-0\" launched on host host-1 at 200, \
             already launched on host host-0 at 100"
        ]
    );
}

/// Asserts that a run launched again on the same host by the process that replaced the
/// one that died is flagged at the step the ledger records it, and is reported once:
/// the next step, which appends nothing, flags nothing. Catches a launch count kept in
/// the agent's state (lost with the process) instead of the cell's ledger, and a check
/// that re-reads the whole ledger at every step.
#[test]
fn a_second_launch_by_a_replacement_process_is_flagged() {
    let mut checker = Checker::new();
    let first = Observations::quiet()
        .at(ms(100))
        .with_agent(host(0), runs_lease_zero())
        .with_launch(run(0), host(0), ms(100));
    let touched = Touched::nothing().agent(host(0));
    none(&checker.step(&first, &touched));
    let replaced = first
        .with_agent(host(0), runs_lease_zero())
        .with_launch(run(0), host(0), ms(500))
        .at(ms(600));
    assert_eq!(
        lines(&checker.step(&replaced, &touched)),
        [
            "at-most-one-launch at 600: run \"run-0\" launched on host host-0 at 500, \
             already launched on host host-0 at 100"
        ]
    );
    none(&checker.step(&replaced, &Touched::nothing()));
    assert_eq!(
        flagged(&step_all(&launched_again_by_a_replacement())),
        [Invariant::AtMostOneLaunch]
    );
}

/// Asserts that a second terminal answer is flagged as `OneTerminalAnswer`, naming
/// both answers and their times. Catches a check that counts only `Started` after a
/// terminal.
#[test]
fn a_second_terminal_answer_is_flagged() {
    assert_eq!(
        lines(&step_all(&second_terminal())),
        [
            "one-terminal-answer at 1500: item0 heard terminal(lost) at 1200 after \
             terminal(succeeded) at 1000"
        ]
    );
}

/// Asserts that `Started` after a terminal answer is flagged as `OneTerminalAnswer`,
/// and that a release after the terminal is not. Catches a check that counts only
/// terminals, and one that flags any event after a terminal.
#[test]
fn a_terminal_item_that_reads_started_again_is_flagged() {
    assert_eq!(
        lines(&step_all(&started_after_terminal())),
        ["one-terminal-answer at 1500: item0 heard started at 1100 after terminal(failed) at 1000"]
    );
    none(&step_all(&released(HowEnded::Failed)));
}

/// Asserts that an item that never released its lease is flagged at the end as
/// `SettlesWhenQuiet`. Catches an end check that takes a terminal answer for a
/// released lease.
#[test]
fn a_lease_still_held_at_the_end_is_flagged() {
    assert_eq!(
        lines(&end(&lease_held_at_end())),
        ["settles-when-quiet at 5000: item0 still holds lease 1.0"]
    );
}

/// Asserts that an item with no terminal answer is flagged at the end as
/// `SettlesWhenQuiet`, and that an item the client gave up on (and released) is not,
/// whatever else it heard. Catches an end check that misses an unfinished item, and one
/// that does not count `GaveUp` as terminal.
#[test]
fn an_unfinished_item_that_was_not_given_up_is_flagged() {
    assert_eq!(
        lines(&end(&unfinished_at_end())),
        ["settles-when-quiet at 5000: item0 is not terminal"]
    );
    let given_up = unfinished_at_end()
        .with_event(ItemId(0), ms(200), queued(0))
        .with_event(ItemId(0), ms(4_000), ended(HowEnded::GaveUp))
        .with_event(ItemId(0), ms(4_000), ClientEvent::ReleaseSent)
        .with_event(ItemId(0), ms(4_000), ClientEvent::Released);
    none(&end(&given_up));
}

/// Asserts that a host whose listing still books a lease at the end, while its agent
/// still runs that lease's run, is flagged as `SettlesWhenQuiet` and not as
/// `BookingsMatchAgents` (the two sides agree). Catches an end check that reads the
/// client's release as the end of the booking.
#[test]
fn a_host_still_booked_at_the_end_is_flagged() {
    assert_eq!(
        lines(&end(&host_booked_at_end())),
        ["settles-when-quiet at 5000: host host-0 still books lease 1.0"]
    );
}

/// Asserts that the end check refuses to run, rather than passing, while a fault is
/// active and until `settle_within` has passed since the last fault, and that it runs
/// from that moment on. The observations hold an unfinished item, so an end check that
/// ran would flag it. Catches an end check evaluated mid-fault, where unfinished work is
/// expected, and an off-by-one at the settle boundary.
#[test]
fn an_end_check_is_not_evaluated_while_a_fault_is_active() {
    let faulted = unfinished_at_end().with_fault();
    let refused = Checker::end(&faulted, SETTLE);
    let expected = NotSettled {
        at: ms(END),
        fault_active: true,
        settles_at: ms(1_000),
    };
    assert_eq!(refused, Err(expected));
    assert_eq!(
        expected.to_string(),
        "end check refused at 5000: a fault is active"
    );
    let early = unfinished_at_end().quiet_since(ms(4_500));
    let expected = NotSettled {
        at: ms(END),
        fault_active: false,
        settles_at: ms(5_500),
    };
    assert_eq!(Checker::end(&early, SETTLE), Err(expected));
    assert_eq!(
        expected.to_string(),
        "end check refused at 5000: not quiet long enough until 5500"
    );
    let settled = early.at(ms(5_500));
    assert_eq!(flagged(&end(&settled)), [Invariant::SettlesWhenQuiet]);
}

/// Asserts that a booking on the server's listing that the host's agent does not hold
/// is flagged as `BookingsMatchAgents` (and only there, not a second time as
/// `SettlesWhenQuiet`), whether the agent runs nothing or there is no agent at all.
/// Catches a leak detector that does not read the listing, or one that counts a
/// booking as held because some agent exists.
#[test]
fn a_booking_no_agent_holds_is_flagged() {
    let expected = "bookings-match-agents at 5000: host host-0 books lease 1.0 and its agent \
                    runs no run of it";
    assert_eq!(lines(&end(&booking_no_agent_holds())), [expected]);
    let mut no_agent = booking_no_agent_holds();
    no_agent.agents.clear();
    assert_eq!(lines(&end(&no_agent)), [expected]);
}

/// Asserts that a run on an agent that the server's listing of that host does not book
/// is flagged as `BookingsMatchAgents`, and that a booking of the same lease on another
/// host does not excuse it. Catches a leak detector that reads only the server's side,
/// or that matches leases across hosts.
#[test]
fn a_run_nothing_books_is_flagged() {
    let expected = "bookings-match-agents at 5000: host host-0 runs run \"run-0\" of lease 1.0 \
                    and the server books no such lease there";
    assert_eq!(lines(&end(&run_nothing_books())), [expected]);
    let elsewhere = run_nothing_books().with_booking(host(1), lease(0));
    let booked_elsewhere = "bookings-match-agents at 5000: host host-1 books lease 1.0 and its \
                            agent runs no run of it";
    assert_eq!(lines(&end(&elsewhere)), [booked_elsewhere, expected]);
}

/// Asserts the cost rule: a step that touched one agent is checked against that
/// agent's facts and the checker's indices, not by a scan of every host. Of 200 agents,
/// 199 run more than their capacity; a step touching the one correct agent passes,
/// while a step over everything flags all 199. No timing is asserted.
#[test]
fn a_step_that_touched_one_agent_is_not_checked_against_every_agent() {
    let mut observations = Observations::quiet()
        .at(ms(300))
        .with_agent(host(0), runs_lease_zero());
    for h in 1..200 {
        let over = AgentTruth::new(capacity()).with_run(run(h), lease(h), res(5_000, GIB));
        observations = observations.with_agent(host(h), over);
    }
    let touched = Touched::nothing().agent(host(0));
    none(&Checker::new().step(&observations, &touched));
    let full = step_all(&observations);
    assert_eq!(full.len(), 199);
    assert_eq!(flagged(&full), [Invariant::NoDoubleBooking]);
}

/// Asserts that every member of `Invariant::ALL` has a negative case in
/// `NEGATIVE_CASES` or the whole-host or job module's table, that no two members share a name, and that each case is flagged
/// by exactly the invariant of its row. Catches an invariant added without a negative
/// case, two invariants reported under one name, and a negative case that passes by
/// tripping another invariant.
#[test]
fn every_invariant_has_a_negative_case_in_the_table() {
    let names: BTreeSet<&str> = Invariant::ALL.iter().map(|i| i.name()).collect();
    assert_eq!(
        names.len(),
        Invariant::ALL.len(),
        "two invariants share a name"
    );
    let table = || jobs::table(NEGATIVE_CASES.iter().chain(whole_host::NEGATIVE_CASES));
    for invariant in Invariant::ALL {
        let rows = table().filter(|(row, _)| *row == invariant);
        assert!(
            rows.count() > 0,
            "{} has no negative case",
            invariant.name()
        );
    }
    for (invariant, case) in table() {
        let found = verdict(*invariant, &case());
        assert_eq!(
            flagged(&found),
            [*invariant],
            "{}: {:?}",
            invariant.name(),
            lines(&found)
        );
    }
}

/// Asserts each invariant's name and when it is checked (the job module's own test the five
/// job invariants'), and each way an item ends' name, as the failure lines spell them. Catches a renamed invariant (a scenario's
/// recorded failure would stop matching) and two answers spelled alike.
#[test]
fn names_are_stable_and_distinct() {
    let invariants: Vec<(&str, Checked)> = Invariant::ALL
        .iter()
        .map(|i| (i.name(), i.checked()))
        .collect();
    assert_eq!(
        invariants[..11],
        [
            ("no-double-booking", Checked::EveryStep),
            ("at-most-one-launch", Checked::EveryStep),
            ("one-terminal-answer", Checked::EveryStep),
            ("settles-when-quiet", Checked::AtEnd),
            ("bookings-match-agents", Checked::AtEnd),
            ("no-lease-on-a-lost-host", Checked::EveryStep),
            ("no-result-lost", Checked::AtEnd),
            ("whole-host-is-exclusive", Checked::EveryStep),
            ("held-means-ready", Checked::EveryStep),
            ("drain-kills-nothing", Checked::EveryStep),
            ("marked-hosts-serve-no-ordinary-start", Checked::EveryStep),
        ]
    );
    let ends: Vec<&str> = HowEnded::ALL.iter().map(|h| h.name()).collect();
    assert_eq!(
        ends,
        [
            "succeeded",
            "failed",
            "cancelled",
            "lost",
            "forgotten",
            "gave-up"
        ]
    );
}
