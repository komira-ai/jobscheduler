# Multi-instance server: review findings and open questions

Companion of [multi-instance.md](multi-instance.md), moved out of it so that file stays under 1000 lines (R12).

## Review findings and responses

A critic reviewed the first draft (eleven findings, one blocker). They are accepted in substance and the current text reflects them; where the
reviewer's remedy was not taken as written, or a claim was not re-checked, it says so. Finding 9 no longer applies and is
not listed, so the numbering has a gap there.

| # | Finding | Response |
|---|---|---|
| 1 | The flip contract contradicted `host-live` | Accepted. Fixed in "Presence": same credential and agent `instance` take over at once, a draining owner writes a release mark, and the difference from `registry.rs` is stated. The reviewer proposed "same credential or instance"; the rule here needs both to match, because the hello already carries the `instance` that `admits(credential, instance)` uses, and a credential alone cannot tell a flip from a second process. Checked in `registry.rs` (the `HostLive` refusal follows the `HostBound` check). |
| 2 | Heartbeat budget false for per-job hosts | Accepted. The row is split; a feed-fed job and allocation cache is required (PR 3); end-to-end times added. The "about 25 reads per beat" is the reviewer's count and was not re-measured; PR 3 measures it. |
| 3 | Taint hand-off broken when placer and owner differ | Accepted. "Taint hand-off": the taint is written into the binding in the placement txn; the owner reads it; deletion is by version. The reviewer's order "write binding then drop row" is made atomic instead of ordered. |
| 4 | Startup backfill unsafe beside a live instance | Accepted. "Startup index backfill": out of open, once at copy, then a periodic duty with a marker row and retry. |
| 5 | Cutover step 2 not reversible | Accepted, with the reviewer's first remedy (export tool plus a dry run). Step 2 is stated as reversible only through the export; the original file is a backup. |
| 6 | `hostbook` under-specified | Accepted. It is a derived cache; one allocation write helper bumps it; the callers are listed; absent is not zero; reconcile per row; all `Resources` fields. The caller list comes from the reviewer and a grep of `js-jobs` and `js-lease-server`; PR 5 owns the exhaustive list through the helper test. |
| 7 | First-listing loss becomes routine and destructive | Accepted. "First listing after a flip": `Starting` is re-sent, lost only after `start_confirm`. Needs an idempotent agent start (PR 5a). The reviewer's alternative (key the test to the presence epoch) was not taken: the epoch changes on every flip, which is the case to protect. |
| 8 | Settle without a presence precondition; time sources | Accepted. Settle carries `Cond::Version` on presence and hostbook; `last_heard` is the store's commit time. |
| 10 | Rolling updates run N and N+1 on one store | Accepted. "Mixed-version instances", `min_protocol` on duty rows, `n_and_n_plus_1_together`, and the `StaleCursor` reload. |
| 11 | Model check incomplete, a gate that has not run | Accepted. Properties and actions added, with which are run in CI and which are author-run; the spec is stated as not yet written or run. TLC counts are a record, not a CI comparison. |

### Second round (after the leaderless revision)

A second critic reviewed the leaderless text. Four findings, all raised against this document, answered below. Finding 2 is accepted in its
facts and changes an owner-listed recommendation, which is why it is also an open question.

