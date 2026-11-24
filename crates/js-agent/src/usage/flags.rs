//! The flags of what the agent samples beyond a job's usage, which both builds take: each
//! job's CPU and memory second by second, sent with its result, and the whole host's CPU each
//! second, sent with the load.

/// What the agent records beyond each job's usage. Both builds take every flag; each is off
/// by default.
#[derive(Debug, Clone, Default, clap::Args)]
pub struct SampleFlags {
    /// Record each job's CPU second by second and send it with the job's result
    /// (`cpu_series`): at most 128 points of a sixteenth of a core, a granularity that
    /// doubles when a run outlasts them, and the agent's own run length. Read from the same
    /// samples as the job's usage: on Linux each job's cgroup (as the job's CPU peak is), by
    /// the crun runtime only (a job of the container engine records none); on a Mac the CPU
    /// time of the run's processes, by the native runtime.
    #[arg(long)]
    pub cpu_series: bool,
    /// Record each job's memory about once a second and send it with the job's result
    /// (`memory_series`): at most 128 points in MiB, each the most of its span, combined in
    /// pairs by their maximum when a run outlasts them. On Linux the non-file memory of the
    /// job's cgroup (`memory.current - (file - shmem)`: no page cache, shared memory and tmpfs
    /// kept), by the crun runtime only; on a Mac the physical footprint of the run's processes
    /// together, by the native runtime. The job service books a class's memory from the peak
    /// of it.
    #[arg(long)]
    pub memory_series: bool,
    /// Sends, in each heartbeat, the busy CPU of the whole host for each second of the beat's
    /// window (at most 16 values, newest last) and, on Linux, its CPU pressure: the agent
    /// reads the host's CPU counters every second (`/proc/stat` and `/proc/pressure/cpu` on
    /// Linux, the host's CPU load on a Mac) and the heartbeat carries the rates between those
    /// readings. Telemetry only; the lease server lists it on `GET /v1/hosts` and nothing
    /// places or sizes by it. The readings ride with the load, so on Linux without an actions
    /// control group (no `--actions-cgroup` for the container engine, none delegated for
    /// crun) there is no load and the flag does nothing; a host with no `/proc/pressure/cpu`
    /// sends the busy values alone, as a Mac does.
    #[arg(long)]
    pub host_second_samples: bool,
}

impl SampleFlags {
    /// The three flags with whether each was given: an agent that runs no job refuses them.
    #[must_use]
    pub fn given(&self) -> [(&'static str, bool); 3] {
        [
            ("--cpu-series", self.cpu_series),
            ("--memory-series", self.memory_series),
            ("--host-second-samples", self.host_second_samples),
        ]
    }
}
