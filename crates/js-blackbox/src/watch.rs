//! The wait and the hold the pass conditions are measured with, on an injected clock: the
//! black-box test gives them the system's monotonic clock and a real pause, and the judge's
//! tests a clock and a pause of their own, so no test of this crate waits.

use std::fmt::Display;
use std::time::Duration;

use js_types::Monotonic;

/// What one probe saw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Seen<T> {
    /// What was waited for, with the value it came with.
    Yes(T),
    /// Not yet: what was seen instead, said for the failure message.
    No(String),
}

/// How a wait ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Waited<T> {
    /// A probe saw it, `after` the wait began (at most the limit).
    Met {
        /// When the probe that saw it ended.
        after: Duration,
        /// What it saw.
        value: T,
    },
    /// No probe saw it within the limit.
    Missed {
        /// When the last probe ended.
        after: Duration,
        /// What the last probe saw.
        last: String,
    },
}

impl<T> Waited<T> {
    /// The value and when it was seen, or one line naming `what` was waited for, the limit,
    /// and what was seen last.
    ///
    /// # Errors
    ///
    /// The line, when the wait missed.
    pub fn within(self, what: &str, limit: Duration) -> Result<(Duration, T), String> {
        match self {
            Waited::Met { after, value } => Ok((after, value)),
            Waited::Missed { after, last } => Err(format!(
                "{what}: not within {} s (gave up after {} ms); last seen: {last}",
                limit.as_secs(),
                after.as_millis()
            )),
        }
    }
}

/// How a hold ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Held {
    /// Every probe of the span passed.
    Kept {
        /// How many probes there were.
        probes: u32,
    },
    /// A probe failed, `after` the hold began.
    Broken {
        /// When the probe that failed ended.
        after: Duration,
        /// What it saw.
        seen: String,
    },
}

impl Held {
    /// Nothing, or one line naming `what` should have held, when it broke and what was seen.
    ///
    /// # Errors
    ///
    /// The line, when the hold broke.
    pub fn kept(self, what: &str) -> Result<u32, String> {
        match self {
            Held::Kept { probes } => Ok(probes),
            Held::Broken { after, seen } => Err(format!(
                "{what}: broken {} ms in: {seen}",
                after.as_millis()
            )),
        }
    }
}

/// Probes at a fixed pace on a clock it is given.
pub struct Watch<'a> {
    clock: &'a dyn Monotonic,
    pause: &'a mut dyn FnMut(Duration),
    every: Duration,
}

impl<'a> Watch<'a> {
    /// Reads `clock`, and waits between two probes by calling `pause` with `every`.
    pub fn new(
        clock: &'a dyn Monotonic,
        pause: &'a mut dyn FnMut(Duration),
        every: Duration,
    ) -> Self {
        Self {
            clock,
            pause,
            every,
        }
    }

    /// Probes until a probe sees what is waited for, or until a probe ends at or after `limit`
    /// without it. A probe that sees it after the limit has passed is a miss: the condition is
    /// that it is seen within the limit.
    pub fn until<T>(&mut self, limit: Duration, mut probe: impl FnMut() -> Seen<T>) -> Waited<T> {
        let start = self.clock.reading();
        loop {
            let seen = probe();
            let after = self.clock.reading().saturating_sub(start);
            match seen {
                Seen::Yes(value) if after <= limit => return Waited::Met { after, value },
                Seen::Yes(_) => {
                    return Waited::Missed {
                        after,
                        last: "seen only after the limit".to_owned(),
                    };
                }
                Seen::No(last) if after >= limit => return Waited::Missed { after, last },
                Seen::No(_) => (self.pause)(self.every),
            }
        }
    }

    /// Probes for `span`: the first probe at once, the last one ending at or after `span`.
    /// The hold breaks at the first probe that fails.
    pub fn hold<E: Display>(
        &mut self,
        span: Duration,
        mut probe: impl FnMut() -> Result<(), E>,
    ) -> Held {
        self.holding(span, true, || probe().map(|()| false))
    }

    /// Probes until a probe says it is done (`Ok(true)`), for at most `limit`. The hold breaks
    /// at the first probe that fails, and when `limit` passes with no probe done.
    pub fn hold_until<E: Display>(
        &mut self,
        limit: Duration,
        probe: impl FnMut() -> Result<bool, E>,
    ) -> Held {
        self.holding(limit, false, probe)
    }

    /// The hold of [`Watch::hold`] and [`Watch::hold_until`]: a span that passes with no probe
    /// done is kept when `span_keeps`, and breaks otherwise.
    fn holding<E: Display>(
        &mut self,
        span: Duration,
        span_keeps: bool,
        mut probe: impl FnMut() -> Result<bool, E>,
    ) -> Held {
        let start = self.clock.reading();
        let mut probes = 0;
        loop {
            let outcome = probe();
            probes += 1;
            let after = self.clock.reading().saturating_sub(start);
            match outcome {
                Err(seen) => {
                    return Held::Broken {
                        after,
                        seen: seen.to_string(),
                    };
                }
                Ok(true) => return Held::Kept { probes },
                Ok(false) if after < span => (self.pause)(self.every),
                Ok(false) if span_keeps => return Held::Kept { probes },
                Ok(false) => {
                    return Held::Broken {
                        after,
                        seen: format!("not done within {} s", span.as_secs()),
                    };
                }
            }
        }
    }
}
