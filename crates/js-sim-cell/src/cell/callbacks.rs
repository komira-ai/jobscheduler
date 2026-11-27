//! The cell's callback receiver and the second instance that claims callbacks: what a
//! scenario does to them, and the cell's read of the calls the receiver got.

use std::time::Duration;

use super::Cell;
use crate::server::{SECOND_CLAIMER, ServerNode};

impl Cell {
    /// Switches the callback receiver down now for `lasting`: it answers a failed try to
    /// every call made before `lasting` has passed, so the server keeps each callback due
    /// and tries it again after its wait. A cell with no server node has no receiver a call
    /// reaches, and the switch then reaches none.
    pub fn set_callback_receiver_down(&mut self, lasting: Duration) {
        let now = self.sim.now();
        let server = self.server().map(ServerNode::receiver);
        server.cloned().unwrap_or_default().down_for(now, lasting);
        self.record(format!("callback receiver down {}ms", lasting.as_millis()));
    }

    /// Has a second instance of the server (`sim-server-2`) claim every callback that
    /// is due now and never answer, as an instance that dies holding its claims: each claim
    /// lapses after 30 s, and the cell's server takes the callback again. How many it
    /// claimed.
    pub fn claim_callbacks_and_vanish(&mut self) -> usize {
        let server = self.server();
        let claimed = server.map_or(0, |s| s.claim_callbacks_and_vanish(SECOND_CLAIMER));
        self.record(format!("callbacks claimed by {SECOND_CLAIMER}: {claimed}"));
        claimed
    }

    /// Reads the calls the receiver got since the cell last read them into the
    /// observations, writing each into the trace.
    pub(super) fn read_callbacks(&mut self) {
        let seen = self.observations.callback_calls.len();
        let server = self.server().map(ServerNode::receiver);
        let calls = server.map(|receiver| receiver.calls_after(seen));
        for call in calls.unwrap_or_default() {
            let (job, answer, ended) = (&call.job, call.answer, call.ended);
            self.record(format!("callback {job} {answer:?} ended={ended}"));
            self.observations.callback_calls.push(call);
        }
    }
}
