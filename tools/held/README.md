# Held scripts

A held script is a step only the owner runs: one that spends money on a real account, or changes a
real system. Each script here is a dry run by default, never forces, and does the real thing only when
given an explicit flag. Each one names the runbook that says when to run it and what it needs first.

| Script | What it does | Runbook |
|---|---|---|
| `provider-live/provider_live.sh` | Runs the live conformance suite of one machine provider (EC2, Compute Engine or Lambda): a dry run prints the plan and its hash, `--spend --plan-hash H` launches. | `docs/runbooks/live-conformance.md` |
| `required-checks/require_macos_checks.sh` | Makes `macos` and `js-agent-macos-pkg` required checks of `main`: a dry run prints the checks it would add, `--apply` adds them and removes none. Run it after the change that makes every pull request run them is merged. | `docs/release.md` (Configuration the project owner must set) |

A new held script goes in a directory of its own here, with a row in this table, and is `bash -n` and
shellcheck clean.
