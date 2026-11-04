//! The two calls a scenario runner needs between steps: a fault set that changes during a
//! run ([`Sim::set_faults`]) and the time of the next event ([`Sim::next_event_time`]).

use std::time::Duration;

use js_sim::{Chance, Event, Faults, Node, NodeId, NodeInput, Output, Sim};
use js_types::sched::{Effect, FarmTime, StateMachine};

/// Sends scripted messages at scripted times and records what it receives.
#[derive(Default)]
struct Scripted {
    /// (send time in ms, receiver, message), each sent from a timer set at start.
    script: Vec<(u64, NodeId, u32)>,
    out: Vec<Output<u32>>,
    /// (message, arrival time in ms)
    got: Vec<(u32, u64)>,
}

impl Scripted {
    fn sending(script: impl IntoIterator<Item = (u64, &'static str, u32)>) -> Self {
        Self {
            script: script
                .into_iter()
                .map(|(at, to, msg)| (at, NodeId::from(to), msg))
                .collect(),
            ..Self::default()
        }
    }
}

impl StateMachine for Scripted {
    type Input = NodeInput<u32>;

    fn apply(&mut self, input: NodeInput<u32>) -> Vec<Effect> {
        match input.event {
            Event::Start => {
                for (tag, (at, _, _)) in (0..).zip(&self.script) {
                    self.out.push(Output::Timer {
                        after: Duration::from_millis(*at),
                        tag,
                    });
                }
            }
            Event::Timer { tag } => {
                let (_, to, msg) = self.script[usize::try_from(tag).unwrap()].clone();
                self.out.push(Output::Send { to, msg });
            }
            Event::Message { msg, .. } => self.got.push((msg, input.now.as_millis())),
        }
        Vec::new()
    }
}

impl Node for Scripted {
    type Msg = u32;

    fn trace_msg(msg: &u32) -> String {
        format!("msg({msg})")
    }

    fn take_outputs(&mut self) -> Vec<Output<u32>> {
        std::mem::take(&mut self.out)
    }
}

/// Every message takes exactly `millis`; nothing is lost, duplicated or reordered.
fn fixed_delay(millis: u64) -> Faults {
    Faults {
        min_delay: Duration::from_millis(millis),
        max_delay: Duration::from_millis(millis),
        ..Faults::default()
    }
}

/// "a" sends 1 to "b" at 0 ms and 2 at 10 ms, every message taking 20 ms. At 1 ms, with
/// message 1 in flight, `change` may change the faults.
fn two_sends(change: impl FnOnce(&mut Sim<Scripted>)) -> Sim<Scripted> {
    let mut sim = Sim::new(7, fixed_delay(20)).with_trace_lines();
    sim.add_node("a", Scripted::sending([(0, "b", 1), (10, "b", 2)]));
    sim.add_node("b", Scripted::default());
    sim.run_until(FarmTime::from_millis(1));
    change(&mut sim);
    sim.run_until(FarmTime::from_millis(1_000));
    sim
}

fn received(sim: &Sim<Scripted>) -> Vec<(u32, u64)> {
    sim.node(&NodeId::from("b"))
        .expect("b was added")
        .got
        .clone()
}

/// Asserts that a message sent before `set_faults(drop always)` is still delivered, at
/// the time it was given when sent (20 ms, though the new faults say 50 ms), and that the
/// one sent after the call is lost; and that the call records nothing in the trace (a
/// run that sets the faults it already has hashes as one that never calls it). Catches a
/// `set_faults` that is a no-op (message 2 would arrive), one that re-times or drops the
/// messages already in flight, and one that writes a trace line of its own.
#[test]
fn a_changed_fault_set_applies_to_later_sends_only() {
    let untouched = two_sends(|_| {});
    assert_eq!(received(&untouched), vec![(1, 20), (2, 30)]);

    let lossy = two_sends(|sim| {
        sim.set_faults(Faults {
            drop: Chance::always(),
            ..fixed_delay(50)
        });
    });
    assert_eq!(received(&lossy), vec![(1, 20)]);
    let lines = lossy.trace_lines().expect("lines kept");
    assert!(
        lines.iter().any(|line| line == "10 lost #1 a->b dropped"),
        "{lines:?}"
    );

    let same = two_sends(|sim| sim.set_faults(fixed_delay(20)));
    assert_eq!(same.trace_hash(), untouched.trace_hash());
    assert_eq!(same.trace_lines(), untouched.trace_lines());
}

/// Asserts that a fault set with an empty delay range is refused by `set_faults` as by
/// `Sim::new`, rather than reaching the delay draw.
#[test]
#[should_panic(expected = "exceeds max_delay")]
fn a_changed_fault_set_with_an_empty_delay_range_is_refused() {
    let mut sim: Sim<Scripted> = Sim::new(1, Faults::default());
    sim.set_faults(Faults {
        min_delay: Duration::from_millis(5),
        max_delay: Duration::from_millis(4),
        ..Faults::default()
    });
}

/// Asserts that the peeked time is the time `step` then moves the clock to, at each step
/// of a run whose timers were scheduled out of time order (30 ms first, then 7 ms), and
/// that the queue reads `None` empty, before any node and after the last event. Catches a
/// peek that is a constant, one that reads the last entry or the first scheduled rather
/// than the earliest due, and one that consumes the event it reads.
#[test]
fn the_next_event_time_is_the_time_of_the_next_step() {
    let mut sim: Sim<Scripted> = Sim::new(3, fixed_delay(5));
    assert_eq!(sim.next_event_time(), None);
    sim.add_node("a", Scripted::sending([(30, "b", 1), (7, "b", 2)]));
    sim.add_node("b", Scripted::default());
    let mut seen = Vec::new();
    while let Some(due) = sim.next_event_time() {
        assert_eq!(sim.next_event_time(), Some(due), "a peek changes nothing");
        assert!(sim.step());
        assert_eq!(sim.now(), due);
        seen.push(due.as_millis());
    }
    // start a, start b, timer 7, timer 30, delivery of 2 at 12, delivery of 1 at 35.
    assert_eq!(seen, vec![0, 0, 7, 12, 30, 35]);
    assert_eq!(sim.next_event_time(), None);
    assert!(!sim.step());
}
