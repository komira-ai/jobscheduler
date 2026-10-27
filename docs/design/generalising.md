# What each part of the agent contract generalises

Companion to [docs/design.md](../design.md), section 5.3. It states, for each part of what a kbf daemon reports and receives, how the
generic contract differs. Section numbers refer to design.md.

- **Capacity.** The Hello entries `cpus`, `mem_gib`, `gpu`, `vm.slots`, `action_slots` become `Resources` in
  `crates/kbf-server/src/worker.rs`: a named map of dimensions plus the separate lease-count cap. Advertised capacity stays "what the
  agent is willing to give": it lowers it by its own reservations (`NodeReport::within` in `crates/kbf-daemon/src/report.rs`)
  and the scheduler never sees the reservation.
- **Capabilities.** The comparators in `crates/kbf-caps/src/matching.rs` (exact, at-least ISA level, feature subset, countable minimum,
  set membership, single device satisfies all) become a schema a provider registers: key to comparator kind, with namespaced keys. A
  request key unknown to the schema is refused at submit; an unknown key in a *report* is still skipped, as R3 requires.
- **Phases.** The generic phase set is the wire's. A host takes new leases only in `serving`; a launched host is `registering` from
  launch until its first hello, and takes none.

  | Phase | Meaning | New leases |
  |---|---|---|
  | registering | launched or listed, no hello yet (scheduler-side only) | no |
  | serving | normal | yes |
  | preparing, restoring | the node is switching sessions, tied to a whole-node lease (`SESSION_SWITCH_GRACE`; 5.17) | no |
  | memory-pressure | only while the node holds a lease | no |
  | draining | announced by the agent (SIGTERM): leases run to their end, nothing is requeued | no |
  | quarantined | held out | no |

  An unknown phase value is read as quarantined (fail closed). This is the one deliberate exception to "skip what you do not know", since
  skipping a phase could place work on a node that is not serving. `NodePhase` and the fence constants move with the core (`js-types`,
  `js-sched`, 10.2); the macOS logic that decides to enter preparing or restoring stays in kbf (9.3). Agent-announced draining is distinct
  from operator cordon and drain (4.1), which are scheduler state.
- **Start payload and result.** `Start.action_digest` and `Result.action_result` become an opaque payload reference and an opaque result
  blob. The kbf adapter interprets digests; the ucdk adapter interprets an image and arguments.
- **Failure taxonomy.** Keep the classes in `crates/kbf-daemon/src/runtime.rs`: Killed, Failed, Invalid, MissingInput, TimedOut,
  OutOfMemory (own limit), BusyNode (node pressure), Lost (restarted or fenced). Spot or instance reclaim shows as the agent being Lost,
  or as a capacity-provider `observe` of Absent with a reclaim reason; the work is rerun elsewhere and the platform-fault budget is spent
  (8.1), as for BusyNode. The single mapping point to the wire stays `result_of` in `crates/kbf-daemon/src/lease.rs`. `MissingInput` is
  kbf-only; the generic set names it "InputUnavailable" and the kbf adapter maps it.
- **Usage.** A usage report carries a `basis` and optional blocks, as `ResourceUsage` does, plus the censoring signals the estimator
  needs: limit hit, swap peak, out-of-memory kill count, and node pressure at start. Usage is reported on failure too
  (`Runtime::failed_usage`).
