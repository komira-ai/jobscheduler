# ucdk and jobscheduler

Companion to [docs/design.md](../design.md), section 11. Section numbers refer to design.md.

## What is true today

jobscheduler and ucdk do not use each other. No crate of this repository depends on a ucdk crate, and ucdk's `main` names no
jobscheduler crate, type or call.

- **What ucdk is now.** ucdk on `main` is a release tool and a ledger: its crates are `ucdk-api`, `ucdk-capi`, `ucdk-cli`,
  `ucdk-codegen`, `ucdk-core`, `ucdk-ledger`, `ucdk-release` and the `ucdk-release-*` crates. It has no `Job` resource, no `run` verb, no
  plan, and no `Provider` trait. Those went with its deployment side in komira-ai/ucdk#28, which removed `ucdk-proto`, `ucdk-plan`,
  `ucdk-provider` (the `Provider` trait, the stamps and the contract kit), `ucdk-provider-fake` and `ucdk-engine`.
- **What jobscheduler offers a caller.** Jobs, not Job definitions. The public client API is the job service of `js-proto`
  (`crates/js-proto/proto/jobscheduler/v1/service.proto`): `SubmitJob`, `GetJob`, `ListJobs` (by state, label, client and the time a
  job was queued), `CancelJob`, `GetResult` and `GetLogs`, and the operator calls on hosts, pools, providers and usage. It has no
  definitions, no schedules and no scheduled triggers; a caller that wants a job on a schedule submits it when the time comes.
- **What jobscheduler launches machines with.** Its own provider contract, `js_elastic::CapacityProvider`, implemented by
  `js-provider-lambdalabs`, `js-provider-ec2` and `js-provider-gce` and registered as the kinds `lambdalabs`, `ec2` and `gce`
  (`Kinds::standard`, `crates/js-lease-server/src/kinds.rs`). It has nothing to do with the `Provider` trait ucdk had (D12).

## Superseded (2026-11-24): jobscheduler as a second realization of ucdk Jobs

This file used to hold sections 11.1 to 11.3 of the design: jobscheduler as an optional second realization of a ucdk `Job` beside
ucdk's direct launcher, a ucdk-side crate implementing ucdk's `Provider` trait for Job definitions and schedules, jobscheduler serving
those definitions, schedules and runs, the mapping of a `Job`'s fields to a request, schedule ownership, and the gaps (fence, run
store, retries, sizing, tenant). That plan was dropped when ucdk removed its deployment side in komira-ai/ucdk#28: the `Job`
resource, the `Provider` trait and its kit, and the `run` verb the plan was written against no longer exist. Nothing of it was built in
this repository. The text is in this file's git history. Other design files that mention ucdk Jobs, a ucdk class or a run service for
ucdk describe the same dropped plan.
