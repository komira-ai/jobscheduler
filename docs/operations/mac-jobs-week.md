# A week of jobs on the four Macs

How routine are build jobs on the job service's four Macs? This page holds the counts for the
seven days from 2026-11-19 23:05Z to 2026-11-26 23:05Z: jobs per Mac, how they ended, the
reasons for the failures, the taints and the queue time, read from the job service. Each failure
class has an issue. The Macs are called Mac 1 to Mac 4 here. It is a record of that week, not
kept up to date.

## In short

- All four Macs are in the pool: each is listed live, registered, with no taint, `os=macos`,
  `arch=arm64`, `exec=sandbox-exec` and agent 0.1.422, and 28 or 32 cores and 96 GiB.
- They ended 60,316 job bookings in the week, nearly all from 2026-11-23 on (10 before).
- 986 of those jobs failed: 603 ran and failed, 383 failed to start. That is 1.6% of the week.
  The other 59,330 succeeded; 3,079 of these were read as succeeded and 56,251 are inferred
  (see [What a count measures](#what-a-count-measures)).
- 573 of the 603 runs that failed are one class: Python test and cp314/cp314t wheel actions,
  which failed every attempt that was read and were never read as succeeded on a Mac
  (komira-ai/komira#2415). Without it, 0.7% of the week's jobs
  failed (413).
- The median job waited 5.1 s in the queue; one in ten waited more than 57 s.

## Per Mac

| | Ended | Succeeded (read) | Succeeded (inferred) | Failed | Failed to start | Queue p50 | p90 | p99 | Max |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| Mac 1 | 15,288 | 698 | 14,445 | 71 | 74 | 5.3 s | 104 s | 572 s | 722 s |
| Mac 2 | 15,339 | 857 | 14,179 | 200 | 103 | 5.2 s | 47 s | 299 s | 551 s |
| Mac 3 | 14,200 | 775 | 13,185 | 141 | 99 | 5.1 s | 53 s | 198 s | 551 s |
| Mac 4 | 15,489 | 749 | 14,442 | 191 | 107 | 5.0 s | 47 s | 168 s | 553 s |
| All | 60,316 | 3,079 | 56,251 | 603 | 383 | 5.1 s | 57 s | 363 s | 722 s |

The queue time is the booking's `queueWaitMs`, from queued to placed, over every ended booking
on the host, failures included. Percentiles are nearest-rank.

Jobs ended per day (UTC):

| Day | Mac 1 | Mac 2 | Mac 3 | Mac 4 | All |
|---|---:|---:|---:|---:|---:|
| 2026-11-21 | 10 | 0 | 0 | 0 | 10 |
| 2026-11-23 | 2,515 | 1,360 | 1,354 | 1,445 | 6,674 |
| 2026-11-24 | 7,937 | 6,025 | 7,298 | 6,375 | 27,635 |
| 2026-11-25 | 622 | 2,457 | 2,193 | 2,186 | 7,458 |
| 2026-11-26 | 4,204 | 5,497 | 3,355 | 5,483 | 18,539 |

No job ended on a Mac on 2026-11-20 or 2026-11-22.

## Failures by reason

| Class | Jobs | Mac 1 | Mac 2 | Mac 3 | Mac 4 | When | Issue |
|---|---:|---:|---:|---:|---:|---|---|
| Failed: a Python test or cp314/cp314t wheel action | 573 | 60 | 200 | 130 | 183 | 2026-11-25 23:41Z and 2026-11-26 08:54Z, a minute each | komira-ai/komira#2415 |
| Failed: other build actions and bring-up jobs | 30 | 11 | 0 | 11 | 8 | spread over 2026-11-21 to 2026-11-26 | none yet: exit reasons not read |
| Failed to start: `input-unavailable` | 321 | 71 | 76 | 70 | 104 | 2026-11-24 mostly, 2026-11-26 01Z | komira-ai/jobscheduler#1211 |
| Failed to start: refused `bundle-pending` | 59 | 2 | 27 | 27 | 3 | 2026-11-23 18Z | fixed by komira-ai/jobscheduler#1052 |
| Failed to start: refused `host-tainted` | 2 | 0 | 0 | 2 | 0 | 2026-11-26 11Z | komira-ai/jobscheduler#1212 |
| Failed to start: refused `runtime-failed` | 1 | 1 | 0 | 0 | 0 | 2026-11-21 17Z, before the Macs took build work | none |
| Lost | 0 | | | | | | |
| Cancelled | 0 | | | | | | |

**Python tests and wheels (573).** The 97 targets are tests under `komira//python/tests` (for
Python 3.11, 3.12 and 3.14) and cp314 and cp314t wheels under `komira//third_party/python`.
Each failed every attempt it had, three per build; 94 of them were sent by both builds. None of
these targets succeeded on a Mac in the part of the succeeded jobs that was read. 265 of the
runs ended within 10 ms of starting (the job's `endedAtMs` less its `startedAtMs`). 18 of the 573
runs, of three `ops_udf_test_*` targets, are also komira-ai/komira#2288. Whether any of the
56,251 inferred successes are of these targets is not known: those jobs were not read.

**Other failed runs (30).** 3 are bring-up jobs from 2026-11-21. 23 are build actions that name
no build target (no `historyKey`), which ran from 2 s to 115 s. 3 are the three attempts of one
`pyarrow-cp314` action, each ended within 4 ms on 2026-11-26 00:58Z, and 1 is a
`parquet_footers` action. Their exit codes and stderr were not read, so they are not
grouped further and have no issue yet.

**`input-unavailable` (321).** The host could not read an input the job named (a tierstore layer
or mount) and the job failed to start. The median time from placement to that end (the job's
`endedAtMs` less its `placedAtMs`) is 120.5 s. The job service does not send such a job back to the queue (its requeue covers
`host-held`, `no-capacity` and `bundle-pending` only), although the code is documented as the
host's state. A job with a retry policy would be retried; none of these jobs set one, and each
had one attempt. The same holds for the two `host-tainted` refusals.

**`bundle-pending` (59).** All in the hour from 2026-11-23 18Z, before komira-ai/jobscheduler#1052
(merged 2026-11-23 21:06Z) made this refusal requeue the job. None after.

## Taints

Two starts were refused `host-tainted` on Mac 3 in the hour from 2026-11-26 11Z, so Mac 3 was
tainted at least once in the week. No Mac was tainted when the host list was read at 23:05Z.
The host list shows only the current taint, so these two refusals are the only record of a taint
in the week; a taint that refused no start would not appear here.

## What a count measures

Every number here comes from three read-only listings of the job service, read at
2026-11-26 23:05Z:

- `js --json host list`: the hosts, their state, capabilities, agent version and taint.
- `js --json usage --from-ms <start> --to-ms <end> --host-id <host>`, every page, for each Mac:
  one record per ended booking (a job's resources on one host), selected by when the booking
  ended. A job's queue wait, run time and host are from it. No job had more than one booking.
- `js --json job list --states <state>`, every page, for `failed`, `failed-to-start`, `lost`,
  `cancelled` and `succeeded`: the job's end state, start failure code and detail, times and
  build target (`historyKey`).

A job's outcome is its state in the job list, joined by job id, because the usage record does
not hold it (komira-ai/jobscheduler#1213). The `failed`, `failed-to-start`, `lost` and
`cancelled` listings were read to their last page. The `succeeded` listing was not: the job
service stopped answering at page 3,905, after 390,400 jobs. The listing is in job-id order (as
text), not by time, and every one of the 56,251 Mac jobs found in no listing has an id after the
last id read, while every Mac job with an id before it was found in some listing. Those 56,251
are counted as succeeded. That is inferred, not read: each has an ended booking and is in none of
the complete listings of the other end states, so a succeeded state is the only one left.

The failure reasons are the start failure code and detail for a job that failed to start, and,
for a job that ran and failed, its build target, its run time and when it ended. A run's exit
code and stderr (`js job result`, `js job logs`) were not read.
