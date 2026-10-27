# Retry policy and run records

Companion to [docs/design.md](../design.md), sections 8.1 and 8.3. Section numbers refer to design.md.

## 8.1 Retry policy
Four budgets exist (4.1). The generic core gets `RetryPolicy` with those four axes by name: *own-limit* (sizing ladder, spends nothing
shared), *platform fault* (shared budget, which also takes instance reclaim, that is preempted capacity), *not started*, and *loss*. It adds a fifth,
*task failure*: the attempt ran and the work failed (a non-zero exit). kbf's budget for it is zero, because a failed action is a result and not a retry. For ucdk classes the portable
`max_retries` (UJ1) is one budget that counts every failed attempt on the platform-fault, not-started and task-failure axes together,
so a Job whose container exits non-zero is retried exactly as under the cloud's native batch engines; timeout (UJ2) applies per attempt.
The ladder and loss axes are extra capabilities, not part of the portable contract. The ucdk conformance kit has a UJ1/UJ2 case that
fails on a non-zero exit; step 24b adds the axis.

## 8.3 Run records
A submit returns only after a durable run record exists. kbf's store retention is short (a 60 s default in the scheduler, at least 600 s for done rows).
Jobs need a configurable floor, with a default of days, and the state store holds it. Record fields: request id, definition reference,
phase, tenant, labels, the resolved fence, attempts (provider, lease, start, end, outcome, usage), final
outcome, log cursor. Log text itself is not in the record; the record holds a cursor and a `complete` flag. The record is written with
additive fields (step 6).
