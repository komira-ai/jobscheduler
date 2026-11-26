# CI

`.github/workflows/ci.yml` checks every pull request, every merge group of the merge queue and
every push to `main`; its header and the comment above each job say what the job runs. Every job
a pull request or a merge group starts runs on GitHub's hosted Linux x86_64 runners. macOS and
arm64 are the beta stage of the release, on the push to `main` (`docs/release.md`).

## The merge queue

With the ruleset below on, `main` takes a pull request only through GitHub's merge queue. A
pull request whose checks passed is added to the queue. The queue makes a branch `gh-readonly-queue/main/pr-<n>-<sha>`
holding `main`, the pull requests ahead of it in the queue and its own squash commit, runs
`ci.yml` there (event `merge_group`), and moves `main` to that commit when the required check
passed. A pull request is not brought up to date with `main` and checked again every time
`main` moves: it is checked once more, on top of exactly what will be ahead of it.

The one required check is `all-passed`, the last job of `ci.yml`. It needs every other job of
the workflow and fails when any of them failed, was cancelled or was skipped, so a job that did
not run is a red check, never a skipped one that counts as passed. Every job runs for a merge
group and reads the group's change from its `base_sha` (the site steps of `lint`, the coverage
comparison with the base); `crates/xtask/tests/merge_queue` holds `ci.yml` and this section to
that.

The ruleset of `main` (the body of `POST /repos/{owner}/{repo}/rulesets`):

```json
{
  "name": "main",
  "target": "branch",
  "enforcement": "active",
  "conditions": {
    "ref_name": {
      "include": ["refs/heads/main"],
      "exclude": []
    }
  },
  "rules": [
    {
      "type": "required_status_checks",
      "parameters": {
        "strict_required_status_checks_policy": false,
        "do_not_enforce_on_create": false,
        "required_status_checks": [
          { "context": "all-passed", "integration_id": 15368 }
        ]
      }
    },
    {
      "type": "merge_queue",
      "parameters": {
        "merge_method": "SQUASH",
        "grouping_strategy": "ALLGREEN",
        "max_entries_to_build": 5,
        "min_entries_to_merge": 1,
        "max_entries_to_merge": 5,
        "min_entries_to_merge_wait_minutes": 5,
        "check_response_timeout_minutes": 360
      }
    }
  ]
}
```

and the repository's squash message settings are `squash_merge_commit_title: PR_TITLE` and
`squash_merge_commit_message: BLANK`. Why each:

- **Squash, as before.** `main` gets one commit per pull request.
- **The message is the title alone.** The queue writes the squash message from the
  repository's default settings; nobody passes one when queueing. The title gives
  `<title> (#<n>)`. The body is left blank: the repository is public, and the default body
  (`COMMIT_MESSAGES`) would copy every branch commit message into `main`'s history.
- **`all-passed` is required, from GitHub Actions alone** (`integration_id` 15368), so no other
  app's status of the same name passes it. `strict_required_status_checks_policy` is off: the
  queue tests each pull request on top of what is ahead of it, so it need not be up to date.
- **Each pull request is checked on its own (`ALLGREEN`).** Every queued pull request's own
  commit must pass before it merges, so `main` never holds a commit whose check failed, not
  even inside a group whose last commit passed.
- **Up to five build at once and up to five merge together; a group of one does not wait.**
  Five queued pull requests building at once give up to five merges per run of `ci.yml`
  instead of one. When the first ones in the queue pass together they reach `main` in one
  push. `min_entries_to_merge: 1` means a pull request never waits for others to make a
  group, so the five minutes of `min_entries_to_merge_wait_minutes` never apply.
- **The check timeout is 360 minutes, the most GitHub allows.** The longest chain of
  `ci.yml`'s jobs is `coverage-shard`, `coverage` and `all-passed`, 95 minutes of
  `timeout-minutes` (the test holds the chain under this timeout). A job's timeout counts from
  when a runner takes it, not the wait for one, and five groups run five copies of the
  workflow beside the pull requests' own runs, so the wait for runners can be long. A job that
  hangs still ends red at its own timeout, so the queue sees a failure, not a silence.
- **Advisories never block.** `deny` fails on licences, bans and sources; its advisories step
  is shown and does not fail the job.

The ruleset and the two squash settings are turned on by the held script
`tools/held/merge-queue/merge_queue.sh`, run by an admin of the repository. It is a dry run by
default: it prints the settings and the ruleset now and after, and the exact API calls. With
`--apply` it writes the squash settings first, then the ruleset (a new ruleset `main`, or the
existing one with its other rules kept), reads both back, and prints the calls that undo the
change: a `DELETE` of the ruleset it created (or a `PUT` of the old body), and a `PATCH` back to
the old squash settings. It stops before writing anything when the repository is not public, when
`ci.yml` on `main` does not start on `merge_group` or has no job `all-passed`, or when another
ruleset already puts a merge queue or required checks on `main`. Deleting the ruleset turns the
queue off, and pull requests merge directly again.

A pull request whose group fails leaves the queue. The queue removes it and builds the pull
requests behind it again without it; it comes back only when it is added to the queue again,
after a fix.

The checks of a pull request that are not in `ci.yml` (`js-agent-deb.yml`,
`js-agent-crun-runtime.yml`, `js-agent-quota-tests.yml`, `egress-spike.yml` and `release.yml`'s
dry run) run when a pull request changes the paths they name. They run on the pull request
before it is queued, not on a merge group, and are not required. `farm.yml`, the build farm lane
(`docs/farm.md`), runs on every pull request from a branch of this repository and on the push to
`main`, likewise not on a merge group, and is not required.

The macOS checks keep their place: the beta stage of `release.yml` on the push to `main`
(`beta.yml`, `js-agent-macos-pkg.yml` and `macos-probe.yml`), which holds back only the macOS
archives of a release. A merge group, like a pull request, runs nothing on macOS. When the
repository variable `MACOS_ON_FARM` is `true`, the Apple silicon archives and the installer package
of that stage are built on the build farm's Macs from a Linux runner instead of a hosted macOS
runner (`macos-farm.yml`, [mac-farm.md](mac-farm.md)); the tests and the probe stay hosted. A change
whose macOS tests should be seen before it merges is tried by a manual run of `beta.yml` on its
branch (`gh workflow run beta.yml --ref <branch>`), the same job on the same hosted runner.