| # | Finding | Response |
|---|---|---|
| 1 | The order of the head lock and the row locks is not stated, and both orders break (row-locks-first deadlocks with a head-first free; head-first makes `SKIP LOCKED` and the pool row dead code and puts the engine's compute inside the global lock) | Accepted. "The placement transaction" now states one order: pool row (`SKIP LOCKED`, only a pass takes it), then the head lock, then other rows in key order, and reads and the engine run before the head lock as plain reads validated by versions afterwards. Both rejected orders are written up with the reason. The Why-each-lock table, parallelism text, the latency rows (pool-row hold and head-lock hold given separately), the diagrams and the model (`LockOrder`, with a negative case) follow. The critic's case 2 is right for the existing store: after the head lock `SKIP LOCKED` on any other row never skips, so the doc stopped using it there (duty batches, scaler claims) and keeps it for the one lock taken before the head lock. A deadlock the server reports anyway is retried by the store helper (`txn_retry_max`). The head-lock hold is inferred (3r to 4r), not measured; PR 5 measures it. |
| 2 | Heartbeat ordering relies on `sent_unix`, which does not exist | Accepted in fact: no `sent_unix` exists in this checkout, and `Heartbeat` (`js-agent-wire/src/heartbeat.rs`) has no send-time field. The owner's recommendation named an existing field with a guard and no proto change; the premise was wrong. Within the owner's intent (no proto change, no agent change) the guard is dropped, and ordering is by session: one connection per session orders beats, and across sessions the session secret, epoch and the CAS on the presence row reject a superseded beat (see "Presence"). Adding a timestamp field instead would be a wire change that old agents do not send and, as an agent wall-clock value, would ignore every beat after a clock step back; it is not proposed. Open question 2 asks the owner to confirm the change. |
| 3 | A stale `hostbook` row after rollback then roll-forward over-commits a host until the periodic reconcile | Accepted. The remedy is both of the critic's options in a form that costs no new writer: a `hostbook/_gate` row records the `writer_generation` at which every row was last exact (the previous release bumps it at every start, the new one only reads it), every placement transaction checks it after the head lock, and an instance reconciles every host before it enables its first pass if the gate is stale. Rows also carry a `writer` tag for later rule changes. The periodic reconcile stays as a backstop only. Test `rollback_then_roll_forward_does_not_overcommit`; properties `HostbookGate`. |
| 4 | Release rollback is described two contradictory ways | Accepted. "Compatibility": a rollback to a leaderless release overlaps two instances (start-first), a rollback to the single-writer release is stop-first with a deploy gap, and the updater hook picks the order from the target's `protocol` and defaults to stop-first. The owner's "release rollback as a service update" stays; only the order of that update depends on the target. Property `RollbackOrder`. The remark that the old process's `save()` wipes `hosts/` keys is kept as the reason. |

## Open questions for the project owner

Decided by the owner and built into the design above, so not questions: Postgres as the single source of truth with no leader; the database as
the central dependency, failing closed; one instance in steady state, two only while an update overlaps; a presence write per host about every
5 s; no agent stickiness beyond TCP keep-alive plus a server-side connection max age; job-store cutover by a short pause with a dry-run copy and a
Postgres-to-SQLite export for rollback; a hello with the same credential and agent instance takes the host over at once; the loss of one unflushed
usage buffer on instance death is acceptable (here reduced by writing usage with the presence refresh); no coordinator orders while the database is
unreachable; zero staleness for deny rows and revoked tokens; the initial-admin file stays on the shared volume for now; `fleet_hosts` unlogged
only if measurement shows WAL cost; release rollback as a service update with migrations the previous release tolerates (the update's order depends on the target, see "Compatibility").

Still open:

1. **The coordinator's Mac LAN listener is host-mode and cannot bind on two tasks at once.** A start-first update that overlaps two tasks
   fails to bind the second. Options:
   - **A. SNI route through the edge.** The Macs connect to a hostname; the edge proxy (raw TCP, TLS passthrough) routes by SNI to the
     coordinator's overlay address, and the listener leaves host-mode. Works for any N and for overlap, with the same health check and drain as
     the server's pools. Cost: Macs must send SNI to a name the edge serves (a TLS client to a name already does), and the edge gains a route.
   - **B. Ingress mesh (published port through the orchestrator's routing mesh).** No edge change, but the mesh hides dead tasks and balances
     long connections poorly (the same reasons Routing rejects the VIP), and it may change the Macs' visible source address.
   - **C. One task for this listener.** Run it as its own single-replica, stop-first service. No protocol change, and the Macs see a gap of the
     stop grace plus a start on every update, retrying. Fine if Macs tolerate a short reconnect, which their bootstrap already does.
   Recommended: A, because it is the only one that serves through an update and it reuses what the edge already does; C as the fallback if Macs
   cannot send SNI. Not measured here: which Macs send SNI today.

2. **Heartbeat ordering: the existing `sent_unix` does not exist.** The recommendation was to order beats by the `sent_unix` the agent already
   sends, with a guard and no proto change. `Heartbeat` has no send-time field. Options:
   - **A. Order by session, add nothing (this document).** One connection orders beats within a session; the session secret, epoch and the CAS on
     `presence/<host>` reject a beat from a superseded session. No wire change, no agent change, nothing silently off for old agents, and no guard
     that a clock step could turn into ignored heartbeats.
   - **B. Add a send-sequence field to `Heartbeat`.** A per-session counter (not a wall clock) reset by hello; additive on the wire, but old
     agents do not send it so the guard is off for them, and it protects only a case A already covers.
   - **C. Add an agent wall-clock `sent_unix`.** Rejected: a clock step back (NTP correction, VM restore) makes the owner ignore every beat until
     the clock passes the old value, the host looks silent and is settled after grace plus retention.
   Recommended: A. Not measured: whether any path delivers a beat of a superseded session to a current owner; the model's `NoStaleCommit` and
   `AgentConverges` check that it cannot commit.
