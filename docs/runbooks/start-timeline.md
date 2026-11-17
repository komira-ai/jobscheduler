# Reading a job's start timeline

Every container the agent starts is logged with the steps of its start: where the time from the
start's arrival to its container went. The line is the `Started` line the agent already printed,
with the timeline added:

```text
js-agent: job: Started { run_id: "<run>", timeline: StartTimeline { received_ms: R,
  steps: [("taken", a), ("admitted", b), ("ready", c), ("dirs", d), ("created", e)],
  runtime: [("begin", W1), ("recorded", W2), ("image", W3), ("dirs", W4), ("cgroup", W5),
  ("bundle", W6), ("supervised", W7), ("running", W8)] } }
```

`R` and the lowercase letters are milliseconds on the agent's clock, the `W`s wall clock times
(below). On a node the line is in the agent's
journal: `journalctl -u <the agent's unit> | grep 'job: Started'`.

## The lifecycle's steps (`steps`)

Each is the milliseconds after `received_ms`, the moment the heartbeat reply that carried the start
arrived, on the agent's own monotonic clock (milliseconds since the agent started). In order:

| Step | Done when |
|---|---|
| `taken` | the worker began on the start (the time before it is the reply waiting in the worker's queue) |
| `admitted` | its admission passed (its body, network ceiling, host ports and mounts, and the firewall for a wired job) and its slot is booked |
| `resolved` | a start with layers or mounts only: its images are on the node and its host paths were found |
| `dirs` | its scratch directory, and the runtime's directory of the lease beside it, are made and under the run's quota |
| `ready` | everything it waited for is here: its images, its providers' answers, its assembled file system |
| `created` | the runtime's create returned |

A start whose image is still being pulled, or whose providers or file system are being prepared,
waits between `admitted` and `ready` (and `resolved`). The work it waits on wakes the agent's worker
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
