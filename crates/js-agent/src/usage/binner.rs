//! Cuts a lease's CPU counter into minutes counted from the lease's own start.

use std::collections::BTreeMap;

use super::clock::MINUTE_USEC;
use super::sampler::{LeaseSample, Sample};

/// A minute observed for less than this many microseconds is absent.
pub const MIN_SPAN_USEC: u64 = 30_000_000;
/// Pending minutes and idle lease state older than this many microseconds are dropped.
pub const MAX_UNSENT_AGE_USEC: u64 = 600_000_000;
/// The last minute index: minute 63 means "minute 63 or later", folded by the maximum.
pub const TAIL_MINUTE: u16 = 63;

/// One closed minute of one lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClosedMinute {
    /// The lease's id.
    pub lease_id: String,
    /// The minute from the lease's start, at most [`TAIL_MINUTE`].
    pub minute: u16,
    /// Mean CPU over the observed span, in thousandths of a core; never above the peak.
    pub cpu_mean_millis: u32,
    /// The highest CPU rate of any window part in the minute, in thousandths of a core.
    pub cpu_peak_window_millis: u32,
    /// The highest working set seen in the minute, in bytes.
    pub ws_max_bytes: u64,
}

/// How often each kind of data was set aside. Each is a count since the binner was made.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counters {
    /// Windows longer than three window lengths: they contribute nothing.
    pub gaps: u64,
    /// Windows whose CPU counter went back: they contribute nothing.
    pub counter_back: u64,
    /// Samples whose clock went back before the previous one: the lease starts over.
    pub clock_back: u64,
    /// Minutes observed for less than [`MIN_SPAN_USEC`]: they are absent.
    pub thin_minutes: u64,
    /// Pending minutes dropped for being older than [`MAX_UNSENT_AGE_USEC`].
    pub aged_out: u64,
    /// Leases forgotten for not appearing in samples for [`MAX_UNSENT_AGE_USEC`].
    pub idle_forgotten: u64,
    /// Lease samples taken before the lease's start: ignored.
    pub before_start: u64,
}

/// What one sample told about the leases.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    /// The leases that ran the whole window since the previous sample: they had a baseline
    /// and the window was valid.
    pub covered: Vec<String>,
}

/// The share of a window that falls in one minute of a lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowPart {
    /// The minute from the lease's start (not folded).
    pub minute: u64,
    /// How long the part is, in microseconds; never 0.
    pub len_usec: u64,
    /// The CPU given to the part, in microseconds.
    pub cpu_usec: u64,
}

/// Cuts the window `(from, to]` at every minute boundary of a lease that started at
/// `started`, and shares `cpu_usec` over the parts by length.
///
/// Each part but the last gets `floor(cpu_usec * len / (to - from))` and the last gets what
/// is left, so the parts add up to `cpu_usec` exactly and their lengths to `to - from`.
/// An empty window (`to <= from`) has no parts. A `from` before `started` counts as minute 0.
pub fn window_parts(started: u64, from: u64, to: u64, cpu_usec: u64) -> Vec<WindowPart> {
    let total = u128::from(to.saturating_sub(from));
    let mut parts = Vec::new();
    let mut at = from;
    let mut given = 0;
    while at < to {
        let minute = at.saturating_sub(started) / MINUTE_USEC;
        let boundary = started.saturating_add(minute.saturating_add(1).saturating_mul(MINUTE_USEC));
        let end = to.min(boundary);
        let share = if end == to {
            cpu_usec - given
        } else {
            let part = u128::from(cpu_usec) * u128::from(end - at) / total;
            u64::try_from(part).unwrap_or(u64::MAX)
        };
        given += share;
        parts.push(WindowPart {
            minute,
            len_usec: end - at,
            cpu_usec: share,
        });
        at = end;
    }
    parts
}

/// A minute being filled.
#[derive(Debug, Clone, Copy)]
struct Open {
    index: u64,
    cpu_usec: u64,
    span_usec: u64,
    peak_millis: u32,
    ws_max: u64,
}

impl Open {
    fn new(index: u64, ws: u64) -> Self {
        Self {
            index,
            cpu_usec: 0,
            span_usec: 0,
            peak_millis: 0,
            ws_max: ws,
        }
    }
}

/// What the binner knows of one lease.
struct LeaseState {
    started: u64,
    last_mono: u64,
    last_cpu: u64,
    open: Open,
}

/// A closed minute waiting to be taken.
#[derive(Clone, Copy)]
struct Entry {
    closed_at: u64,
    mean: u32,
    peak: u32,
    ws: u64,
}

