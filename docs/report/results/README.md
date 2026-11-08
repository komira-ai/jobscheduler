# Results of the report's measurements

This directory is where the measured results behind [the report](../jobscheduler.html) go. It holds none yet: every cell of the report's section "What we have measured, and what not yet" is open, and a cell is filled only from a result produced as this page says.

A result is one run of one plan of [`crates/js-measure/plans`](../../../crates/js-measure/plans) against a lease server, and the analysis of its records by `js-analysis`. The harness and the record format are documented in [`crates/js-measure/src/lib.rs`](../../../crates/js-measure/src/lib.rs); the statistics in [`docs/design/bench-analysis.md`](../../design/bench-analysis.md).

## The plans

Each plan is a pair of files, `<plan>.plan.json` (the parameter, its settings, the arms, the runs per arm, the warm-ups and the seed of the order) and `<plan>.workload.json` (the jobs of one run). Every workload names its image as a `REPLACE-WITH-...` placeholder pinned by digest: no plan is runnable as shipped, and the image has to be built, pushed and named, with its digest, before a live run. The image needs a `sleep` on its path; the job runs `sleep N` and nothing else.

| Plan | What it measures | Arms | Baseline | Metrics |
|---|---|---|---|---|
| `job-launch` | The launch of one small job on an idle host, as a client sees it: the lease request to the lease held, and the job's planned submit to the first poll that sees its run running. It is not the agent's own start-to-process time of design step 18. | `shared` (the `action` class the workload names) and `whole-host` (every job asks for `whole-node`); 30 runs each after 2 warm-ups, which take the image pull | `shared` | `request_to_token_ms_mean`, `request_to_token_ms_max`, `queue_wait_ms_mean`, `queue_wait_ms_max` |
| `queue-wait-under-load` | Queue wait and makespan of 1000 jobs of 2 cores and 4 GiB, one submitted every 100 ms, each holding its host for 60 s, on the pool as it is (design section 8 row 21) | `load-placement` alone; 10 runs after 1 warm-up | none (one arm) | `jobs`, `jobs_failed`, `makespan_ms`, `queue_wait_ms_mean`, `queue_wait_ms_max` |
| `whole-host-vs-backfill` | The wait of one large job submitted 60 s into a steady stream of 300 small jobs (4 cores, 16 GiB, one every 2 s, 120 s each), when the large job is a whole-host request (it marks a host, and small jobs use the host meanwhile only if they end inside its estimated time left) against a share of the same size (no mark, so the small jobs fill in ahead of it; design section 8 row 17) | `backfill` (every job asks for `action`, so the large job is a share) and `whole-host` (the workload's own classes: the large job `whole-node`, the small ones `action`); 20 runs each after 1 warm-up | `backfill` | `queue_wait_ms_max`, `queue_wait_ms_mean`, `makespan_ms`, `host_cpu_share_mean`, `jobs_failed` |

`whole-host-vs-backfill` asks for the large job's size as 64 cores and 256 GiB, the shape of the simulated farm's hosts. Before a live run, set its `cpu_millis` and `memory_bytes` to the schedulable size of one host of the pool (the host's shape under `memory_schedulable_percent`), so that as a share it fits on an empty host and nowhere else. The large job's own wait is its `queue_wait_ms` in `jobs.jsonl` (job `large-0`); `queue_wait_ms_max` of the record is the largest wait of any job of the run.

`queue_wait_ms` is good to two poll intervals (one at each end), so a launch is measured with a short `--poll-ms`; `job-launch` is run with `--poll-ms 50`.

## Producing a result

A result directory is `docs/report/results/<plan>/<commit>/`, where `<commit>` is the commit of the server under test. It is made in two steps.

1. Run the plan. `js-measure run` appends to three JSON-lines files in `--out` and refuses a directory that already holds records:

   ```text
   js-measure run --plan crates/js-measure/plans/<plan>.plan.json \
       --workload crates/js-measure/plans/<plan>.workload.json \
       --out <dir> --commit <commit> --poll-ms <ms> \
       --live --server <url> --token-file <file>
   ```

   Without `--live` the same command runs on a simulated farm of `--fake-hosts` hosts on a fake clock. That checks a plan end to end and measures nothing: a result is never produced without `--live`.

2. Analyse the records. `js-analysis` prints the report as markdown and writes it as JSON:

   ```text
   js-analysis <dir>/records.jsonl --baseline <baseline> --json <dir>/analysis.json > <dir>/analysis.md
   ```

   `<baseline>` is the plan's baseline arm from the table above; a plan of one arm takes no `--baseline`.

A result directory holds `records.jsonl`, `analysis.md` and `analysis.json`. It holds `jobs.jsonl` and `minutes.jsonl` only when their `host` column names no hostname or address: the column holds the server's host ids, and a farm whose host ids are hostnames keeps those two files out of the repository. A report cell names the result directory its number comes from.

## The columns

A metric that the run could not read is `null`, never `0`: the analysis leaves it out of that metric's statistics and counts it as missing.

### `records.jsonl`

One line per run of the plan, warm-ups included; this is the input of `js-analysis`.

| Column | Meaning |
|---|---|
| `setting` | The setting of the plan's parameter the run was at, as the plan's text. |
| `arm` | The arm's label. |
| `run_index` | The run's place in the executed order of the whole plan, from 0. |
| `seed` | The plan's seed, from which the order was drawn. |
| `commit` | The commit given to `--commit`. |
| `label` | `warmup` on a warm-up run, which the analysis lists and leaves out of every statistic; empty otherwise. |
| `metrics` | The run's numbers, by name; the names are below. |

The metrics of a record:

| Metric | Meaning |
|---|---|
| `jobs` | Jobs of the workload. |
| `jobs_failed` | Jobs that did not end `succeeded`, or never ran. |
| `memory_kills` | Jobs that ended `out_of_memory`. |
| `makespan_ms` | First lease request to the last poll that saw a run over; `null` when any job has no finish. |
| `queue_wait_ms_mean` | Mean over the jobs of the planned submit time to the first poll that saw the job's run running. |
| `queue_wait_ms_max` | Largest of the same. |
| `request_to_token_ms_mean` | Mean over the jobs of the lease request to the lease seen held. |
| `request_to_token_ms_max` | Largest of the same. |
| `host_cpu_share_mean` | Mean over the host-minutes of the run of a host's CPU in use over its CPU shape. |
| `host_cpu_share_max` | Largest of the same. |
| `host_ram_share_mean` | Mean over the host-minutes of a host's working set over its memory shape. |
| `host_ram_share_max` | Largest of the same. |
| `runtime_actual_over_predicted_mean` | Mean over the jobs of the observed run time over the run time the server's curve for the job's profile predicts. |
| `memory_actual_over_predicted_mean` | Mean over the jobs of the peak working set over the peak the curve predicts. |
| `cpu_used_over_booked` | CPU used over CPU booked, summed over the jobs that have both. |
| `memory_used_over_booked` | Peak working set over memory booked, summed over the jobs that have a peak. |
| `reports_dropped` | Jobs that did not succeed but printed a result line. |
| `reports_missing` | Jobs that succeeded and printed no result line. |
| `reported_<key>` | A number the run's jobs printed on a `js-measure-result` line; none of these plans' jobs prints one. |

### `jobs.jsonl`

One line per job of each run.

| Column | Meaning |
|---|---|
| `setting`, `arm`, `run_index` | The run the job belongs to, as in `records.jsonl`. |
| `job` | The job's name, `<kind>-<index>` from the workload. |
| `host` | The host id the job's lease held; `null` when it held none. |
| `outcome` | How the job's run ended, as the server says (`succeeded`, `out_of_memory`, ...); `null` when it did not end. |
| `failure` | Why the job did not get through (`deadline`, a refusal of the server); `null` when nothing went wrong. |
| `metrics` | The job's numbers, by name; the names are below. |

The metrics of a job:

| Metric | Meaning |
|---|---|
| `queue_wait_ms` | The planned submit time to the first poll that saw the run running. |
| `request_to_token_ms` | The lease request to the lease seen held. |
| `runtime_ms` | The first poll that saw the run running to the poll that saw it over. |
| `predicted_runtime_ms` | The run time the curve of the job's profile predicts. |
| `runtime_actual_over_predicted` | `runtime_ms` over `predicted_runtime_ms`. |
| `peak_ws_bytes` | The largest working set of the host over the lease's window, when the lease was alone on the host. |
| `predicted_peak_ws_bytes` | The peak the curve predicts. |
| `memory_actual_over_predicted` | `peak_ws_bytes` over `predicted_peak_ws_bytes`. |
| `booked_cpu_milli_ms` | CPU booked over the held time, in thousandths of a core times milliseconds. |
| `used_cpu_milli_ms` | CPU the host used over the held time, same unit, when the lease was alone on the host. |
| `booked_memory_bytes` | Memory booked. |

### `minutes.jsonl`

One line per minute of each host a run used, over the run's window.

| Column | Meaning |
|---|---|
| `setting`, `arm`, `run_index` | The run, as in `records.jsonl`. |
| `host` | The host id. |
| `minute` | Minutes since the epoch, as the server's usage store buckets them. |
| `cpu_share` | The minute's mean CPU in use over the host's CPU shape. |
| `ram_share` | The minute's largest working set over the host's memory shape. |
| `leases` | Leases that ran on the host in the minute. |

### `analysis.md` and `analysis.json`

The report of `js-analysis`: per setting, arm and metric the measured runs with a value, the missing ones, the warm-ups, the mean and its 95% interval, the median, the standard deviation, the range, std/mean and its 95% interval; the comparison of each arm with the baseline (the 95% Welch interval of the difference of means, the relative change and its interval, a permutation p-value, a verdict, and whether it is underpowered); the runs per arm that resolve a 5% and a 10% effect; and, last, every run. The fields are the types of [`crates/js-analysis/src/types.rs`](../../../crates/js-analysis/src/types.rs).

## Checking the plans

`cargo test -p js-measure --test suite plans::` checks that every plan validates, that its workload expands to the jobs this page describes, that this page names every column the harness writes, and that a run of each plan on the simulated farm, cut to 3 runs per arm after 1 warm-up, analyses to the golden in [`crates/js-measure/tests/golden`](../../../crates/js-measure/tests/golden). The goldens are simulated runs, not measurements.
