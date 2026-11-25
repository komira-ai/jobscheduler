//! What a native run reports it used: the samples of its account's processes, taken at the
//! polls while it runs, and what its supervisor's wait of the job said (the fake launcher's
//! [`REAPED`]), as the job's usage and, with the series asked for, its CPU and memory series.

use std::fs;
use std::io;
use std::sync::{Arc, Mutex, PoisonError};

use js_agent::lifecycle::{Exit, Runtime, Status};
use js_agent::mac_usage::{ProcUsage, ProcessUsage, Series};
use js_agent_wire::JobUsage;

use super::fake_launcher::REAPED;
use super::{Failing, Rig, me};

/// Usage that finds one process of every uid asked, `process`, or none, and records the uids
/// asked.
#[derive(Debug, Default)]
pub struct Fixed {
    process: Option<ProcUsage>,
    asked: Mutex<Vec<u32>>,
}

impl Fixed {
    /// Usage that finds no process.
    pub fn none() -> Self {
        Self::default()
    }

    /// Usage that finds `process` for every uid.
    fn one(process: ProcUsage) -> Self {
        Self {
            process: Some(process),
            asked: Mutex::default(),
        }
    }

    /// The uids asked, in order.
    fn asked(&self) -> Vec<u32> {
        self.asked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl ProcessUsage for Fixed {
    fn of_uid(&self, uid: u32) -> io::Result<Vec<ProcUsage>> {
        self.asked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(uid);
        Ok(self.process.into_iter().collect())
    }
}

/// The sampled process: less CPU than the wait's, more memory than its resident set.
const SAMPLED: ProcUsage = ProcUsage {
    cpu_usec: 1_000_000,
    footprint_bytes: 64 << 20,
};

/// Starts a job that runs until the file `go` is made, polls it once while it runs (one
/// sample), makes the file, and polls until it ends.
fn run_one_sampled(rig: &mut Rig) -> js_agent::lifecycle::RunHandle {
    let go = rig.dir.path("go");
    let script = format!("while [ ! -e {} ]; do sleep 0.01; done", go.display());
    let id = rig.start("r0", "/bin/sh", &["-c", &script]);
    assert_eq!(rig.rt.poll(&id).unwrap(), Status::Running);
    fs::write(&go, b"").unwrap();
    assert_eq!(
        rig.settle(&id),
        Status::Exited(Exit {
            code: 0,
            oom_killed: false
        })
    );
    id
}

/// Asserts that a run's usage is, for each of its CPU and its peak memory, the more of what
/// the samples of its account's processes saw (64 MiB) and what its supervisor's wait said
/// (2.5 s), with none of the counters only Linux has; that its account's uid is what was
/// sampled; and that with both series asked for each comes with the result, the memory's
/// largest point the sampled footprint. Catches a native run that reports no usage (its class
/// then never learns its size), a sample of another uid, and the wait's figures ignored.
#[test]
fn a_native_run_reports_the_more_of_its_samples_and_its_wait() {
    let processes = Arc::new(Fixed::one(SAMPLED));
    let both = Series {
        cpu: true,
        memory: true,
    };
    let mut rig = Rig::new("usage-sampled").sampling(processes.clone(), both);
    let id = run_one_sampled(&mut rig);
    assert_eq!(
        rig.rt.usage(&id),
        Some(JobUsage {
            peak_memory_bytes: 64 << 20,
            cpu_usage_usec: REAPED.cpu_usec,
            cpu_throttle: None,
            cpu_pressure_some_usec: None,
            cpu_peak: None,
            peak_swap_bytes: None,
        })
    );
    let asked = processes.asked();
    assert!(
        !asked.is_empty() && asked.iter().all(|&uid| uid == me()),
        "{asked:?}"
    );
    let cpu = rig.rt.cpu_series(&id).expect("a CPU series");
    assert!(!cpu.partial && cpu.run_usec > 0, "{cpu:?}");
    let memory = rig.rt.memory_series(&id).expect("a memory series");
    assert_eq!(memory.points.iter().max(), Some(&64), "{memory:?}");
    rig.rt.remove(&id).unwrap();
}

/// Asserts that without the series asked for a run sends its usage and no series. Catches a
/// series sent that the agent was not told to record.
#[test]
fn without_the_series_a_native_run_sends_its_usage_alone() {
    let processes = Arc::new(Fixed::one(SAMPLED));
    let mut rig = Rig::new("usage-no-series").sampling(processes, Series::default());
    let id = run_one_sampled(&mut rig);
    assert!(rig.rt.usage(&id).is_some());
    assert_eq!(rig.rt.cpu_series(&id), None);
    assert_eq!(rig.rt.memory_series(&id), None);
    rig.rt.remove(&id).unwrap();
}

/// Asserts that a run whose end is read at its first poll (so no sample was taken) and whose
/// exit record has no usage (a supervisor from before the record had it) reports no usage,
/// never a zero; and that a run the runtime does not hold reports none. Catches an unknown
/// usage sent as zero, which would teach the job's class a size of nothing.
#[test]
fn a_native_run_with_neither_a_sample_nor_a_wait_reports_no_usage() {
    let processes = Arc::new(Fixed::one(SAMPLED));
    let mut rig = Rig::new("usage-none").sampling(processes, Series::default());
    rig.launcher.fail(Failing::ExitOnce);
    let id = rig.start("r0", "/usr/bin/true", &[]);
    assert_eq!(
        rig.rt.poll(&id).unwrap(),
        Status::Exited(Exit {
            code: 7,
            oom_killed: false
        })
    );
    assert_eq!(rig.rt.usage(&id), None);
    rig.rt.remove(&id).unwrap();
    assert_eq!(rig.rt.usage(&id), None);
}
