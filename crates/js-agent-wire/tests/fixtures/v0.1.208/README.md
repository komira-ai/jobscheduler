# Bodies of release v0.1.208

The JSON the agent wire of release `v0.1.208` sends, a release in which a server queued
runs and whole-node leases and answered an agent's heartbeat with run-keyed commands. The
wire crate is the same in every release from `v0.1.208` to `v0.1.214` (`git diff v0.1.208
v0.1.214 -- crates/js-agent-wire` is empty), so these are also the bodies of `v0.1.213`,
the last release whose server queued runs. Each body was written by that tag's own code and is stored as it came out: the
two hellos by `crates/js-agent/src/contract.rs` of the tag (`report` for the container agent,
`report_native` for the native one) over a hello of the wire crate, and the heartbeat and
the reply by the tag's wire types. They are files, not values built from the current types,
so a change to those types cannot move them.

- `hello_per_job.json`: a container agent. It reports `per-job` and `start-fence`, and
  `runtime` and `exec` are `container`.
- `hello_native.json`: a native agent (`sandbox-exec`). It reports no `per-job` and lists
  a held run.
- `heartbeat.json`: two held runs, two run results, a load with lease minutes, one job
  listed running and one with its result, and a retired job.
- `reply.json`: what a server of that release answers: a run-keyed start, a stop and a
  cancel beside a job start and a job stop, a result acknowledgement of each kind, a job
  acknowledgement and a retired acknowledgement.

`tests/previous_release_v0_1_208.rs` decodes them with the current types, and decodes the
reply a current server builds for such an agent with a copy of that release's structs.
`crates/js-registry/tests/previous_release_heartbeat.rs` feeds them to the registry.
