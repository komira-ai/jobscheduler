//! The cell's callback receiver: where the completion callbacks of the cell's jobs are
//! sent, in place of an HTTP endpoint.
//!
//! The server node hands each due callback to it ([`Receiver::answer`]) through the
//! in-process seam (`InProcess::deliver_callback`), after each tick, so nothing is sent
//! over a socket and no thread delivers. The receiver answers 2xx ([`CallbackAnswer::Delivered`])
//! unless it was switched down ([`Receiver::down_for`]), when it answers a failed try until the
//! switch ends. It records every call ([`CallbackCall`]), with the state of the job's row
//! at the call, and it belongs to the node, so a restart of the server does not lose it.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use js_jobs::JobState;
use js_types::sched::FarmTime;
use js_types::{CallbackAnswer, FailureClass};

use crate::jobs::CallbackCall;

/// What the receiver holds.
#[derive(Debug, Default)]
struct Held {
    /// Until when it answers a failed try; at or after this it answers 2xx.
    down_until: FarmTime,
    /// Every call it got, oldest first.
    calls: Vec<CallbackCall>,
}

/// The receiver of the cell's callbacks, shared between the server node and the cell.
#[derive(Clone, Debug, Default)]
pub struct Receiver(Rc<RefCell<Held>>);

impl Receiver {
    /// A receiver that answers 2xx and has got no call.
    #[must_use]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Switches the receiver down at `now` for `lasting`: it answers a failed try to each
    /// call made before `now` plus `lasting`. A later switch replaces the end of an earlier
    /// one, shorter or longer.
    pub(crate) fn down_for(&self, now: FarmTime, lasting: Duration) {
        self.0.borrow_mut().down_until = now.saturating_add(lasting);
    }

    /// Takes the call of `job` with `webhook_id` made at `at`, the job's row then in
    /// `state` (`None`: no such row), and answers it: a failed try while the receiver is
    /// down, else 2xx. The call is kept ([`Receiver::calls_after`]).
    pub(crate) fn answer(
        &self,
        job: js_jobs::JobId,
        webhook_id: &str,
        at: FarmTime,
        state: Option<JobState>,
    ) -> CallbackAnswer {
        let mut held = self.0.borrow_mut();
        let answer = if at < held.down_until {
            CallbackAnswer::Failed(FailureClass::Failed)
        } else {
            CallbackAnswer::Delivered
        };
        held.calls.push(CallbackCall {
            job,
            webhook_id: webhook_id.to_owned(),
            at,
            answer,
            ended: state.is_some_and(JobState::is_ended),
            state,
        });
        answer
    }

    /// The calls got after the first `seen`, oldest first.
    #[must_use]
    pub fn calls_after(&self, seen: usize) -> Vec<CallbackCall> {
        self.0.borrow().calls.iter().skip(seen).cloned().collect()
    }
}
