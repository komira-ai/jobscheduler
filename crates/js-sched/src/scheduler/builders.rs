//! The builders that set the load, memory and curve settings of a scheduler.

use std::sync::Arc;

use js_types::sched::usage::CurveSource;

use super::Scheduler;
use crate::admission::LoadConfig;
use crate::headroom::MemoryPercent;

impl Scheduler {
    /// This scheduler, admitting actions as `config` says. A scheduler built with
    /// [`Scheduler::new`] places by load with the defaults of [`LoadConfig`]: `On`, 5 s
    /// windows, `f` 1.0, `s_cpu` 0.10, `s_mem` 0, the flat-memory guard at `d` 0 and charges
    /// ending at the expected life plus 2 minutes. `PlacementLoad::Off` here is the
    /// rollback switch to the booked rule.
    #[must_use]
    pub fn with_load_config(mut self, config: LoadConfig) -> Self {
        self.admission.configure(config);
        self
    }

    /// This scheduler, taking `percent` of the memory a worker reports as that worker's
    /// capacity: `floor(memory * percent / 100)` bytes, once, when the worker registers or
    /// its report changes. Every admission path reads the capacity so cut (the load
    /// predicate, the flat-memory guard, the memory ladder's cap, the whole-host test, VM
    /// booking, the unservable check) and [`Scheduler::capacity`] shows it. Cores, GPUs,
    /// VM slots and the lease cap are not cut, and neither is any figure an agent
    /// measures or a job's learned memory. Set it before the first worker registers.
    #[must_use]
    pub const fn with_memory_percent(mut self, percent: MemoryPercent) -> Self {
        self.memory_percent = percent;
        self
    }

    /// The share of a worker's reported memory this scheduler takes as its capacity.
    #[must_use]
    pub const fn memory_percent(&self) -> MemoryPercent {
        self.memory_percent
    }

    /// This scheduler, holding `curves` for the rule that charges a lease by a curve over
    /// its lifetime. The flat rule does not read them: a key that has a curve is charged
    /// flat and counted ([`crate::LoadCounters::curve_flat`]).
    #[must_use]
    pub fn with_curves(mut self, curves: Arc<dyn CurveSource + Send + Sync>) -> Self {
        self.admission.use_curves(curves);
        self
    }
}
