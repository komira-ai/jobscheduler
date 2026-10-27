//! What decides when the table is ticked.

use std::future::Future;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tokio::time::{Interval, MissedTickBehavior};

use crate::util::or_pending;

/// One tick of a [`Ticker`]. The server calls [`Tick::finish`] when the tick is done.
#[derive(Debug, Default)]
pub struct Tick {
    done: Option<oneshot::Sender<()>>,
}

impl Tick {
    /// Reports the tick done to whoever asked for it, if anyone is waiting.
    pub fn finish(self) {
        self.done.into_iter().for_each(|done| {
            let _ = done.send(());
        });
    }
}

/// The source of ticks the server's loop waits on: a timer in production, a channel a
/// test steps.
pub trait Ticker: Send + 'static {
    /// Completes at the next tick.
    fn next(&mut self) -> impl Future<Output = Tick> + Send;
}

/// Ticks every `period`, the first one at once. A tick that falls behind (a slow table
/// call) delays the next instead of bursting to catch up.
#[derive(Debug)]
pub struct IntervalTicker {
    interval: Interval,
}

impl IntervalTicker {
    /// A ticker with this period, which must be positive; build it inside a runtime.
    pub fn new(period: Duration) -> Self {
        let mut interval = tokio::time::interval(period);
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        Self { interval }
    }
}

impl Ticker for IntervalTicker {
    async fn next(&mut self) -> Tick {
        self.interval.tick().await;
        Tick::default()
    }
}

/// A ticker a caller steps: each [`TickHandle::step`] is one tick. Time does not enter
/// into it, so a test decides exactly when the table is ticked.
#[derive(Debug)]
pub struct ChannelTicker {
    steps: mpsc::Receiver<oneshot::Sender<()>>,
}

/// The sending half of a [`ChannelTicker`].
#[derive(Debug, Clone)]
pub struct TickHandle {
    steps: mpsc::Sender<oneshot::Sender<()>>,
}

/// A ticker and the handle that steps it.
pub fn channel_ticker() -> (TickHandle, ChannelTicker) {
    let (steps, receiver) = mpsc::channel(1);
    (TickHandle { steps }, ChannelTicker { steps: receiver })
}

impl TickHandle {
    /// Asks for one tick and returns when the server has finished it. It returns at
    /// once when the server is gone.
    pub async fn step(&self) {
        let (done, finished) = oneshot::channel();
        let _ = self.steps.send(done).await;
        let _ = finished.await;
    }
}

impl Ticker for ChannelTicker {
    async fn next(&mut self) -> Tick {
        let done = or_pending(self.steps.recv().await).await;
        Tick { done: Some(done) }
    }
}
