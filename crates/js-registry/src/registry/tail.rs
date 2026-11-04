//! Where the tail counters go beyond the registry's memory: the sink it is given, the
//! counts it loads from it at the start, and the hand-over of each change once the
//! registry's lock is released.

use std::sync::{Arc, MutexGuard, PoisonError};

use js_usage::{TailSink, TailSinkError};

use super::{Registry, State};

impl Registry {
    /// Gives the registry the sink its tail counters are kept in ([`crate::TailCounters`]).
    /// The registry reads what the sink holds (outside its lock) and counts on from it:
    /// [`Registry::tail_counters`] is then the stored counts plus every count since. From
    /// then on every start and stop hands its counts to the sink once the registry's lock
    /// is released, and the counts made before the sink was given go with the first.
    /// Only the first call takes effect, as with [`Registry::set_lives`]; a later one is
    /// ignored. With no sink the counters are in memory only.
    ///
    /// # Errors
    /// The sink's [`TailSinkError`] when what it holds cannot be read or was bucketed by
    /// other edges. The sink is still taken, the counters count on from zero, and the
    /// caller says so once.
    pub fn set_tail_sink(&self, sink: Arc<dyn TailSink>) -> Result<(), TailSinkError> {
        let stored = sink.load();
        let base = stored.as_ref().copied().unwrap_or_default();
        let taken = self.state().tail.set_sink(sink, base);
        let ignored = Ok(());
        match taken {
            true => stored.map(drop),
            false => ignored,
        }
    }

    /// Hands the tail counts that wait to the sink, if there is one. Called once the
    /// registry's lock is released, never under it, so the sink may take its own locks.
    pub(crate) fn send_tail(&self) {
        let unsent = self.state().tail.take_unsent();
        let due = unsent.filter(|(_, delta)| !delta.is_empty());
        due.into_iter().for_each(|(sink, delta)| sink.add(&delta));
    }

    /// The state, with no sweep: the tail book is read and changed alone.
    fn state(&self) -> MutexGuard<'_, State> {
        self.shared
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::{Mutex, OnceLock};
    use std::time::Duration;

    use js_lease::{ManualClock, Timestamp};
    use js_provider::{Pool, StartSpec};
    use js_types::{
        Admission, Capabilities, ClassId, LeaseId, ProviderId, Request, RequestId, ResourceClass,
        Resources, Shape,
    };
    use js_usage::{TailDelta, TailTotals};

    use super::*;
    use crate::PoolConfig;

    fn class() -> ResourceClass {
        ResourceClass::new(ClassId::new("c").unwrap(), Admission::BookAll, 0)
    }

    fn registry() -> Registry {
        let resources = Resources::new(4000, 8 << 30, 0, None).unwrap();
        let shape = Shape::new(resources, BTreeMap::new());
        let config = PoolConfig::new(ProviderId::new("p").unwrap(), class(), shape, 4);
        Registry::new(config, ManualClock::new(Timestamp::from_millis(1_000_000)))
    }

    /// A sink that records, at each add, the delta and whether the registry's lock was
    /// free.
    #[derive(Default)]
    struct Probe {
        registry: OnceLock<Registry>,
        calls: Mutex<Vec<(bool, TailDelta)>>,
    }

    impl TailSink for Probe {
        fn load(&self) -> Result<TailTotals, TailSinkError> {
            Ok(TailTotals::default())
        }

        fn add(&self, delta: &TailDelta) {
            let registry = self.registry.get().unwrap();
            let free = registry.shared.state.try_lock().is_ok();
            self.calls.lock().unwrap().push((free, *delta));
        }
    }

    /// Counts one backfilled start straight into the registry's book.
    fn count_one(registry: &Registry) {
        let id = RequestId::new("q").unwrap();
        registry.state().tail.started(Some(&id), None);
    }

    fn one_backfill() -> TailDelta {
        TailDelta {
            started_on_marked: 1,
            backfilled: 1,
            ..TailDelta::default()
        }
    }

    /// Asserts that the provider's start and stop hand the registry's counts to the sink
    /// with the registry's lock released, each count once, and that a call with nothing
    /// counted does not call the sink. Catches: the sink called under the registry's lock
    /// (a store write that would hold up every heartbeat), a count handed twice, and a
    /// start or stop that never hands its counts on.
    #[test]
    fn the_registry_never_calls_the_sink_under_its_lock() {
        let registry = registry();
        let probe = Arc::new(Probe::default());
        let _ = probe.registry.set(registry.clone());
        registry.set_tail_sink(probe.clone()).unwrap();
        let mut provider = registry.provider();
        count_one(&registry);
        provider.stop(&LeaseId::new(1, 1));
        count_one(&registry);
        let request = Request {
            class: ClassId::new("c").unwrap(),
            resources: Resources::new(1000, 1, 0, None).unwrap(),
            capabilities: Capabilities::new(),
            max_wall: Duration::from_secs(60),
            labels: BTreeMap::new(),
            validation_run: None,
            probe_host: None,
        };
        let spec = StartSpec::new(LeaseId::new(1, 2), request, &[class()]).unwrap();
        provider.start(spec);
        provider.stop(&LeaseId::new(1, 3));
        let calls = probe.calls.lock().unwrap().clone();
        assert_eq!(calls, vec![(true, one_backfill()), (true, one_backfill())]);
        assert_eq!(registry.tail_counters().backfilled, 2);
    }

    /// A sink that loads a fixed answer and keeps what it is given.
    struct Fixed(Result<TailTotals, TailSinkError>, Mutex<TailDelta>);

    impl TailSink for Fixed {
        fn load(&self) -> Result<TailTotals, TailSinkError> {
            self.0.clone()
        }

        fn add(&self, delta: &TailDelta) {
            self.1.lock().unwrap().merge(delta);
        }
    }

    fn stored(backfilled: u64) -> TailTotals {
        TailTotals {
            started_on_marked: backfilled,
            backfilled,
            ..TailTotals::default()
        }
    }

    /// Asserts that counts made before a sink is set are kept and go to the sink with the
    /// first hand-over, on top of what it held; that a second sink is ignored; and that a
    /// sink that cannot be read is still taken, with the counters from zero. Catches: a
    /// sink whose stored counts replace the ones already made (or are lost), a second sink
    /// that takes over, and a load failure that drops the sink.
    #[test]
    fn a_sink_counts_on_from_what_it_holds_and_only_the_first_is_taken() {
        let registry = registry();
        count_one(&registry);
        registry.send_tail();
        let first = Arc::new(Fixed(Ok(stored(5)), Mutex::default()));
        registry.set_tail_sink(first.clone()).unwrap();
        assert_eq!(registry.tail_counters().backfilled, 6);
        let second = Arc::new(Fixed(Ok(stored(100)), Mutex::default()));
        registry.set_tail_sink(second.clone()).unwrap();
        assert_eq!(registry.tail_counters().backfilled, 6);
        registry.send_tail();
        assert_eq!(*first.1.lock().unwrap(), one_backfill());
        assert_eq!(*second.1.lock().unwrap(), TailDelta::default());

        let unread = registry();
        let down = TailSinkError::Conflict { tries: 1 };
        let sink = Arc::new(Fixed(Err(down.clone()), Mutex::default()));
        assert_eq!(unread.set_tail_sink(sink.clone()), Err(down));
        count_one(&unread);
        unread.send_tail();
        assert_eq!(unread.tail_counters().backfilled, 1);
        assert_eq!(*sink.1.lock().unwrap(), one_backfill());
    }
}