/// Cuts samples into [`ClosedMinute`]s.
pub struct Binner {
    window_usec: u64,
    latest: u64,
    leases: BTreeMap<String, LeaseState>,
    pending: BTreeMap<(String, u16), Entry>,
    in_flight: BTreeMap<(String, u16), u64>,
    counters: Counters,
}

/// `cpu` microseconds over `span` microseconds, in thousandths of a core, saturating.
fn millis(cpu: u64, span: u64) -> u32 {
    let value = u128::from(cpu) * 1000 / u128::from(span);
    u32::try_from(value).unwrap_or(u32::MAX)
}

impl Binner {
    /// A binner for samples about `window_usec` apart (0 is read as 1). A window longer
    /// than three of these is a gap.
    pub fn new(window_usec: u64) -> Self {
        Self {
            window_usec: window_usec.max(1),
            latest: 0,
            leases: BTreeMap::new(),
            pending: BTreeMap::new(),
            in_flight: BTreeMap::new(),
            counters: Counters::default(),
        }
    }

    /// Takes one sample in. Time is `sample.mono_usec` only.
    pub fn observe(&mut self, sample: &Sample) -> Observation {
        self.latest = self.latest.max(sample.mono_usec);
        let mut covered = Vec::new();
        for lease in &sample.leases {
            if sample.mono_usec < lease.started_usec {
                self.counters.before_start += 1;
            } else if self.step(lease, sample.mono_usec) {
                covered.push(lease.lease_id.clone());
            }
        }
        self.forget_idle(sample.mono_usec);
        Observation { covered }
    }

    /// The lease is over: closes its open minute under the same span rule and forgets it.
    /// Its closed minutes stay until taken. An unknown lease is ignored.
    pub fn lease_ended(&mut self, lease_id: &str) {
        if let Some(state) = self.leases.remove(lease_id) {
            self.close(lease_id, state.open);
        }
    }

    /// Takes at most `max` pending minutes, oldest close first, then lease id, then
    /// minute. Minutes older than [`MAX_UNSENT_AGE_USEC`] at `now_usec` are dropped
    /// (counted) instead. Give back what could not be sent with [`Binner::requeue`]
    /// before the next call.
    pub fn take(&mut self, max: usize, now_usec: u64) -> Vec<ClosedMinute> {
        let mut all: Vec<_> = std::mem::take(&mut self.pending).into_iter().collect();
        let before = all.len();
        all.retain(|(_, entry)| now_usec.saturating_sub(entry.closed_at) <= MAX_UNSENT_AGE_USEC);
        self.counters.aged_out += (before - all.len()) as u64;
        // Stable: ties keep the map's order, lease id then minute.
        all.sort_by_key(|(_, entry)| entry.closed_at);
        let rest = all.split_off((max + 1).min(all.len()));
        self.pending = rest.into_iter().collect();
        self.in_flight = all
            .iter()
            .map(|(key, entry)| (key.clone(), entry.closed_at))
            .collect();
        all.into_iter()
            .map(|((lease_id, minute), entry)| ClosedMinute {
                lease_id,
                minute,
                cpu_mean_millis: entry.mean,
                cpu_peak_window_millis: entry.peak,
                ws_max_bytes: entry.ws,
            })
            .collect()
    }

    /// Puts minutes that were taken but not delivered back, merged by lease and minute
    /// with the maximum of each field, so a minute is never held twice. A minute keeps
    /// the close time it had when it was taken.
    pub fn requeue(&mut self, minutes: Vec<ClosedMinute>) {
        for minute in minutes {
            let key = (minute.lease_id, minute.minute.min(TAIL_MINUTE));
            let closed_at = self.in_flight.remove(&key).unwrap_or(self.latest);
            self.merge(
                key,
                Entry {
                    closed_at,
                    mean: minute.cpu_mean_millis,
                    peak: minute.cpu_peak_window_millis,
                    ws: minute.ws_max_bytes,
                },
            );
        }
    }

    /// How many closed minutes wait to be taken.
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    /// How many leases have state.
    pub fn tracked(&self) -> usize {
        self.leases.len()
    }

    /// The counts of what was set aside.
    pub fn counters(&self) -> Counters {
        self.counters
    }

