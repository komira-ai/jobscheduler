# Roll out a SubmitJob change and measure the submit round trip

This runbook rolls out, to the control plane's js-server, the release that contains a change to the
`SubmitJob` path, and measures the submit round trip before and after it, the way the build
service's design page measures it: the `rpc` step of the frontends' `kbf_executor_step_seconds`
histogram, at the p50 and the p90. It was written for komira-ai/jobscheduler#1092 (one read of the
job id, no separate clock read, no wait behind a pass's re-read of the job tables, queued rows read
500 per statement; 11 round trips to the store per submit become 8), and works for any change that
ships in js-server.

Two scripts, both in `tools/held/submit-rollout/`, both run by the owner on the Swarm node that runs
the js-server task and the frontends:

| Script | Changes anything | What it does |
|---|---|---|
| `submit_rollout.sh` | only with `--apply` | Checks that the newest release contains `--commit` and runs; when that release's control-plane deploy failed, `--apply` re-runs the run's failed jobs. |
| `submit_rpc.sh` | never | Reads the frontends' `rpc` histogram twice, a window apart, and prints the count, rate, p50, p90 and mean of the submits in between. |

## What the rollout is

A release reaches js-server through its own release run: `cp-deploy` deploys the release's image to
the Swarm service and passes when the deploy converged (`docs/release.md`, "The control-plane
deploy"). So a merged change is normally rolled out with no step from anyone, and
`submit_rollout.sh` checks that it was. Its only action is the documented recovery of a failed
deploy, "Re-run failed jobs" of that run (`docs/release.md`, "Recovery"), and it takes it only for
the newest release: re-running an older run's deploy would move the service back. It never deploys
an image of its own.

A change to the server alone (as #1092 is) does not touch the agents, so no js-agent roll is
needed. It does not change the order the frontends keep, either: blobs, then the tree, then
`SubmitJob`. The `rpc` step starts after the tree is stored, so the change shows in it alone.

## Steps

1. **Before.** While the older version still runs, take a 30-minute window:

   ```sh
   tools/held/submit-rollout/submit_rpc.sh measure --out-dir <scratch dir>/before
   ```

   A reading can also be taken from another machine, streaming the script to the node; nothing is
   written there:

   ```sh
   ssh <node> 'bash -s -- snapshot -' < tools/held/submit-rollout/submit_rpc.sh > first.txt
   # ... the window ...
   ssh <node> 'bash -s -- snapshot -' < tools/held/submit-rollout/submit_rpc.sh > second.txt
   tools/held/submit-rollout/submit_rpc.sh compute first.txt second.txt
   ```

2. **Roll out.** A dry run first:

   ```sh
   tools/held/submit-rollout/submit_rollout.sh --commit <full sha of the change> --out-dir <scratch dir>/rollout
   ```

   | It prints | Exit | What to do |
   |---|---|---|
   | `ROLLED OUT: ...` | 0 | Nothing: the service runs a release with the change. |
   | `no release contains ... yet` | 3 | Wait for the release run of the merge (or of a later commit), then run it again. |
   | `the control-plane deploy of run N has not ended` | 3 | Wait for the run's deploy, then run it again. |
   | `rollout: gh run rerun N ...` and `dry run: ...` | 0 | The deploy failed. Run it again with `--apply`, then once more when the jobs ended. |
   | any other refusal | 1 | Read what it names (the service, the run) before anything else. It acts on none of these. |

3. **After.** Once the dry run says `ROLLED OUT`, take a 30-minute window the same way, into
   `<scratch dir>/after`. Its first reading must come after the new server started: `compute` refuses
   a window over which the js-server container changed (exit 1), as such a window measures two
   servers.

4. **Report**, with the template below.

## What the numbers prove, and what they do not

They prove how long kbf's frontends waited for `SubmitJob`'s answer, on each server version, in each
window, and how the two compare.

They do not prove:

- **Why.** The `rpc` step holds the frontend's call, the network, the server's wait for a place on
  its blocking pool and for a store connection, the read of the job id, the store transaction and
  the note to the placer. No server-side metric splits it until the submit phase histogram
  (komira-ai/jobscheduler#1109) ships. A drop of tens of milliseconds or more cannot come from the
  three round trips to the store #1092 removes (0.1-0.6 ms on a local socket); it points to the
  wait behind a pass's re-read of the job tables, which #1092 also removes.
- **The same load.** Nobody controls the farm's traffic. Two windows differ in their submit rate
  (the `count` and `/s` of each line) and in their mix of builds. A difference is a fair claim when
  the rates are alike; otherwise take more windows, or report it as a difference under different
  load. The design asks for 30 minutes of light load and 30 of burst; the script measures the load
  the window happens to have.
- **More than the buckets show.** The quantiles are interpolated inside the histogram's buckets
  (1, 2.5, 5, 10, 25, 50, 100, 250, 500 ms, 1, 2.5, 5, 10 s, ...), so a p50 of 464 ms means somewhere
  in 250-500 ms, near the top. A quantile past the last bucket is printed as `>` its bound.
- **The other items' measures.** The design measures items 4, 5 and 6 by the server's own phases
  (`insert_queued`, the pool checkout, the transaction) and `js_job_pass_duration_seconds`. Those
  need the phase histogram and the server's metrics port; this runbook reads neither.
- **Frontends that restarted.** A frontend whose counts went down during a window, or that is in
  one reading only, is left out and named in the `frontends:` line; its submits are not counted.

## Report template

```text
SubmitJob round trip (kbf rpc step) before and after <change>

Change:     <PR>, merged as <sha>; in release <vX.Y.Z>
Rollout:    submit_rollout.sh said <ROLLED OUT ... | re-ran run N at <time>>; js-server <old> -> <new>,
            container started <time>
Measure:    kbf_executor_step_seconds{step="rpc"}, every frontend, difference of two readings
            (submit_rpc.sh compute); p50/p90 interpolated inside the buckets

            window (UTC)        server  frontends  submits (rate)  p50     p90     mean
Before:     <from> - <to>       <old>   <n kept>   <count> (<r>/s) <p50>   <p90>   <mean>
After:      <from> - <to>       <new>   <n kept>   <count> (<r>/s) <p50>   <p90>   <mean>

Load:       <alike | after was lighter/heavier by ...>; <light | burst> in each window
Says:       <the difference, and whether the rates allow the claim>
Does not:   why (no server phase split); other items' measures; <anything dropped>
Raw:        <where the four readings are kept>
```
