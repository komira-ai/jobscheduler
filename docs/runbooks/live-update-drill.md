# Live update drill

What it proves: an update of js-server while a real build runs on the farm loses nothing. The
server runs as one Swarm service with a start-first update, so for a while two instances share
the Postgres store: the new one starts and turns ready, then the old one gets SIGTERM. On that
signal the old one answers `/readyz` 503, closes the listener of the agent routes, answers each
held heartbeat at once and places no more jobs; each agent's next heartbeat reaches the new
instance, which has it say hello, and the agent keeps its runs. The drill checks the outcome
from outside: no job ends lost, failed or failed to start while the update runs, and every host
that took jobs before takes jobs after.

It is a held step: the owner runs it, on the Swarm manager node, with the script
`tools/held/live-update-drill/live_update_drill.sh`. It is a dry run unless given `--update`.

## Before

- The release that carries the drain (the pull request that added this runbook) is the one the
  service runs, and the next release is published, so the forced update starts a server of the
  same code.
- The service's update is `order: start-first` with `failure_action: rollback`, its health check
  reads `/readyz`, and its `stop_grace_period` is longer than `--drain-hold` plus `--drain-timeout`
  plus one tick (40 s with the defaults), so the engine's SIGKILL never cuts the drain
  ([release.md](../release.md), "Stopping").
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

   It prints how many hosts take jobs, how many jobs run, and the update it would make. A run
   that finds no running job means the build has not reached the farm yet.
3. Run the drill:

   ```sh
   tools/held/live-update-drill/live_update_drill.sh --server https://<job service> \
     --token-file <client token file> --out-dir <workspace scratch directory> --update
   ```

   It runs `docker service update --force --detach=false js-server_server`, reads the hosts while
   the update runs, gives them `--settle-samples` readings (30, two seconds apart) to take jobs
   again, and reads the jobs that ended lost, failed or failed to start since the update began.
4. Let the build finish and read its own summary: the drill passes only with zero failed actions
   in the build as well as the script's `PASS`.

## Reading the verdict

- `PASS`: every host that took jobs before takes jobs again, and no job ended badly. With the
  build's zero failed actions, the drill passed.
- `FAIL: the update ended with status ...`: the update did not converge (the new server did not
  turn ready, and Swarm rolled back). `update.log` in the out-dir has Docker's output.
- `FAIL: hosts that took jobs before the update and do not now`: the named hosts did not come
  back within the settle readings. `hosts-during-<n>.txt` shows when each was last seen.
- `FAIL: jobs that ended lost, failed or failed to start during the update`: each is named with
  its state; `js job get --job-id <id>` gives its events.

Every reading is kept in the out-dir: `hosts-before.txt`, `hosts-during-<n>.txt`,
`hosts-after.txt`, `hosts-missing.txt`, `update.log` and `jobs-ended-badly.txt`.

## What it does not cover

A job submitted to the new server before any host has said hello to it is refused
`no-capable-host`: admission reads the hosts of the server that is asked, and the new server
knows no host until the agents move (the old one closes their listener at its SIGTERM, before it
stops the job service). A build client that keeps its connection to the old server submits there
until the old one's `--drain-hold` ends, and by then the hosts are on the new one. Admission over
every server's hosts is gap G5 of the multi-instance design, and is not built yet.
