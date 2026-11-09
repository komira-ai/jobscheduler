# The estimate feed: what it measures and when to turn it on

**Status: the feed is gone.** `--estimate-feed` and the file key `server.estimate_feed` are still accepted, with the refusals below (beside `--usage-off`, without an agent pool), but they do nothing: the server logs `--estimate-feed is deprecated and does nothing: no lease is costed by an estimate any more (drop the flag)`. The lease table, the registry's runs, the profile lives (`lives.rs`, `history.rs`) and the tail counters that this note describes as *Built* are deleted, so no lease is costed by an estimate, backfill is given no estimated end, and no stop is counted as estimated or not. What remains is the usage store's ended row of each freed job allocation (section 2). The sections below record what the feed did when it existed and why the owner was asked to decide on it; read their *Built* statements as describing the release before the lease table was deleted.

Design section 13, question 3, asks: is the estimate feed on by default, and what evidence in the tail counters would justify it? Step 10 of section 12.1 asks for that decision. This note is what the owner decides from. It changes no code, flag or default.

Each statement is one of three kinds, and the note keeps them apart:

- **Built**: what the code on main does, with the file that does it.
- **Derived**: arithmetic from the built rules, with the working shown.
- **Proposed**: a choice the owner can accept, reject or change.

The note has no measurement of a running deployment. Every number in it is either a constant copied from the code, a value chosen here (and marked as chosen), or arithmetic on those two.

## 1. What the feed does

*Built.*

**Where an estimate comes from.** `Registry::estimate_of` (`crates/js-registry/src/registry/history.rs`) calls `lives::estimate_end` (`crates/js-registry/src/lives.rs`). A lease's estimated end is its start plus the largest `wall_minutes_p90` among the profile keys of the runs queued on it, converted from minutes to milliseconds. A lease with no keyed run, or whose keys have no P90, has no estimate.

The P90 comes from `fit` (`crates/js-usage/src/fit.rs`). It is the nearest-rank 90th percentile (`p90`: the `ceil(0.9 n)`-th smallest value) of the wall minutes of the key's last runs that ended cleanly. Each run's minutes are rounded up, and the result is at least 1. The ring keeps `RING_RUNS` = 32 runs (`crates/js-usage/src/ring.rs`). There is no fit below `MIN_RUNS_FOR_CURVE` = 3 runs. A key idle for more than `STALE_DAYS` = 60 days gives no P90 (`ProfileCurves::wall_minutes_p90_of`, `crates/js-usage/src/curves.rs`).

The feed reads the raw P90, not the life admission charges a lease (`lives.rs` header). The cost curve fitted from the same ring is admission's and plays no part in the estimated end.

**Where the estimate is used.** Three places use it:

- `js_whole::cost::tail` (`crates/js-whole/src/cost.rs`) works out a host's tail. Each lease's end is its estimate cut to its max-wall bound, else the bound, else unknown (`lease_end`). The tail is the latest of those ends.
- `drain_cost` ranks the hosts a whole-node request could mark. Hosts with a known tail come first, then the smaller wait, then fewer leases, then the lower host id. A mark keeps the tail it was made with (test `a_mark_keeps_the_end_of_the_tail_it_was_made_with`).
- Backfill on the marked host. A start of the pool's share class is shown the marked host with the time left until that tail. It is booked there only if `fits_in_tail(now, duration, tail_end, DEFAULT_BACKFILL_MARGIN_MS)` holds, that is `now + duration + margin <= tail_end`, with `DEFAULT_BACKFILL_MARGIN_MS` = 60 000 ms (`js_sched::backfill::fits`, `crates/js-sched/src/backfill.rs`).

`fits_in_tail` fails closed: unknown work, an unknown tail, or a sum that overflows all mean no. In the lease server, a backfilled start's duration is its max wall time. The server's engine is given no learned curves (`crates/js-sched-placer/src/engine.rs`), and it trusts a wall time only up to `server.max_lease_wall`. So the feed changes the tail that backfill must fit in, not the duration of the start being backfilled.

**What changes when it is off.** `crates/js-lease-server/src/agents.rs` gives the registry its source of profile lives only when the flag is on: `estimate_feed.then(|| registry.set_lives(lives))`. With the flag off:

