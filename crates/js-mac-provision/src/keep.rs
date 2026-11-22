//! The agent's keeper: the pf anchor of the pool loaded at the agent's start and again at
//! every tick (30 seconds) until the keeper is dropped.
//!
//! [`start`] applies once, and when that is refused (an apply before the system's own pf
//! start at boot is `NotReferenced`) waits one tick and applies once more; that result is
//! the start's. It then spawns a thread that applies at every tick, so a reload of the main
//! ruleset that dropped the anchor is repaired within one interval. A tick whose apply
//! fails is not an error of the keeper: the caller's `apply` closure logs it and the next
//! tick tries again. The keeper stops when it is dropped: the sender of its stop channel is
//! dropped, which ends the wait of [`Every`] at once, and the thread is joined.
//!
//! The clock is a trait ([`Ticks`]) so that a test gives its own ticks and none asserts a
//! time.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::provision::{Config, ProvisionError, Provisioner, enforced};

/// The time between two applies.
pub const INTERVAL: Duration = Duration::from_secs(30);

/// What [`apply`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    /// The policy was enforced already; nothing was loaded.
    InForce,
    /// The policy was not enforced and `install` loaded it.
    Installed,
}

/// Loads the policy of `config` unless it is enforced already: the check, and `install`
/// only when the check fails. It is what `js-mac-provision install` does, and what the
/// agent's keeper does at its start and every 30 seconds.
///
/// # Errors
/// What `install` returns when the check failed and the load did not work.
pub fn apply(config: &Config<'_>) -> Result<Applied, ProvisionError> {
    if enforced(config).is_ok() {
        return Ok(Applied::InForce);
    }
    Provisioner::new(config.pf, config.anchor, config.dir)
        .install(config.uids, config.endpoints)?;
    Ok(Applied::Installed)
}

/// What a keeper does at each apply.
pub type Apply = Box<dyn FnMut() -> Result<Applied, ProvisionError> + Send>;

/// The clock of a keeper.
pub trait Ticks: Send {
    /// Blocks until the next tick (`true`) or until the keeper is to stop (`false`): `stop` is
    /// the channel whose sender the [`Keeper`] holds and drops when it is dropped.
    fn wait(&mut self, stop: &Receiver<()>) -> bool;
}

/// A tick after each span of the duration, for as long as the keeper lives.
#[derive(Debug, Clone, Copy)]
pub struct Every(pub Duration);

impl Ticks for Every {
    fn wait(&mut self, stop: &Receiver<()>) -> bool {
        matches!(stop.recv_timeout(self.0), Err(RecvTimeoutError::Timeout))
    }
}

/// The thread that applies at every tick; dropping it stops and joins the thread.
#[derive(Debug)]
pub struct Keeper {
    stop: Option<Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Keeper {
    fn drop(&mut self) {
        drop(self.stop.take());
        let _ = self.thread.take().map(JoinHandle::join);
    }
}

/// Applies once; when that is refused, waits one tick and applies once more. Returns that
/// result and the [`Keeper`], whose thread then applies at every tick. The caller logs
/// inside `apply`.
pub fn start(
    mut apply: Apply,
    mut ticks: Box<dyn Ticks>,
) -> (Result<Applied, ProvisionError>, Keeper) {
    let (stop, stopped) = mpsc::channel::<()>();
    let mut first = apply();
    if first.is_err() && ticks.wait(&stopped) {
        first = apply();
    }
    let thread = thread::spawn(move || {
        while ticks.wait(&stopped) {
            let _ = apply();
        }
    });
    (
        first,
        Keeper {
            stop: Some(stop),
            thread: Some(thread),
        },
    )
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use js_testkit::dir::TestDir;

    use super::*;
    use crate::policy::{AnchorName, Endpoint, UidRange};
    use crate::testing::Fake;

    /// Ticks from a script: `true` is a tick, `false` or an empty script is a stop.
    struct Script(VecDeque<bool>);

    impl Ticks for Script {
        fn wait(&mut self, _stop: &Receiver<()>) -> bool {
            self.0.pop_front().unwrap_or(false)
        }
    }

    fn script(ticks: &[bool]) -> Box<dyn Ticks> {
        Box::new(Script(ticks.iter().copied().collect()))
    }

    fn refused() -> ProvisionError {
        ProvisionError::PfDisabled
    }

    /// An apply that answers from `answers` (an empty script answers `InForce`) and counts.
    fn answering(answers: Vec<Result<Applied, ProvisionError>>) -> (Apply, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&calls);
        let answers = Mutex::new(VecDeque::from(answers));
        let apply = move || {
            seen.fetch_add(1, Ordering::SeqCst);
            let next = answers.lock().map(|mut a| a.pop_front());
            next.ok().flatten().unwrap_or(Ok(Applied::InForce))
        };
        (Box::new(apply), calls)
    }

