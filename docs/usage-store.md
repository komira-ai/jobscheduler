# The usage store

The server keeps what it measured and what it learned in one `js-state` store: the SQLite file `--usage-db` names (`usage.db` beside the lease file unless given). `js-usage` owns the rows; the server opens the store and hands it to them. These key spaces live there:

| prefix | one row per | written | kept |
|---|---|---|---|
| `usage/node/<host>/<hour>` | node and hour: up to 60 minutes of the node's CPU, working set and lease count | by the flush, every five minutes | 14 days (`NODE_RETENTION_MINUTES`) |
| `usage/key/<profile key>` | profile key: the ring of its last runs that ended cleanly, from which its cost curve and its P90 wall time are fitted | when a clean run of the key ends | under `--profile-store-bytes`, least recently used key first |
| `usage/ended/<ended-at>/<id>` | ended allocation: the record below | by the tick after the allocation ended | `--history-retention-days` (default 90) |
| `usage/ended-id/<id>` | ended allocation: the key of its record, so an allocation is recorded once | with its record, in one transaction | deleted with its record |

Every row starts with a version byte.

## The record of an ended allocation

`<ended-at>` is the end in milliseconds since the Unix epoch, zero-padded to 20 digits, so a scan of the prefix is in time order; `<id>` names the allocation. The record is the first form of the allocation record of the design (section 10.1, "Usage history"): today an allocation is a lease's booking on a host, and `<id>` is the lease id's text (`<term>-<sequence>`). When the scheduler owns job allocations, the same row is written, under the same key and with the same fields, keyed by the job id, when an allocation is freed.

| field | what it holds |
|---|---|
| `id` | the allocation: the lease id's text today, the job id later |
| `profile` | the profile key a run of the lease named, if one did (the first a run of the lease was queued with) |
| `host` | the host it was booked on; absent for a request cancelled before it had one |
| `booked` | what it booked: whether the whole host, and the CPU (thousandths of a core) and memory (bytes); absent with no host |
| `queued_ms` | the queue wait, below; absent when not known |
| `run_ms` | from the lease's start to its end, on the registry's clock; zero for a request cancelled before it had a host; absent when the start is not known |
| `ended_reason` | why it ended: `released`, `expired`, `lost`, `cancelled-while-waiting`, `failed-to-start` or `stopped-at-wall-limit` |
| `drain_outcome` | what became of the host set aside for a whole-node request: `none` (an ordinary lease), `no-mark`, `mark-held`, `mark-dropped`, or `unknown` (put back after a restart) |
| `give_up_outcome` | how many hosts a whole-node request gave up waiting for (drain give-ups); absent for an ordinary lease |
| `estimated_end` | the end the estimate feed (`--estimate-feed`) gave the lease, if it had one |
| `actual_end` | when it ended: the key's time |
| `error_ms` | `actual_end` less `estimated_end`, positive for a lease that outlived its estimate; absent without an estimate. It is the error the tail counters bucket |
| `used` | over the minutes the host's heartbeats reported for the lease while the server held it: the highest minute of mean CPU (thousandths of a core), the highest minute of working set (bytes), and the CPU averaged over the minute indices (thousandths of a core, rounded down). The agent folds every minute from minute 63 of the lease on into that one index and sends it again as it grows; it counts as one minute of the average, at the highest CPU it was sent with. Absent when none was measured |

**Queue wait.** For a whole-node request it is the time from the request's first presentation to the start of its lease (the commit on its host), across every presentation and every host it gave up. An ordinary lease is placed when it is asked for or refused, never queued, so its wait is zero. A request whose lease is stopped while it still waits gets a record with no host, a run time of zero and the wait it had. A lease put back from the lease file after a restart has a known wait only if it is an ordinary one.

**Why it ended, as the registry tells it today.** A stop does not say whether the client released the lease or its time ran out, so both are `released`; the registry tells `lost` (the host was silent past the grace when the lease ended), `stopped-at-wall-limit` (the lease ended at or after the latest end its wall time allows) and `cancelled-while-waiting`. A whole-node prepare that fails (the agent refuses it, or it is not ready within `prepare_timeout`) frees its booking without a stop; the registry records it then, as `failed-to-start`, with the whole host booked and the time from the commit to the failure as its run time, and a later stop of that lease records nothing more. `expired` is kept for the writer that can tell it. A whole-node request that the wait list dropped because its consumer stopped presenting it has no record: it booked nothing and was not cancelled. One put back from the lease file and cancelled before it was presented again is recorded, waiting from its first presentation as the lease file kept it.

**One estimate, one profile.** The estimated end of a stopped lease is read once, and the same value goes into its record and into the tail counters. The profile is the first a run of the lease was queued with, kept until the lease stops however soon the run itself is forgotten.

**How it is written.** The registry reads the record before the end changes anything (in its stop, or as a failed prepare frees the booking), and hands it to the usage side after its own lock is released. The usage side adds what it measured of the lease and queues the record in memory; it never waits on the store there, so a stop or a heartbeat never does either. The next tick writes the queue, oldest first; one write runs at a time, and a record leaves the queue only once the store has taken it. A record and its `usage/ended-id/` entry are written together, and only if the allocation has no record yet under any end: a record written twice, a second stop of a lease, and a lease put back after a restart and stopped again all keep the first. A write that fails (or panics) leaves that record and the ones after it queued for the next tick; at most 4096 records wait, and past that the oldest is dropped. Written, failed and dropped records are counted in the usage counters, and a run of failed writes is logged once when it begins and once when a write succeeds again.

**Retention.** The daily prune deletes the records that ended more than `--history-retention-days` days ago, each with its `usage/ended-id/` entry (`history_retention_days` in the configuration file, default 90, at least 1); one that ended exactly that long ago is kept. The setting bounds how long records are kept, not how many tenants or how much work they may describe.

**Reading.** `UsageSink::ended_rows(from_ms, to_ms, limit)` returns the records that ended in a range, oldest first. A record whose key is not the one its own end and id give is skipped and counted, as an unreadable one is. No route serves them yet.

**Stored form.** A version byte (1), then each field in the order of the table: text as a varint length and UTF-8 bytes, a number as a varint, the error as a zigzag varint, and an optional field as a byte 0 (absent) or 1 followed by the field. A field is added at the end under the same version, and a reader ignores the bytes after the fields it knows, so an older build reads a newer record; the version changes only if a field already written changes meaning. A record this build does not read (another version, a reason or outcome name it does not know, bytes that do not decode) is left in place, and a read skips and counts it rather than failing.