    /// Applies one lease sample at `now`; whether the lease was covered.
    fn step(&mut self, lease: &LeaseSample, now: u64) -> bool {
        let id = lease.lease_id.as_str();
        let ws = lease.working_set_bytes;
        let fresh = LeaseState {
            started: lease.started_usec,
            last_mono: now,
            last_cpu: lease.cpu_usage_usec,
            open: Open::new((now - lease.started_usec) / MINUTE_USEC, ws),
        };
        let Some(mut state) = self.leases.remove(id) else {
            self.leases.insert(id.to_owned(), fresh);
            return false;
        };
        let same_series = state.started == lease.started_usec;
        if !same_series || now < state.last_mono {
            if same_series {
                self.counters.clock_back += 1;
            }
            self.close(id, state.open);
            self.leases.insert(id.to_owned(), fresh);
            return false;
        }
        let mut covered = false;
        if lease.cpu_usage_usec < state.last_cpu {
            self.counters.counter_back += 1;
            state.rebase(now, lease.cpu_usage_usec);
        } else if now - state.last_mono > self.window_usec.saturating_mul(3) {
            self.counters.gaps += 1;
            state.rebase(now, lease.cpu_usage_usec);
        } else if now > state.last_mono {
            self.split(id, &mut state, (now, lease.cpu_usage_usec), ws);
            covered = true;
        }
        let minute = (now - state.started) / MINUTE_USEC;
        self.roll(id, &mut state, minute);
        state.open.ws_max = state.open.ws_max.max(ws);
        self.leases.insert(id.to_owned(), state);
        covered
    }

    /// Gives each part of the window `(last_mono, now]` to its minute.
    ///
    /// A minute that lies wholly inside the window has no sample of its own, so it takes
    /// the larger of the working sets the window is bracketed by: nothing is invented.
    /// The first part belongs to the minute already open, which keeps its own samples, and
    /// the last is filled by the sample that ends the window; neither takes the bracket.
    fn split(&mut self, id: &str, state: &mut LeaseState, at: (u64, u64), ws: u64) {
        let (now, cpu) = at;
        let delta = cpu - state.last_cpu;
        let bracket = state.open.ws_max.max(ws);
        let parts = window_parts(state.started, state.last_mono, now, delta);
        let last = parts.len().saturating_sub(1);
        for (n, part) in parts.into_iter().enumerate() {
            self.roll(id, state, part.minute);
            if 0 < n && n < last {
                state.open.ws_max = state.open.ws_max.max(bracket);
            }
            state.open.cpu_usec += part.cpu_usec;
            state.open.span_usec += part.len_usec;
            state.open.peak_millis = state
                .open
                .peak_millis
                .max(millis(part.cpu_usec, part.len_usec));
        }
        state.rebase(now, cpu);
    }

    /// Makes `minute` the open one, closing an earlier one.
    fn roll(&mut self, id: &str, state: &mut LeaseState, minute: u64) {
        if state.open.index < minute {
            let old = std::mem::replace(&mut state.open, Open::new(minute, 0));
            self.close(id, old);
        }
    }

    /// Closes a minute: pending when it was observed long enough, counted thin otherwise.
    fn close(&mut self, id: &str, open: Open) {
        if open.span_usec < MIN_SPAN_USEC {
            self.counters.thin_minutes += 1;
            return;
        }
        let bucket = u16::try_from(open.index.min(u64::from(TAIL_MINUTE))).unwrap_or(TAIL_MINUTE);
        let entry = Entry {
            closed_at: self.latest,
            mean: millis(open.cpu_usec, open.span_usec),
            peak: open.peak_millis,
            ws: open.ws_max,
        };
        self.merge((id.to_owned(), bucket), entry);
    }

    /// Adds an entry, merging with the one under the same key by the maximum of each field.
    fn merge(&mut self, key: (String, u16), entry: Entry) {
        match self.pending.get_mut(&key) {
            Some(held) => {
                held.closed_at = held.closed_at.max(entry.closed_at);
                held.mean = held.mean.max(entry.mean);
                held.peak = held.peak.max(entry.peak);
                held.ws = held.ws.max(entry.ws);
            }
            None => {
                self.pending.insert(key, entry);
            }
        }
    }

    /// Forgets the leases that no sample has named for [`MAX_UNSENT_AGE_USEC`].
    fn forget_idle(&mut self, now: u64) {
        let before = self.leases.len();
        self.leases
            .retain(|_, state| now.saturating_sub(state.last_mono) <= MAX_UNSENT_AGE_USEC);
        self.counters.idle_forgotten += (before - self.leases.len()) as u64;
    }
}

impl LeaseState {
    fn rebase(&mut self, mono: u64, cpu: u64) {
        self.last_mono = mono;
        self.last_cpu = cpu;
    }
}
