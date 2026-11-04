# The usage store

A server with agent registration keeps what it measures in one state store, the usage
store: by default the SQLite file `usage.db` beside the lease file, or the file
`--usage-db` (`server.usage_db`) names. `--usage-off` (`server.usage_off`) runs without
one, and so does a server without agent registration.

Its key spaces:

| Prefix | One row per | Holds | Code |
|---|---|---|---|
| `usage/node/` | node and hour | the node's minutes of that hour | `js-usage` `store.rs` |
| `usage/key/` | profile key | the key's last runs and the day it was last used | `js-usage` `profile_store.rs` |
| `usage/tail/` | tail counter | the counter's total since the rows were first written | `js-usage` `tail.rs` |

## Tail counters

The registry counts what the tail rule of whole-node placement is measured by: leases
started on a host a whole-node request marked, how many of those were backfill, and how
far each ended lease's end was from its estimated end (a count with an estimate, a count
without, a signed sum in milliseconds and eight signed buckets).

**Rows.** One row per counter, named by its field:

- `usage/tail/started_on_marked`, `usage/tail/backfilled`, `usage/tail/ended_estimated`,
  `usage/tail/ended_unestimated`: a count;
- `usage/tail/error_sum_ms`: the signed sum of the errors (actual end minus estimated end);
- `usage/tail/error_bucket_0` to `usage/tail/error_bucket_7`: the errors by bucket;
- `usage/tail/error_edges_ms`: the bucket edges the counts were made with.

A counter row is a version byte (1) and the value as 8 little-endian bytes, unsigned for a
count and two's complement for the sum. The edges row is a version byte (1), the number of
edges and each edge as 8 little-endian bytes.

**Writing.** The registry hands each change to the sink once its lock is released; the
sink joins it to the counts waiting to be written, and the usage flush (every few minutes,
and at shutdown) adds them to the rows in one transaction conditional on the version of
every row it read. A row that changed in between is read again and the write tried again,
so two writers, or two boots, each add their own counts and neither loses nor doubles the
other's. A write that fails keeps its counts for the next flush, is counted
(`UsageCounters::tail.writes_failed`) and is logged once per run of failures. The counts
not yet written when a server stops without its final flush are lost from the store.

**Restart.** At start the registry reads the rows and counts on from them, so its
counters are the stored totals plus every count since, written or not. The rows are
additive: the server never resets them; only a person deleting them does.

**Refusals.** Rows bucketed by other edges than the build's, or whose edges row does not
read, are refused: the counters start at zero, nothing is added to those rows, and they
are left as they are. A counter row of a newer version is skipped, read as zero, counted
(`UsageCounters::tail.rows_skipped`) and never overwritten. A store that cannot be read at
start leaves the counters at zero and is logged once.

**Without a usage store** the counters are in the registry's memory only and start at
zero at every start.
