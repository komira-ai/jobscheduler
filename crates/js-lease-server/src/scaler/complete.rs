//! When the shell vouches for the registry's view of its hosts.
//!
//! The registry is not persisted: after a restart it holds the hosts of the lease file
//! (seated, waiting for a hello) and the hosts that have said hello since. A machine the
//! scaler launched, or a host that holds no lease, is absent until its agent says hello
//! again, and the step must not read that absence as "gone". The view is complete when
//! every host that can be known has had its chance to announce itself:
//!
//! ```text
//! complete = uptime >= REGISTRY_FLOOR
//!            && (every host of the roster has been heard since start || uptime >= REGISTRY_QUIET)
//! ```
//!
//! The roster is the scaler's records in `Booting`, `Registered` or `Draining`, and the
//! hosts of the lease file at start, refused seats included (a seat the registry refuses
//! is never heard, so only `REGISTRY_QUIET` releases it). A host is heard when a tick saw
//! it `Live`. Once the view is complete it stays complete until the process ends: a
//! machine launched later is not the restart's to wait for.
//!
//! Uptime is counted from [`Scaler::new`](super::Scaler::new) and counted again from the
//! moment the listeners are bound ([`Scaler::listening`](super::Scaler::listening)), so
//! the time the server spends in startup (the seat of the lease file, TLS, the bind) is
//! not part of it. It is the sum of the forward steps of the injected clock between two
//! observations, never the difference of two readings: a clock that steps back adds
//! nothing and does not stretch the uptime of a process that has already counted it.
//!
//! A forward leap of the wall clock (an NTP or RTC correction at boot, which is when a
//! server restarts) must not complete the view at once. In production the server gives
//! [`Scaler::listening`](super::Scaler::listening) a cap of twice its tick interval, and
//! no observation adds more than that. A tick that stalls longer than the cap counts as
//! the cap, which only delays the verdict. Tests that jump the clock on purpose pass no
//! cap.

use std::collections::BTreeSet;
use std::time::Duration;

use js_agent_wire::{
    DEFAULT_GRACE_MS, DEFAULT_HELLO_BURST, DEFAULT_HELLO_INTERVAL, RETRY_AFTER_CAP,
};
use js_scaler::{Millis, Phase, Records};

use super::Core;

/// What the agent's connect and TLS handshake may take before its hello arrives.
const CONNECT_MS: u64 = 25_000;

/// The size of the fleet one credential can restart together that the floor covers.
const HERD: u64 = 200;

/// The time the hello limiter needs to admit `herd` agents of one credential that all
/// say hello at once: the burst goes in, the rest one per interval.
const fn hello_drain_ms(herd: u64, burst: u32, interval: Duration) -> u64 {
    herd.saturating_sub(burst as u64) * interval.as_millis() as u64
}

/// How long after the listeners are bound the view cannot be complete, whoever has been heard.
///
/// A host that was told to wait before its next hello waits at most
/// `RETRY_AFTER_CAP` (120 s, the agent's own constant); its heartbeat grace is `DEFAULT_GRACE_MS` (15 s); its
/// connection and TLS handshake may take 25 s; and a fleet of 200 agents on one
/// credential restarting together is admitted at the hello limiter's default 60 at once
/// and then one a second, so the last needs 140 s more. The sum is 300 s, 5 minutes.
pub const REGISTRY_FLOOR: Duration = Duration::from_millis(
    RETRY_AFTER_CAP.as_millis() as u64
        + DEFAULT_GRACE_MS
        + CONNECT_MS
        + hello_drain_ms(HERD, DEFAULT_HELLO_BURST, DEFAULT_HELLO_INTERVAL),
);

/// How long after the listeners are bound a host of the roster that has not been heard stops
/// holding the view open: a dead host cannot keep the scaler blind for ever. Below the
/// default boot timeout (15 minutes) so that a machine that never came up is still
/// released by the step's own count after the view completes.
pub const REGISTRY_QUIET: Duration = Duration::from_secs(10 * 60);

/// What the shell remembers to answer "is the view complete".
#[derive(Debug)]
pub(super) struct Gate {
    /// The clock at the last observation.
    last: Millis,
    uptime: Duration,
    /// The most one observation adds to the uptime, or `None` for no limit.
    cap: Option<Duration>,
    /// The hosts of the lease file at start.
    leased: BTreeSet<String>,
    heard: BTreeSet<String>,
    done: bool,
}

impl Gate {
    /// A gate whose uptime starts at `now`, with no host of the lease file.
    pub(super) fn starting(now: Millis) -> Self {
        Self {
            last: now,
            uptime: Duration::ZERO,
            cap: None,
            leased: BTreeSet::new(),
            heard: BTreeSet::new(),
            done: false,
        }
    }

    /// The listeners are bound at `now`: the uptime starts again from zero, no observation
    /// adds more than `cap` to it from now on, and the hosts of the lease file join the
    /// roster.
    pub(super) fn bound<I: IntoIterator<Item = String>>(
        &mut self,
        (now, cap): (Millis, Option<Duration>),
        leased: I,
    ) {
        self.last = now;
        self.uptime = Duration::ZERO;
        self.cap = cap;
        self.leased.extend(leased);
    }

