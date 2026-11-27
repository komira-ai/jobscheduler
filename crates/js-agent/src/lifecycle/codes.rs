//! The codes the lifecycle reports: the `detail` of a result ([`detail`]) and the code of a
//! [`Finding`](super::Finding) ([`code`]).

/// The `detail` of a `failed_to_start` result when the cause is not a wire refusal code.
pub mod detail {
    /// The scratch directory could not be made.
    pub const SCRATCH_FAILED: &str = "scratch-failed";
    /// The runtime could not create or start the container.
    pub const RUNTIME_FAILED: &str = "runtime-failed";
    /// The job names a GPU and the runtime's containers cannot use one (a container on a
    /// Mac cannot reach the Apple GPU). Nothing is created.
    pub const GPU_UNSUPPORTED: &str = "gpu-unsupported";
    /// The job carries run files and this agent has no files root. A server sends files
    /// only to an agent that reported the `run-files` capability, so this is a fault of the
    /// sender.
    pub const FILES_UNSUPPORTED: &str = "files-unsupported";
    /// The job is a host job and carries run files, which only a contained job (a container,
    /// or a native run's own user) can be given. Nothing is created.
    pub const FILES_NEED_CONTAINER: &str = "files-need-container";
    /// The network the job would get is not limited as the agent was told: the `crun`
    /// runtime's firewall (`--egress`) or a Mac pool's packet filter (`--native-network on`)
    /// is missing or changed. Nothing is created.
    pub const EGRESS_UNCHECKED: &str = "egress-unchecked";
    /// The job may change the host and the taint marker could not be written, so the job
    /// was not started.
    pub const TAINT_FAILED: &str = "taint-failed";
    /// The detail of a `killed` result when the agent stopped the job because it had not
    /// heard from the server for its silence bound (`--silence-stop`).
    pub const SILENCE_STOP: &str = "silence-stop";
}

