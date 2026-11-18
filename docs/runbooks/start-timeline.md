# Reading a job's start timeline

Every container the agent starts is logged with the steps of its start: where the time from the
start's arrival to its container went. The line is the `Started` line the agent already printed,
with the timeline added:

```text
js-agent: job: Started { run_id: "<run>", timeline: StartTimeline { received_ms: R,
  placed_at_ms: Some(P), received_at_ms: Some(A),
  steps: [("taken", a), ("admitted", b), ("ready", c), ("dirs", d), ("created", e)],
  runtime: [("begin", W1), ("recorded", W2), ("image", W3), ("dirs", W4), ("cgroup", W5),
  ("bundle", W6), ("supervised", W7), ("running", W8)] } }
```

`R` and the lowercase letters are milliseconds on the agent's clock, `P`, `A` and the `W`s wall clock
times (below). On a node the line is in the agent's
journal: `journalctl -u <the agent's unit> | grep 'job: Started'`.

## Placement and arrival (`placed_at_ms`, `received_at_ms`)

Both are milliseconds since the Unix epoch.

- `placed_at_ms` (`P`) is the server's time at which it placed the job, as its start carried it
  (`placed_at_ms` of the start). It is `None` for a run start and when the server sent none.
- `received_at_ms` (`A`) is when the heartbeat reply that carried the start arrived, on the node's
  wall clock: the wall clock at the start's admission less the time since `received_ms`. It is
  `None` when the agent's clock has no wall clock time.

`A - P` is the time from placement to arrival. It spans the server's clock and the node's, so the
skew between the two hosts is in it, and it can come out negative. `W8 - A` (the runtime's `running`
step minus `received_at_ms`) is the time from arrival to running, on the node's wall clock alone.

## The lifecycle's steps (`steps`)

Each is the milliseconds after `received_ms`, the moment the heartbeat reply that carried the start
arrived, on the agent's own monotonic clock (milliseconds since the agent started). A start with
neither layers nor mounts logs `taken`, `admitted`, `ready`, `dirs`, `created`. A start with layers
or mounts runs its waits side by side: at its admission its providers are asked, the runtime
begins the lookups of the host paths that wait for no provider, and its directories are made. It
logs `taken`, `admitted`, `dirs`, `provided` (only when it has providers), `resolved`, `ready`,
`created`.

| Step | Done when |
|---|---|
| `taken` | the worker began on the start (the time before it is the reply waiting in the worker's queue) |
| `admitted` | its admission passed (its body, network ceiling, host ports and mounts, and the firewall for a wired job) and its slot is booked |
| `dirs` | its scratch directory, and the runtime's directory of the lease beside it, are made and under the run's quota (a start with layers or mounts: at its admission) |
| `provided` | a start with providers only: the last directory a provider answered for its sources is in (its time after `admitted` is the providers' wait; on the tier store that is the tree attach, `Mount`) |
| `resolved` | a start with layers or mounts only: its providers have answered, its images are on the node and its host paths, the providers' directories among them, were found |
| `ready` | everything it waited for is here (its images, its providers' answers, its assembled file system), and the agent goes on to make what is left and create the container |
| `created` | the runtime's create returned |

A start whose image is still being pulled, or whose providers or file system are being prepared,
waits between `admitted` and `ready` (`provided` and `resolved` fall in between). The work it waits on wakes the agent's worker
when it is done, so the wait is the work's own time, not the worker's step interval.

## The runtime's steps (`runtime`)

The `crun` runtime gives the steps of its create, each the wall clock time (milliseconds since the
Unix epoch) it was done at; another runtime gives none. In order: `begin` (the lease is named and
labelled), `recorded` (its sidecar is written and synced), `image` (its image is pinned, made sure
of, and its config read), `dirs` (its directories made and given to the job's users), `cgroup` (its
cgroup made, limited and given to the job user), `bundle` (its bundle written), `supervised` (the
supervisor made its files and said it is ready) and `running` (its sidecar says it runs and its
network is wired).

## What the line does not hold

`crun run` starts after `supervised`: it makes the container's namespaces and mounts (the overlay
over the image's tree among them), loads the seccomp filter, joins the lease's cgroup and starts the
container's init, which starts the job's command. None of that reports back to the agent, so the
command's exec is not on the line. To measure it on a node, run a job whose command prints the wall
clock as its first act (for example `date +%s%3N` in an image that has `date`) and compare:

- `crun run` and the init: the printed time minus the `supervised` time of the same run;
- arrival to exec: `created` plus the printed time minus the `running` time (the two clocks differ
  in origin, and `running` is the moment the create returned, give or take the clock's millisecond).

## The percentiles of many starts (`js-start-timeline`)

`js-start-timeline` reads the `Started` lines of the agent's journal and prints, for every segment
of a start, how many starts had it and its p50, p90, p99 and maximum in milliseconds. It reads the
files named on its command line, or standard input when it is given none. A line that is not a
`Started` line is skipped, whatever prefix the journal put before it. A `Started` line that does
not parse is counted in the header as malformed and left out of the table.

To collect, run it on each node over the window of the measurement, or on the concatenation of the
nodes' lines:

```text
journalctl -u js-agent-crun --since '2026-11-18 14:00' --until '2026-11-18 14:30' -o short-iso \
  | js-start-timeline
```

Each row is one segment, in the order the segments first appear in the input:

| Row | Value |
|---|---|
| `placed->received` | the agent's `received_at_ms` minus the server's `placed_at_ms`; the two are on two hosts' clocks, so a skew between the hosts is in it |
| `received->X` | the lifecycle step `X`, the time after the reply arrived (the cumulative time) |
| `A->B` (lifecycle) | the time between two consecutive lifecycle steps of the line |
| `A->B` (runtime) | the time between two consecutive runtime steps of the line, one agent's clock |
| `received->running` | the runtime's `running` step minus `received_at_ms`, one agent's clock |
| `placed->running` | the runtime's `running` step minus `placed_at_ms`, two hosts' clocks |

The percentile is the nearest-rank one: the p-th percentile of `n` samples is the sample at rank
`ceil(p / 100 * n)` of the sorted samples, counting from 1; so the p99 of fewer than 100 starts is
the largest. A difference keeps its sign, so a negative value in a row that spans two hosts shows
the clocks disagree by at least that much. The lines of an older server have no `placed_at_ms`, and
those of an older agent have neither it nor `received_at_ms`; they give the rows that do not need the
missing field, and the count column says how many starts each row has. A row's count is lower than
the number of starts when some starts do not have the step (`provided` and `resolved` only appear in
starts with layers or mounts).
