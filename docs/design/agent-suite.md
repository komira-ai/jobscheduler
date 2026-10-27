# Agent conformance suite

Moved from design.md section 13.2. Section numbers refer to design.md.

*Agent suite:*

- start is idempotent on lease id; a `Cancel` is resent while the lease is listed and the lease leaves the heartbeat;
- a stale-lease result is refused; a duplicate is acknowledged and ignored;
- reconciliation frees an absent lease and keeps an unknown one; a lease omitted from a heartbeat is absent only once its `Start` is due
  (5.2, graded by session and process as in 4.4) and pending before then, and silence is not absence;
- a Start past its validity bound produces `NeverStarted`, no Result and no listing; the heartbeat lists unacknowledged results and the
  first heartbeat after re-adoption is complete;
- an unknown phase value is read as quarantined; usage is reported on failure and on kill;
- each start-contract rule (5.2) refuses before any call: non-positive wall time, unstated fence, reserved label key, retries passed on;
- after a job the container and scratch are gone before the slot is freed, and a failed removal frees nothing and taints; a `host` `Start`
  is refused without all three consents (5.12); a host job taints on completion whatever its outcome; each post-job check (stray
  process, file outside the roots, mounts, users, kernel modules, cleanup failure) taints on its own, and a check that cannot run is a
  finding; a taint is resent until acknowledged, survives an agent restart, and is not cleared by a later clean hello; a tainted host
  takes no `Start`; a fake agent that reports no taint after a host job still leaves the host tainted at the scheduler (recorded at the
  `host_jobs` grant or the host `Start`), which grants it no new lease, and a Result, hello or heartbeat does not untaint it;
- per session (5.1): the leasee's session obeys the cancel, grace, ack and never-started rules for its placements, and the scheduler
  cancels none of them; a hello that announces a lease not committed to its host id is answered `Cancel`; a mount outside the agent's
  roots is refused.

