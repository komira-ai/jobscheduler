# Held scripts

A held script is a step only the owner runs: one that spends money on a real account, or changes a
real system. Each script here is a dry run by default, never forces, and does the real thing only when
given an explicit flag. Each one names the runbook that says when to run it and what it needs first.

| Script | What it does | Runbook |
|---|---|---|
| `provider-live/provider_live.sh` | Runs the live conformance suite of one machine provider (EC2, Compute Engine or Lambda): a dry run prints the plan and its hash, `--spend --plan-hash H` launches. | `docs/runbooks/live-conformance.md` |
| `live-update-drill/live_update_drill.sh` | Updates the js-server Swarm service (two replicas, start-first, one task at a time) with `docker service update --force` while a build runs, and checks that no job ended lost, failed or failed to start, every host takes jobs again, both tasks were replaced and the update `completed`: a dry run reads the hosts, running jobs, service spec and tasks, `--update` updates. | `docs/runbooks/live-update-drill.md` |

| `submit-rollout/submit_rollout.sh` | Checks that the newest release contains a commit and that the js-server Swarm service runs it: a dry run reads the release, the running container's version and the release run's control-plane deploy; `--apply` re-runs that run's failed jobs, only when its deploy failed. Its read-only companion `submit-rollout/submit_rpc.sh` measures the SubmitJob round trip (the frontends' `rpc` step, p50 and p90) over a window. | `docs/runbooks/submit-rollout.md` |

A new held script goes in a directory of its own here, with a row in this table, and is `bash -n` and
shellcheck clean.