/// The codes of a [`Finding`](super::Finding).
pub mod code {
    /// The log tails of the held results did not fit the heartbeat even cut down; the tail
    /// of this run was left out of its result.
    pub const LOG_TAIL_SHED: &str = "log-tail-shed";
    /// The runtime could not read the log of a job that exited; its result goes without a
    /// tail.
    pub const LOGS_FAILED: &str = "logs-failed";
    /// A job start's live-log sink failed its check: the job runs without live logs (no
    /// uploader, no `logs` in its result), and is otherwise as a start without one. The
    /// message names the field and the rule it broke, never a value.
    pub const LOG_SINK_IGNORED: &str = "log-sink-ignored";
    /// The runtime could not tell whether a container has exited.
    pub const POLL_FAILED: &str = "poll-failed";
    /// The runtime could not stop a container.
    pub const STOP_FAILED: &str = "stop-failed";
    /// The runtime could not start a container.
    pub const START_FAILED: &str = "start-failed";
    /// The runtime could not remove a container.
    pub const CONTAINER_REMOVE_FAILED: &str = "container-remove-failed";
    /// A native run was removed (its processes and its user are gone) and one or more of
    /// its declared outputs could not be copied, or could not be committed to the put
    /// ingest. The host holds nothing of it: not a taint.
    pub const OUTPUTS_FAILED: &str = "outputs-failed";
    /// A native run's removal committed its collected outputs to the put ingest as one
    /// object. The message is a JSON object with the fields `key` (the key the object is
    /// stored under), `sha256` (the SHA-256 of its bytes) and `bytes` (their length).
    pub const OUTPUTS_COMMITTED: &str = "outputs-committed";
    /// The scratch directory could not be made or removed.
    pub const SCRATCH_FAILED: &str = "scratch-failed";
    /// A labelled container could not be accounted for after a restart.
    pub const UNADOPTABLE: &str = "unadoptable-container";
    /// A container that could not be accounted for could not be removed either, so it is
    /// still on the host, unbooked.
    pub const UNADOPTABLE_KEPT: &str = "unadoptable-container-kept";
    /// A started container's process is not in the agent's `/proc`: the agent does not see
    /// the host its jobs run on. The container was removed and the host is tainted as
    /// unchecked.
    pub const HOST_NOT_VISIBLE: &str = "host-not-visible";
    /// A run the runtime reports as running has no container group (the control group the
    /// runtime puts every container in) a settle wait after the agent first saw it, and its
    /// container's first process runs in another group (a container not made yet, or one
    /// that ended and was removed with its group, is neither). The host is tainted as
    /// unchecked. The post-job checks do not depend on that group, so
    /// what the run leaves is still found, and a re-check of an idle host clears the taint.
    pub const CGROUP_NOT_FOUND: &str = "cgroup-not-found";
    /// The taint marker could not be written. The host is tainted in memory all the same.
    pub const TAINT_WRITE_FAILED: &str = "taint-write-failed";
    /// A taint cleared itself: the host passed two re-checks while idle, the server
    /// acknowledged the clear, and the marker is kept as `taint.json.cleared-<time>`.
    pub const TAINT_CLEARED: &str = "taint-cleared";
    /// The clear of a taint could not be finished (a record of owed checks could not be
    /// removed, or the marker could not be renamed). The host stays tainted and the clear is
    /// tried again at the next acknowledgement.
    pub const TAINT_CLEAR_FAILED: &str = "taint-clear-failed";
    /// The record that a job's checks are owed could not be written or removed. While it
    /// cannot be written the job's container is not removed; while it cannot be removed
    /// the job's slot is not freed.
    pub const OWED_FAILED: &str = "owed-record-failed";
    /// The project id of a slot still holds usage (or its usage cannot be read), so the slot
    /// takes no run. The slot is read again at every step and is free once the usage reads
    /// zero; it is reported when it is parked, and again every hour while it stays parked.
    pub const QUOTA_SLOT_BUSY: &str = "quota-slot-busy";
    /// A finished run's project still read usage after the time the agent waits for it
    /// (`--scratch-usage-wait-secs`): the run's cleanup went on, the slot was parked and the
    /// run's place and booked bytes were given back, and the quota counted one failure.
    pub const QUOTA_USAGE_ABANDONED: &str = "quota-usage-abandoned";
    /// A parked slot read zero usage again and takes runs.
    pub const QUOTA_SLOT_RELEASED: &str = "quota-slot-released";
    /// A finished run's project still reads usage after its tree was removed (the file
    /// system frees inodes in the background): the slot stays held and is read again at
    /// every step.
    pub const QUOTA_USAGE_HELD: &str = "quota-usage-held";
    /// The quota failed several calls in a row and the probe no longer finds project quota:
    /// no run starts until it works again. Reported again every minute.
    pub const QUOTA_UNHEALTHY: &str = "quota-unhealthy";
    /// An unhealthy quota probed `Project` again.
    pub const QUOTA_RECOVERED: &str = "quota-recovered";
    /// The quota of a finished run's scratch directory could not be cleared. The tree is
    /// gone; the slot stays held and the clear is tried again at every step.
    pub const QUOTA_CLEAR_FAILED: &str = "quota-clear-failed";
    /// The records of owed checks in the state root could not be listed when the agent
    /// started, so the host is tainted ([`reason::CHECK_OWED`](crate::lifecycle::taint::reason)).
    pub const OWED_UNREADABLE: &str = "owed-records-unreadable";
    /// A record of owed checks found when the agent started does not hold its run id (a
    /// write cut short), so the host is tainted
    /// ([`reason::CHECK_OWED`](crate::lifecycle::taint::reason)) and the check is not run.
    pub const OWED_INCOMPLETE: &str = "owed-record-incomplete";
    /// The checks owed by runs of an earlier process (records found when the agent started)
    /// ran at the start, passed, and their records were removed: the host is not tainted.
    pub const OWED_CHECKED: &str = "owed-checked";
    /// A held result or the retired list could not be written to the state root.
    pub const HELD_FAILED: &str = "held-record-failed";
    /// A held result or the retired list in the state root could not be read at start.
    pub const HELD_UNREADABLE: &str = "held-record-unreadable";
    /// The agent had not heard from the server for its silence bound and stopped the job
    /// (its result says [`detail::SILENCE_STOP`](super::detail::SILENCE_STOP)).
    pub const SILENCE_STOP: &str = "silence-stop";
    /// A run the agent adopted after a restart could not be offered its host ports again,
    /// and the agent stopped it (its result's `input_failure` says
    /// [`refusal::HOST_PORT_LOST`](js_agent_wire::refusal::HOST_PORT_LOST)).
    pub const HOST_PORT_LOST: &str = "host-port-lost";
    /// A mount provider did not answer the release of a run's sources within a minute after
    /// the run ended: the sources named are left to the provider's own collection. The host
    /// is not tainted and the run's slot is free.
    pub const PROVIDER_RELEASE_FAILED: &str = "provider-release-failed";
    /// A mount provider has not answered the sweep the agent asks of it when it starts; it is
    /// asked again every ten seconds, and a start that names it waits meanwhile. Reported once
    /// for each provider.
    pub const PROVIDER_SWEEP_FAILED: &str = "provider-sweep-failed";
    /// A mount provider did not serve a `Prepare`, `Mount` or `Release` of a run's source
    /// (it answered unavailable or refused it, or did not answer within the call's bound),
    /// and the call waits before it is sent again. One for each run at each such wait, not one
    /// for each call: the message names the provider, the call, the source, the failures in a
    /// row, the provider's reason, the wait in milliseconds, how many more sources of the run
    /// wait with it, and the agent's counts of the `Prepare`s not served and the calls sent
    /// again since it started.
    pub const PROVIDER_BACKOFF: &str = "provider-backoff";
    /// The record of a run's provider sources (`providers/<run>.json` in the state root) could
    /// not be written, so the start failed; or, when the agent started, a record could not be
    /// read, or named a provider this agent was not given (either is removed, its sources left
    /// to the provider's own collection), or the directory of records could not be listed.
    pub const PROVIDER_RECORD_FAILED: &str = "provider-record-failed";
}