    /// Asserts that a first apply that is refused is applied once more on the next tick and
    /// the start returns the second result. It catches a start that gives up on the first
    /// refusal of the boot.
    #[test]
    fn a_first_refusal_is_retried_on_the_next_tick() {
        let (apply, calls) = answering(vec![Err(refused()), Ok(Applied::Installed)]);
        let (first, keeper) = start(apply, script(&[true]));
        assert_eq!(first.unwrap(), Applied::Installed);
        drop(keeper);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    /// Asserts that a refusal with no next tick (the keeper stops) is returned after one
    /// apply. It catches a retry that does not wait for the tick.
    #[test]
    fn a_refusal_with_no_next_tick_is_returned_after_one_apply() {
        let (apply, calls) = answering(vec![Err(refused())]);
        let (first, keeper) = start(apply, script(&[false]));
        assert!(matches!(first, Err(ProvisionError::PfDisabled)));
        drop(keeper);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// Asserts that a start that succeeds first does not wait for a tick: with a script of
    /// no ticks it applies once. It catches a start that always waits.
    #[test]
    fn a_start_that_succeeds_does_not_wait() {
        let (apply, calls) = answering(vec![Ok(Applied::InForce)]);
        let (first, keeper) = start(apply, script(&[]));
        assert_eq!(first.unwrap(), Applied::InForce);
        drop(keeper);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// Asserts that the thread applies once per tick until the ticks stop, and that a failed
    /// tick is followed by another apply. It catches a keeper that stops at its first
    /// failure or applies more or less often than the clock says.
    #[test]
    fn the_thread_applies_once_per_tick_and_goes_on_after_a_failed_tick() {
        let (apply, calls) = answering(vec![
            Ok(Applied::InForce),
            Err(refused()),
            Ok(Applied::Installed),
            Ok(Applied::InForce),
        ]);
        let (first, keeper) = start(apply, script(&[true, true, true]));
        assert_eq!(first.unwrap(), Applied::InForce);
        drop(keeper);
        assert_eq!(calls.load(Ordering::SeqCst), 4);
    }

    /// Asserts that dropping the keeper stops [`Every`] (its stop sender is dropped) and
    /// joins the thread, so no apply runs after the drop. It catches a keeper that is left
    /// running after its owner is gone.
    #[test]
    fn dropping_the_keeper_stops_every_and_joins() {
        let (apply, calls) = answering(vec![Ok(Applied::InForce)]);
        let (_, keeper) = start(apply, Box::new(Every(Duration::from_secs(3600))));
        drop(keeper);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// Asserts that `Every` ticks while the sender lives and stops once it is dropped. It
    /// catches a clock that outlives its keeper.
    #[test]
    fn every_ticks_while_the_sender_lives_and_stops_when_it_is_dropped() {
        let (stop, stopped) = mpsc::channel::<()>();
        let mut every = Every(Duration::ZERO);
        assert!(every.wait(&stopped));
        drop(stop);
        assert!(!every.wait(&stopped));
    }

    /// Asserts that the interval is 30 seconds. It catches a changed cadence that the docs
    /// do not say.
    #[test]
    fn the_interval_is_thirty_seconds() {
        assert_eq!(INTERVAL, Duration::from_secs(30));
    }

    fn applying(pf: &Fake, dir: &TestDir) -> Result<Applied, ProvisionError> {
        let anchor: AnchorName = "com.apple/com.komira.js.pool".parse().unwrap();
        let endpoints: Vec<Endpoint> = vec!["192.0.2.10:443".parse().unwrap()];
        super::apply(&Config {
            pf,
            anchor: &anchor,
            dir: dir.path(),
            uids: UidRange::new(600, 100).unwrap(),
            endpoints: &endpoints,
        })
    }

    /// Asserts that `apply` over a pf with nothing loaded installs, and that a second apply
    /// finds it in force and runs no `-f` load. It catches an apply that reloads every tick.
    #[test]
    fn apply_installs_when_not_in_force_and_reloads_nothing_when_it_is() {
        let dir = TestDir::under(std::env::temp_dir(), "js-mac-provision-keep-apply");
        let pf = Fake::new(dir.path());
        assert_eq!(applying(&pf, &dir).unwrap(), Applied::Installed);
        let before = pf.calls().len();
        assert_eq!(applying(&pf, &dir).unwrap(), Applied::InForce);
        let later = &pf.calls()[before..];
        assert!(!later.iter().any(|c| c.contains(" -f ")), "{later:?}");
    }

    /// Asserts that `apply` returns the install's error when the load fails. It catches an
    /// apply that swallows a failed load.
    #[test]
    fn apply_returns_the_error_of_a_failed_install() {
        let dir = TestDir::under(std::env::temp_dir(), "js-mac-provision-keep-fail");
        let mut pf = Fake::new(dir.path());
        pf.fail = Some((1, false));
        assert!(matches!(
            applying(&pf, &dir),
            Err(ProvisionError::Failed { .. })
        ));
    }
}