    /// One tick at `now`: the verdict for the step. `live` are the hosts the registry
    /// shows `Live` now.
    pub(super) fn observe<'a, I: IntoIterator<Item = &'a str>>(
        &mut self,
        now: Millis,
        records: &Records,
        live: I,
    ) -> bool {
        let forward = now.since(self.last);
        self.uptime += self.cap.map_or(forward, |cap| forward.min(cap));
        self.last = now;
        self.heard.extend(live.into_iter().map(str::to_owned));
        let ours = records.instances.iter().filter(|record| {
            [Phase::Booting, Phase::Registered, Phase::Draining].contains(&record.phase)
        });
        let names = ours.map(|record| record.name.as_str());
        let mut roster = names.chain(self.leased.iter().map(String::as_str));
        let all_heard = roster.all(|name| self.heard.contains(name));
        self.done |= self.uptime >= REGISTRY_FLOOR && (all_heard || self.uptime >= REGISTRY_QUIET);
        self.done
    }
}

impl Core {
    /// The verdict of this tick for the step's `registry_complete`.
    pub(super) fn registry_complete(
        &self,
        records: &Records,
        views: &[js_registry::HostView],
        now: Millis,
    ) -> bool {
        let live = views
            .iter()
            .filter(|view| view.state == js_registry::HostState::Live)
            .map(|view| view.id.as_str());
        let mut gate = self
            .gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        gate.observe(now, records, live)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapt::SharedClock;
    use crate::rate::{HelloLimit, HelloLimiter};
    use js_lease::{Clock, ManualClock, Timestamp};
    use js_registry::Credential;

    /// The floor covers each wait before a hello it is made of, for the limits the flags
    /// default to; a change to one of them breaks this and not the settle. Checked at
    /// compile time.
    const _: () = {
        let drain = (HERD - DEFAULT_HELLO_BURST as u64) * DEFAULT_HELLO_INTERVAL.as_millis() as u64;
        assert!(
            REGISTRY_FLOOR.as_millis() as u64
                >= RETRY_AFTER_CAP.as_millis() as u64 + DEFAULT_GRACE_MS + CONNECT_MS + drain
        );
        assert!(REGISTRY_FLOOR.as_millis() < REGISTRY_QUIET.as_millis());
    };

    /// Asserts (e): the constants the floor is built from are the hello limiter's own
    /// defaults, and the floor is the 5 minutes its doc states.
    #[test]
    fn the_floor_is_built_from_the_limiters_defaults() {
        let limit = HelloLimit::default();
        assert_eq!(hello_drain_ms(HERD, limit.burst, limit.interval), 140_000);
        assert_eq!(limit.burst, DEFAULT_HELLO_BURST);
        assert_eq!(limit.interval, DEFAULT_HELLO_INTERVAL);
        let cli = crate::config::Cli::try_parse_from([
            "js-lease-server",
            "--state-file=a",
            "--boot=resume",
            "--pool-config=b",
            "--token-file=c",
        ])
        .unwrap();
        assert_eq!(cli.agent_hello_burst, DEFAULT_HELLO_BURST);
        assert_eq!(cli.agent_hello_interval, DEFAULT_HELLO_INTERVAL);
        assert_eq!(REGISTRY_FLOOR, Duration::from_secs(300));
    }

    /// Asserts (f): 200 agents on one credential say hello at once through the real
    /// limiter; each held agent retries as soon as it is allowed. The last is admitted
    /// well after the 2 minutes the view used to settle in, the view stays incomplete
    /// until it is, and completes at the floor.
    #[test]
    fn a_herd_on_one_credential_keeps_the_view_open_until_the_last_hello() {
        let clock = ManualClock::new(Timestamp::from_millis(1_000_000));
        let limiter = HelloLimiter::new(HelloLimit::default(), SharedClock::new(clock.clone()));
        let credential = Credential::of_token("pool-token");
        let names: Vec<String> = (0..HERD).map(|n| format!("host-{n}")).collect();
        let mut gate = Gate::starting(Millis::new(clock.now().millis()));
        gate.bound(
            (Millis::new(clock.now().millis()), None),
            names.iter().cloned(),
        );
        let mut retry_at: Vec<u64> = vec![0; names.len()];
        let mut pending: Vec<usize> = (0..names.len()).collect();
        let mut admitted: Vec<&str> = Vec::new();
        let mut last_hello = None;
        let records = Records::default();
        let mut seconds = 0u64;
        loop {
            let now = clock.now().millis();
            pending.retain(|&n| {
                if retry_at[n] > now {
                    return true;
                }
                match limiter.admit(&credential) {
                    Ok(()) => {
                        admitted.push(&names[n]);
                        last_hello = Some(seconds);
                        false
                    }
                    Err(held) => {
                        retry_at[n] = now + held.wait.as_millis() as u64;
                        true
                    }
                }
            });
            let complete = gate.observe(Millis::new(now), &records, admitted.iter().copied());
            let everyone = admitted.len() == names.len();
            if !everyone {
                assert!(!complete, "complete with hellos outstanding at {seconds} s");
            }
            if complete {
                break;
            }
            assert!(seconds < 3600, "never completed");
            clock.advance(Duration::from_secs(1));
            seconds += 1;
        }
        let last = last_hello.unwrap();
        assert!(
            Duration::from_secs(last) > Duration::from_secs(120),
            "the herd outlasts the old settle"
        );
        assert!(Duration::from_secs(last) <= REGISTRY_FLOOR);
        assert_eq!(Duration::from_secs(seconds), REGISTRY_FLOOR);
    }
}
