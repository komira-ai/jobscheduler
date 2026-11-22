# Live update drill

What it proves: an update of js-server while a real build runs on the farm loses nothing. The
server runs as one Swarm service of two replicas, updated start-first one task at a time, so
one `docker service update --force` rolls each task in turn, and for a while three instances
share the Postgres store: a new task starts and turns ready, then an old task gets SIGTERM,
twice. On that signal the old task answers `/readyz` 503, closes the listener of the agent
routes, answers each held heartbeat and dispatch poll at once, finishes the placement or claim
it is in and places and claims nothing more; each agent's next request reaches another task,
which serves the agent's session from the host's row (no hello is repeated), and the agent
keeps its runs. The drill checks the outcome from outside: no job ends lost, failed or failed
to start while the update runs, every host that took jobs before takes jobs after, both tasks
were replaced, and the service's update ended `completed`.

The same update is proven in process, with no farm, by
`crates/js-lease-server/tests/server/live_update_load.rs`: two rolls under load with each old
server signalled inside a placement and inside a dispatch claim, and a server killed after a
claim whose reply was lost. The drill is the check that the deployment's spec and edge do what
that test assumes.

It is a held step: the owner runs it, on the Swarm manager node, with the script
`tools/held/live-update-drill/live_update_drill.sh`. It is a dry run unless given `--update`.

## Before

- The release that carries the drain (the pull request that added this runbook) is the one the
  service runs, and the next release is published, so the forced update starts a server of the
  same code.
- The service runs two replicas, and its update is `order: start-first`, `parallelism: 1`,
  `failure_action: rollback`; its health check reads `/readyz`, and its `stop_grace_period` is
  longer than `--drain-hold` plus `--drain-timeout` plus one tick (40 s with the defaults), so
  the engine's SIGKILL never cuts the drain ([release.md](../release.md), "Stopping"). The
  script reads the spec with `docker service inspect` (read-only, dry run included) and
  refuses `--update` unless the replicas equal `--replicas` (2), the order is `start-first`,
  the parallelism is 1, the failure action is `rollback` and the stop grace period is at least
  40 s.
- Both tasks are running (`docker service ps --filter desired-state=running`): `--update`
  refuses unless there are exactly `--replicas` of them, each `Running`.
- `js`, `docker` and `jq` are on the manager's `PATH`, and a client token file of the job service
  is at hand.

## Steps

1. Start a real build on the farm, large enough to run for several minutes with many actions
   (a full build with remote execution through the job service).
2. While it runs, read the farm with a dry run, which changes nothing:

   ```sh
   tools/held/live-update-drill/live_update_drill.sh --server https://<job service> \
     --token-file <client token file> --out-dir <workspace scratch directory>
   ```

   It prints how many hosts take jobs, how many jobs run, each finding about the service's spec,
   how many tasks run, and the update it would make. A run
   that finds no running job means the build has not reached the farm yet.
3. Run the drill:

   ```sh
   tools/held/live-update-drill/live_update_drill.sh --server https://<job service> \
     --token-file <client token file> --out-dir <workspace scratch directory> --update
   ```

   It runs `docker service update --force --detach=false js-server_server`, which rolls the two
   tasks one at a time, reads the hosts while the update runs, then reads the tasks and the
   service again, gives the hosts `--settle-samples` readings (30, two seconds apart) to take
   jobs again, and reads the jobs that ended lost, failed or failed to start since the update
   began.
4. Let the build finish and read its own summary: the drill passes only with zero failed actions
   in the build as well as the script's `PASS`.

## Reading the verdict

- `PASS`: every host that took jobs before takes jobs again, no job ended badly, both tasks are
  new and the service's update is `completed`. With the build's zero failed actions, the drill
  passed.
- `FAIL: the update ended with status ...`: the update did not converge (a new task did not
  turn ready, and Swarm rolled back). `update.log` in the out-dir has Docker's output.
- `FAIL: hosts that took jobs before the update and do not now`: the named hosts did not come
  back within the settle readings. `hosts-during-<n>.txt` shows when each was last seen.
- `FAIL: jobs that ended lost, failed or failed to start during the update`: each is named with
  its state; `js job get --job-id <id>` gives its events.
- `FAIL: the update did not replace every task`: after the update there are not `--replicas`
  running tasks, or one of them is a task of `tasks-before.txt` (named under the line).
- `FAIL: the service update ended <state>, not completed`: the service's `UpdateStatus.State`
  is not `completed`; a rollback ends `rollback_completed`. `service-after.json` has it.

A refused `--update` (status 2, nothing updated) names what is missing: no running job, no host
taking jobs, a spec finding marked `FAIL` in the output above it, or a running task count that
is not `--replicas`.

Every reading is kept in the out-dir: `service-before.json`, `tasks-before.txt`,
`hosts-before.txt`, `hosts-during-<n>.txt`, `update.log`, `tasks-after.txt`, `tasks-kept.txt`,
`service-after.json`, `hosts-after.txt`, `hosts-missing.txt` and `jobs-ended-badly.txt`.

## What it does not cover

Admission and placement read the `node/` rows of the job store as well as the asked server's
registry, so a job submitted to the new server before any host has said hello to it is admitted
and placed for every host whose row a server of this release wrote: it is not refused
`no-capable-host` for lack of a hello. One caveat: a host that said hello only to a release
before this one has no row until it says hello to this release, and until then a submit to the new
server that only that host could serve is refused `no-capable-host`. A build client that keeps its
connection to the old server submits there until the old one's `--drain-hold` ends, and by then
the hosts are on the new one.
