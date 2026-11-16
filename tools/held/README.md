# Held scripts

A held script is a step only the owner runs: one that spends money on a real account, or changes a
real system. Each script here is a dry run by default, never forces, and does the real thing only when
given an explicit flag. Each one names the runbook that says when to run it and what it needs first.

| Script | What it does | Runbook |
|---|---|---|
| `provider-live/provider_live.sh` | Runs the live conformance suite of one machine provider (EC2, Compute Engine or Lambda): a dry run prints the plan and its hash, `--spend --plan-hash H` launches. | `docs/runbooks/live-conformance.md` |

A new held script goes in a directory of its own here, with a row in this table, and is `bash -n` and
shellcheck clean.