- No estimate is computed for any lease. Every lease is costed at its bound. A lease whose bound saturates (no max wall) has an unknown tail, and its host ranks after every host with a known tail.
- Backfill still runs inside the latest bound on the marked host.
- A lease put back from the lease file keeps its recorded start and bound only with the flag on. With the flag off it has no bound and counts as ending now (tests `a_restored_lease_keeps_its_recorded_bound_only_with_the_flag`, `without_a_source_of_lives_a_restored_lease_counts_as_ending_now`; design section 8, row 10).
- There is nothing to measure: every stop the counters count is `ended_unestimated` (a restored lease's stop counts nothing), and no ended row has an `estimated_end` (section 2).

**The switch.** The switch is the flag `--estimate-feed` or the file key `server.estimate_feed`, `false` by default (`crates/js-lease-server/src/lib.rs`, tests `the_flag_is_off_by_default_and_needs_an_agent_pool_and_usage` and `the_key_is_read_from_the_file_and_refused_beside_usage_off`). It needs `--agent-pool-config` and is refused with `--usage-off`. Today a server starts with at most one pool that registers agents (`lib.rs`, the `[[pool]]` rules), so the server-wide flag is in effect a per-pool setting.

## 2. What is recorded

*Built.* `Registry::stop` (`crates/js-registry/src/registry/provider_side.rs`) reads the estimate once. The same value goes into the tail counters and into the ended row.

### The tail counters

`TailCounters` (`crates/js-registry/src/tail_counters.rs`, deleted with the lease code) had these fields:

| field | counts |
|---|---|
| `started_on_marked` | leases started on a host that carried a mark, the marked request's own included |
| `backfilled` | of those, the leases that were not the marked request's own |
| `ended_estimated` | leases that ended with an estimate |
| `ended_unestimated` | leases that ended with no estimate |
| `error_sum_ms` | the sum of the errors, in milliseconds (saturating) |
| `error_buckets` | the errors by bucket: 8 buckets for the 7 edges |

An error is the actual end minus the estimated end. A lease that outlives its estimate has a positive error. `ERROR_EDGES_MS` = `[-300_000, -60_000, -10_000, 0, 10_000, 60_000, 300_000]`. Bucket `i` counts the errors up to and including edge `i` and above edge `i - 1`. The last bucket counts the errors above 300 000 ms:

| bucket | 0 | 1 | 2 | 3 | 4 | 5 | 6 | 7 |
|---|---|---|---|---|---|---|---|---|
| error (ms) | ≤ −300 000 | (−300 000, −60 000] | (−60 000, −10 000] | (−10 000, 0] | (0, 10 000] | (10 000, 60 000] | (60 000, 300 000] | > 300 000 |

The file documents two limits. First, the end of a lease is the time `stop` is called, on the registry's clock, not the time the work exited. Second, a lease put back after a restart has no bound and no start, so its stop counts nothing. With the feed on, a restored lease keeps both (section 1), so its stop does count.

**Where they are kept.** In the usage store under `usage/tail/` (the SQLite file of `--usage-db`) when the server has one, so they survive a restart (`docs/usage-store.md`). With `--usage-off`, or without agent registration, they are in memory only and start at zero at every server start. Only `Registry::tail_counters` reads them, and today only tests call it: no route, log line or metric serves them. Rows in Postgres are step 3.

### The ended-allocation row

`EndedRow` (`crates/js-usage/src/history.rs`, described in `docs/usage-store.md`) is one row per allocation. It is stored under `usage/ended/<ended-at>/<id>`, with `<ended-at>` in milliseconds since the Unix epoch, zero-padded to 20 digits. Beside it is the index entry `usage/ended-id/<id>`. Both live in the usage store, the SQLite file `--usage-db` names (`usage.db` beside the lease file by default). They are kept for `--history-retention-days` (file key `history_retention_days`, default 90). The fields are:

| field | holds |
|---|---|
| `id` | the allocation: the lease id's text today (`AllocationId::of_lease`) |
| `profile` | the first profile key a run of the lease was queued with, if any |
| `host` | the host; absent for a whole-node request cancelled before it had one |
| `booked` | `whole_host`, `cpu_milli`, `memory_bytes`; absent with no host |
| `queued_ms` | the queue wait; absent when not known |
| `run_ms` | start to end; absent when the start is not known |
| `ended_reason` | `released`, `expired`, `lost`, `cancelled-while-waiting`, `failed-to-start` or `stopped-at-wall-limit` |
| `drain_outcome` | `none`, `no-mark`, `mark-held`, `mark-dropped` or `unknown` |
| `give_up_outcome` | always 0 for a whole-node request (a drain is never given up); absent for an ordinary lease |
| `estimated_end` | the end the feed gave it, if any |
| `actual_end` | when it ended: the key's time |
| `error_ms` | `actual_end` less `estimated_end`; absent without an estimate |
| `used` | what it was measured to use; absent when nothing was measured |

**Who writes it.** On main the registry writes the row when a lease's booking ends: from `Registry::stop`, and when a whole-node prepare fails (`failed-to-start`). When the engine owns job allocations (design step 8), the engine's freeing function will write the same row, under the same key and with the same fields, keyed by the job id. That move is not on main.

**The row against the counters.** For a stop, the row's `error_ms` is the error the counters bucket: the same estimate, the same end and the same clamp. There are differences:

- A row is also written for a failed prepare and for a cancelled waiting request. The counters count neither.
- A row is written once per allocation.
- The row keeps each error exactly, not as a bucket.

So every bucket count, and every threshold in section 4, can be worked out from the rows. The rows are also stored and kept for a window. The evidence has to come from them.

**How an operator reads them.** Nothing serves them to an operator today:

- `UsageSink::ended_rows(from_ms, to_ms, limit)` (`crates/js-usage/src/sink.rs`) is the only reader, and only code calls it. `docs/usage-store.md` says: "No route serves them yet."
- `GetUsage` (`GET /v1/usage`, with `from_ms`, `to_ms`, `host_id`, `job_id`, `page_size`, `page_token`) is declared in `crates/js-proto/proto/jobscheduler/v1/service.proto` and routed by `js-api-gateway`. No server implements it, and its `UsageRecord` carries `estimated_run_ms`, not the estimated end or the error.
- The `js` command line has no usage command.

The rows can be read from the SQLite file with `EndedHistory::range`, but the stored value is a versioned binary layout that needs that decoder.

## 3. Which error matters

*Derived* from the rules of section 1.

**The setting.** A whole-node request marks host H at time m, with tail T. Backfill is booked on H only while `now + wall + 60 s <= T`. The lease table ends each backfilled lease at its wall, so backfill has left H by T − 60 s. The margin is there to absorb the stop's own delay (design section 5.5). The request commits when H is quiet.

**Late, e > 0, for the lease that sets T.** H is busy until T + e, so the request starts e after the tail it was promised. From T − 60 s to T + e no new start fits on H: the time left is shorter than the margin, and once T has passed a host takes none. H carries only the late lease. If another host would have been quiet before T + e, the request waited up to e longer than it had to, because the drain order chose H on its estimate. If T + e passes the reservation's deadline (the pool's `drain_deadline`, or the request's wall time when none is set: `WholeNodeConfig::drain_deadline_ms`, `crates/js-whole/src/config.rs`), nothing more happens: the mark is kept on H, no host is excluded and nothing is counted, and the request commits when H is quiet.

**Early, e < 0.** H's tail lease ends at T − |e|. Backfill booked against T ends by T − 60 s, so if no other lease on H is late, the request starts by T at the latest, and the promise holds. The cost is room: backfill was booked against a later tail than the truth. That can hold the request until T − 60 s when it could have started at T − |e|, or leave H idle.

**Against the feed off.** Every estimate is cut to its bound, so T is never later than the tail B the same host has with the feed off. An early error keeps the start at or before T, and T ≤ B, so it never does worse than the feed off. Only a late error can make the feed's choice worse than the choice made with the feed off. That is why the rule in section 4 counts positive errors only. The argument above follows the lease that sets T, but any lease on H whose actual end passes T breaks the promise the same way, even if its estimate was earlier than T. So the rule counts the late errors of every lease, not only of tail-setting ones.

**Worked example.** The numbers are chosen for illustration. Times are minutes:seconds from 0:00.

- H1 holds lease L: started 0:00, max wall 60 min (bound 60:00), one keyed run whose P90 is 20 min (estimate 20:00).
- H2 holds one lease with bound 30:00 and no keyed run (no estimate, so its bound).
- Feed on: the tails are H1 20:00 and H2 30:00, so H1 is marked with T = 20:00. A share-class start at 5:00 with a 14 min wall gives 5:00 + 14:00 + 1:00 = 20:00 ≤ 20:00, so it fits. One millisecond more does not.
- Feed off: the tails are H1 60:00 and H2 30:00, so H2 is marked with T = 30:00.

| L ends at | e | the request starts | against its promise | against the latest start with the feed off (30:00) |
|---|---|---|---|---|
| 20:00 | 0 | by 20:00 | on time | 10 min sooner |
| 20:45 | +45 s (bucket 5) | 20:45 | 45 s late | 9 min 15 s sooner |
| 25:00 | +5 min (bucket 6) | 25:00 | 5 min late | 5 min sooner |
| 33:00 | +13 min (bucket 7) | 33:00 | 13 min late | 3 min later |

A late error costs more than the feed off only when it is larger than the gap between the two hosts' tails (10 min here). Errors within the margin are small next to that gap.

**What the counters and rows cannot show, and what that does to the evidence.**

- *The end is the stop, not the exit.* A client that releases late reads as a late error even if its work ended on time. For this question that is the right end: the whole-node request waits for the stop, not the exit. What the rows cannot tell is whether the lateness was the work's or the client's.
- *A lease put back after a restart.* With the feed off it has no bound and no start, so it counts nothing and its row has no `run_ms`. With the feed on it keeps both. Runs are kept in server memory only (design section 10.1), so a restored lease has no keyed run, and therefore no estimate, until a new run is queued on it (test `a_lease_put_back_twice_keeps_one_bound_and_its_stop_counts_once`). Restarts raise the unestimated share r of section 4.
- *The raw estimate.* The error is measured against the raw estimate, not the estimate cut to the bound. A lease whose P90 exceeds its bound reads early by up to that excess, although the tail used the bound. This makes errors look earlier, never later, so p and q are not inflated by it.
- *Several runs on one lease.* The estimate takes the largest P90 among a lease's keys, not their sum (`lives::estimate_end`). A lease that runs keyed runs one after another is expected to read late. This is inferred from the code, not measured. In the design's target a job carries one profile key, so this bias belongs to leases and goes away with step 8.
- *With the feed off there are no estimates at all.* The rule below cannot be evaluated for a pool while its feed is off (section 5).

## 4. The evidence rule

*Proposed.*

**What is counted.** For one pool (today, one server), take the ended rows whose `actual_end` falls in a window W ending when the query runs, and keep those that:

- have a `host`;
- have `booked.whole_host` false: a host held by a whole-node lease is never marked (`reserve.rs`), so its lease never sets a tail;
- have `ended_reason` `released` or `stopped-at-wall-limit`. `lost` is left out because its end is when a lost host's lease was stopped, which says nothing about the work. `failed-to-start` and `cancelled-while-waiting` are whole-node bookings.

E is the rows kept that have an `estimated_end`. U is those that have none.

**The rule.** The feed may be turned on for the pool when all of these hold:

1. The pool's feed was on for the whole of W (otherwise E is empty, section 1).
2. W is at least N = 14 days long, and E holds at least M = 1300 rows.
3. At most p = 5 % of E have `error_ms` > 60 000 (late by more than the margin).
4. At most q = 1 % of E have `error_ms` > 300 000 (late by more than 5 minutes).
5. At most r = 15 % of E ∪ U are in U (no estimate).

Once the counters are stored (step 3), rules 3 to 5 read straight off them: p is `(error_buckets[6] + error_buckets[7]) / ended_estimated`, q is `error_buckets[7] / ended_estimated`, and r is `ended_unestimated / (ended_estimated + ended_unestimated)`. The counters do not apply the filter above, so the rows remain the exact source.

**Where each number comes from.**

| number | value | kind | reason |
|---|---|---|---|
| first threshold | +60 000 ms | chosen | equals `DEFAULT_BACKFILL_MARGIN_MS`: the question's own words, "bounded by the margin that backfill uses", and an edge of `ERROR_EDGES_MS` |
| second threshold | +300 000 ms | chosen | the last edge of `ERROR_EDGES_MS`, so stored counters can check it from bucket 7 alone |
| N | 14 days | chosen | covers two weekly cycles of work; equals the node-row retention (`NODE_RETENTION_MINUTES` = 14 × 24 × 60, `crates/js-usage/src/prune.rs`), so node load over the same window is still readable beside the rows |
| | 6.4 windows | derived | 90 / 14 = 6.4: the default history retention holds six whole windows, so a failed window can be checked again without changing retention |
| p | 5 % | chosen | see the arithmetic below |
| q | 1 % | chosen | see the arithmetic below |
| M | 1300 | derived | 1.645² × 0.05 × 0.95 / 0.01² = 1285.4, rounded up to 1300: with that many rows, the true late share is within 1 point above the share seen, at 95 % one-sided confidence |
| r | 15 % | derived | from a chosen 4-lease host and a chosen "at least half the hosts": 1 − 0.5^(1/4) = 0.159, rounded down to 15 % |

**Arithmetic for p.**

- *What a 5 % rate costs.* On a host whose tail is set by one lease, 1 whole-node request in 20 starts more than a minute after its promise.
- *Expected wait from bucket 6.* An error in bucket 6 overshoots the margin by at most 300 − 60 = 240 s. The expected extra wait it causes is at most (p − q) × 240 s = (0.05 − 0.01) × 240 s = 9.6 s per request.
- *A host's tail is the latest of its leases.* If all n leases on a host have estimates at the tail, the chance that the promise breaks by more than the margin is at most 1 − (1 − p)^n: 5 % for n = 1, 1 − 0.95⁴ = 18.5 % for n = 4, and 1 − 0.95⁸ = 33.7 % for n = 8. This is why the per-lease share must be small.
- *What the P90 itself allows.* Take a key whose runs do not change over time, with a full ring of 32. A new run is later than the 29th smallest of the last 32 (`ceil(0.9 × 32)` = 29) with probability (32 − 29 + 1) / (32 + 1) = 4/33 = 12.1 %. With 3 runs it is 1/4 = 25 %. A 5 % ceiling above +60 s therefore passes pools whose keys overshoot their P90 mostly by under a minute, and fails pools whose keys spread more. The rule is meant to tell those two kinds of pool apart.

**Arithmetic for q.**

- *Expected wait from bucket 7.* An error in bucket 7 costs at most q × D, where D is the mean overshoot beyond the margin of those errors. D is at most the gap between the lease's bound and its estimate, because the table ends the lease at its bound. For example, with D = 40 min, 0.01 × 2400 s = 24 s.
- *Total.* Beyond the margin, the expected extra wait is then at most 9.6 s + 24 s = 33.6 s per request, under one margin.
- *Why a separate ceiling.* Section 3 showed that only a late error larger than the gap between tails makes the feed do worse than the feed off. The gap is not fixed: when it is under 5 minutes, a bucket-6 error can already reach it. The 5-minute edge is therefore a chosen proxy for a typical gap, not a guarantee. Errors above it reach any gap of 5 minutes or more, so they get their own, tighter ceiling.

**Arithmetic for M.** Errors of one profile key are correlated with each other, so the effective sample is smaller than the row count. M is therefore a floor, not a target. Whether one key may supply most of M is open point 4.

**Arithmetic for r.** r does not protect anything. An unestimated lease is costed at its bound, which is what the feed off does. r makes sure the feed is worth having. The feed can move a host's tail earlier only if the lease with the latest bound has an estimate, and the simple case where that holds is every lease on the host having one. With r = 15 % and 4 leases, that case covers 0.85⁴ = 52.2 % of hosts. A pool that fails only rule 5 gains little from the feed and loses nothing by it.

**Collecting the evidence.** After a server runs the feed for a pool for at least N days, an operator runs a read-only query over that pool's `usage/ended/` rows. This note runs no such query and claims no result. Two things are missing before the query can run: the feed must have been on (rule 1), and there must be a way to read the rows (section 2). Section 5 lists the changes for both.

## 5. The default

*Proposed.*

**Recommendation.** This keeps the design's default (off, on per pool once evidence holds) and refines its evidence rule in two ways. The evidence is read from the stored ended rows, because the counters are memory-only and do not filter (section 2). And "bounded by the margin" becomes a share: at most p = 5 % of errors beyond the margin and q = 1 % beyond 5 minutes, since a P90 is exceeded by some runs by construction (section 4). Concretely:

- The server default stays off.
- The feed is turned on for a pool once the rule of section 4 holds for it, and turned off again when a later window fails rule 3 or rule 4.

**The mechanism it implies.** It implies a pool key, `estimate_feed` (`true` or `false`), in the pool's table: the `[[pool]]` table of the configuration file, or the `--agent-pool-config` file. The key overrides the server default (`server.estimate_feed` or `--estimate-feed`). A pool that leaves the key out takes the server default. A pool with too little evidence (rule 1 or rule 2 unmet) leaves it out, so it stays off while the default is off.

Today a server starts with at most one pool that registers agents, so the existing flag already is that per-pool switch. A second key would be a second way to say the same thing. The recommendation is therefore to add the pool key with design step 8, when a server first serves several pools, and until then to use `server.estimate_feed` as the pool's switch.

**Approval.** Any of these settings is a configuration change, and the owner approves it. This note adds none of them.

**Follow-up changes, by outcome.** Each line is one change.

- **Keep it off and close the question:** record question 3 as "off" in the design; no code change.
- **Keep it off but collect evidence** (needed by every other outcome, because the rule cannot be evaluated while the feed is off). Choose one of:
  - *(a)* Turn the feed on for one trial pool, a configuration change. That pool takes the late-error cost of section 3 during the window.
  - *(b)* Change the code so the registry computes the estimate for the row and the counters with the feed off, without passing it to the tail or to the restore path. Then `estimated_end` would no longer mean only the end "the estimate feed gave it", and the field's doc changes with it.
- **Plus, in either case:**
  - a read path for the rows: `GetUsage` served from `usage/ended/`, with an optional estimated-end field added to `UsageRecord`, or a read-only operator query tool over `EndedHistory::range`;
  - the counters stored (design step 3).
- **Add the per-pool setting:** the `estimate_feed` pool key overriding `server.estimate_feed`, with design step 8.
- **Flip the server default:** `server.estimate_feed` defaults to `true`, with the flag text in `lib.rs` and test `the_flag_is_off_by_default_and_needs_an_agent_pool_and_usage` changed to match. That also changes restart behaviour: restored leases then keep their recorded bounds (design section 8, row 10).

## 6. Open points

Questions this note could not settle from the code, each with the code that would settle it.

1. **Several runs on one lease.** How late do leases that run keyed runs in sequence read, and should evidence on leases count at all once jobs replace them? Settled by grouping rows by `profile` against `run_ms` and `error_ms` (`lives::estimate_end`, design step 8).
2. **Which read path.** Should the rows be read through `GetUsage`, with a new optional field in `UsageRecord`, or through an operator query tool? Settled in `crates/js-proto/proto/jobscheduler/v1/usage.proto` and `UsageSink::ended_rows`.
3. **The pool on the row.** The row names a host, not a pool. With one agent pool per server that is enough. When a server serves several pools, per-pool evidence needs the pool on the row (an added field, `EndedRow`) or a host-to-pool lookup.
4. **One key dominating the sample.** Should the rule cap the share of M that one `profile` supplies, since a key's errors are correlated? Settled by the spread of `profile` across E in the first window read.
5. **Which estimate the row keeps.** Should the row record the end the tail used (`cost::lease_end`, the estimate cut to the bound) beside the raw one? Settled in `hosted_row` (`crates/js-registry/src/registry/history.rs`).
6. **A stale comment.** The doc comment of `DEFAULT_BACKFILL_MARGIN_MS` (`crates/js-whole/src/cost.rs`) says "Nothing in production uses it yet", but `js_sched::backfill::fits` passes it to `fits_in_tail` on the server's backfill path. That comment needs a correction in its own change.
7. **The reservation deadline.** How often does a late end pass the reservation deadline? A pool with no `drain_deadline` has the request's wall limit as deadline (`drain_deadline_ms`, `crates/js-whole/src/config.rs`). Passing it gives nothing up and the mark is kept, so the count of rows that waited past it is read from the drain outcome and the queue wait.
