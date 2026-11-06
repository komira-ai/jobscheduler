# Live logs: a job's output in an object store

Status: **Planned, partly built.** Built so far: the wire fields of section 7 (`js-agent-wire`, with the per-run capture file cap
in `js-contain`); the object-store client (`js-logstore`); the `[logs]` table of section 6, which the server reads and checks at
start; the mint of section 4 (`js-logmint`) and its second kind for a store with no STS, the prefix grant of section 4.1
(`js-logmint` and its signer in `js-logstore`); the server's side of sections 3.1, 4 and 5 (`js-lease-server`'s `logs` module: one
credential per uploading host, minted, delivered and renewed in heartbeat replies, a sink in each start, and the stream lengths
stored with the result); the agent's uploader (`js-agent/src/logs`, with the progress record of 3.5) of a `crun` job and of a
macOS native job (3.7), which fills `log_grant_state` and `logs`; and the reader of section 8
(`js-lease-server`'s `job_api/logs`: `GetLogs` with a stream, the long poll, and its gateway route). `jobscheduler-design.md` links here for the detail of live logs; it is cited as
"design section N", and a bare section number is this page's own. The step that builds it is step 31 of design section 12.1.

A job's output reaches a client today only as the 8 KiB tail of each stream in the job's result (design section 5.3). This page is the design
of live logs: while a job runs and for the retention window after it ends, a client reads the job's whole output, stdout and stderr
apart, from any byte offset, and can follow it as it is written. The bytes go from the host to an object store the operator names; the
scheduler never stores or holds them.

## 1. What is decided

- **Live logs are part of v1.** Log bytes never go to the scheduler's store or database.
- **The destination is the server's configuration**: a `[logs]` table with `sink = "object_store"` or `"none"` and
  `url = "s3://bucket/prefix"`, naming any S3-compatible store (S3, MinIO, tierstore, Google Cloud Storage's interoperability API; a
  store must also serve `AssumeRole`, section 4, or check prefix grants, section 4.1). `none`, the default, keeps logs on the host only, as today.
- **The agent uploads, not the job.** The agent sends new output in chunks every ~2 s or every 1 MiB, whichever comes first, to
  `{prefix}/{host}/{job}/stdout/{offset}` and `{prefix}/{host}/{job}/stderr/{offset}`, where `{host}` names the host that runs the job
  (section 2) and the offset is the byte offset of the chunk's first byte in
  its stream, zero padded so that names sort. When the job ends the agent writes one final `stdout.log` and one final `stderr.log` and
  deletes the chunks.
- **One upload credential per host.** The scheduler mints each agent's write-only credential, one per host, scoped to the host's own
  prefix under the configured log prefix (section 4): a host writes its own jobs' logs and no other host's. It is the fence of design
  section 5.7 (a job writes to the object store only through a grant), widened from one key to the host's prefix.
- **`GetLogs(job, stream, from_offset, follow)` reads from the object store.** `follow` waits for new chunks (like `tail -f`, about 2 s
  behind the job); the offset lets a client resume exactly where it stopped.
- **The store being down never blocks a job.** The agent keeps the output locally up to its cap and retries; bytes it cannot keep become
  a gap marker that the reader sees. Logging never blocks or slows the job.
- **The format is the raw bytes** exactly as printed, stdout and stderr apart, addressed by byte offset. Each chunk records the arrival
  time of its first byte as object metadata. There is no line parsing and no JSON or CRI wrapping.
- **The result's tail stays as it is**: 8 KiB per stream, in the result, whether a sink is configured or not.

Inside that, the design takes these choices; each is explained where it applies:

- **D1.** The credential is minted by the store's STS `AssumeRole` call with an inline session policy (section 4), or, for a store
  with no STS, derived by the scheduler as a prefix grant (section 4.1); one per host, scoped to the host's prefix, delivered in the
  heartbeat reply and renewed per host.
- **D2.** `GetLogs` stays a unary call. `follow` is a bounded long poll, never a server stream (section 8).
- **D3.** A final file needs the whole stream on the host and at most `MAX_PUT_BYTES`; otherwise the chunks stay and are the log (3.4).
- **D4.** The scheduler never deletes log objects. Their retention is the operator's bucket lifecycle rule (section 10).
- **D5.** A failed mint costs a host's jobs their live upload until a later mint succeeds, never a start (section 5).
- **D6.** This release uploads from the Linux `crun` runtime and the macOS native runtime (3.7, which says how the native runtime's files differ from the capture files of 3.2); the
  container-engine fallback does not upload (section 10).
- **D7.** The agent reads the capture files, not the pipes (3.2).

## 2. The object layout

Every part of this feature uses exactly this layout.

**The prefix.** Let `prefix` be the path of the configured `url`: no leading or trailing `/`; it may be empty; its segments are
non-empty runs of `[A-Za-z0-9._-]` and none is `.` or `..`; at most 512 bytes in all. A character such as `*`, `?`, `$` or `{` is
refused in a prefix, because the session policy of section 4 names the prefix in a resource pattern and such a character would widen
it.

**The host's prefix.** Let `H` be the host segment: `js-` and the first 32 hex digits of the SHA-256 of the host's agent id (an agent
id may be longer than a key segment should be, or hold characters a key refuses; `H` never does). The host's prefix is `prefix/H/`, or
just `H/` when the prefix is empty. Two hosts' prefixes have the same length and differ, so neither is a prefix of the other. The
configured prefix is at most 476 bytes, so that `prefix/H` fits the 512 bytes below; a longer one is refused at start. Both minting
kinds scope a host's credential to exactly the host's prefix (section 4), so a host can write only its own jobs' logs.

**The job's prefix.** Let `P = prefix/H/job_id/`, or just `H/job_id/` when the prefix is empty, where `H` is the segment of the host
the job's start is sent to. The job record keeps that host (`host`) after the job ends, so the reader builds `P` from the record; a
job put back in the queue loses its host and gets a new one, and so a new `P`, with its next start. The job id is the service's (`js-jobs`
`JobId`: 1 to 63 bytes of `[A-Za-z0-9._-]`). The agent wire checks an id more loosely (up to 128 bytes, `MAX_ID_LEN`) but requires a
leading letter or digit, so a job's prefix is built only for an id of 1 to 63 bytes of that alphabet whose first byte is a letter or a
digit. Such an id never holds `/` and is never `.` or `..`. A job whose id does not qualify runs without live logs (its start carries
no `log_sink`).

**The objects.**

| Object | Key | Body | Metadata |
|---|---|---|---|
| chunk | `P` + `stdout/` or `stderr/` + `offset20` | the raw bytes, 1 to 1,048,576 of them | `x-amz-meta-first-byte-at-ms`; content type `application/octet-stream` |
| gap marker | `P` + `stdout/` or `stderr/` + `offset20` + `.gap` | JSON `{"length":N}` | none |
| final | `P` + `stdout.log`, `P` + `stderr.log` | the whole stream, one object | content type `application/octet-stream` |

- `offset20` is the decimal byte offset of the object's first byte in its stream, zero padded to 20 digits (every `u64` fits).
- `x-amz-meta-first-byte-at-ms` is decimal milliseconds since the Unix epoch, on the agent's clock, of the first poll of the job's
  capture file that saw the chunk's first byte. The agent polls every `LOG_POLL` (250 ms), so this is the byte's arrival time to within
  that interval; bytes written while no agent ran carry the time of the poll that resumed the upload.
- A gap marker says the bytes `[offset, offset+N)` of that stream were never uploaded. The next chunk, if any, begins at `offset+N`. A
  marker and a chunk never share an offset.
- A final, when it exists, is authoritative: the chunks are gone or ignored.
- A chunk is rewritten whole at the same key when an agent resumes after a restart (3.5); a reader always sees the last write of a key,
  and chunks never overlap.
- **A reused job id.** A job id is reusable once the retention window of the job that held it has passed (design section 5.3), and the
  lifecycle rule may keep that job's objects longer. The reader therefore ignores every object of `P` last modified before the job was
  placed (the placement time is in the job record): those belong to the earlier job. The reader allows a margin of 5 minutes before
  the placement, for a store's clock behind the server's and timestamps in whole seconds; an earlier job's objects are older than that
  by at least its retention window.

**The stream's length** is what the job printed, which the agent reports in the result (`logs.stdout_bytes`, `logs.stderr_bytes`). A
reader knows a stream is complete when its next offset equals that length. `logs.stdout_dropped` and `logs.stderr_dropped` are the
bytes of the stream that the host's capture could not keep: the stream's length minus the capture file's, that is the bytes past the
local cap or after a file write failed. They are never uploaded.

### 2.1 Worked example: 2.5 MiB to stdout, 100 bytes to stderr

The configuration has `url = "s3://logs/js"`, so the bucket is `logs`, the prefix `js`, and for the job `build-42` on a host whose
segment is `H`, `P = js/H/build-42/`. The job prints 2,621,440 bytes (2.5 MiB) to stdout, fast, and 100 bytes to stderr, early. The local cap is the
default 64 MiB, so nothing is dropped.

| Object | Offset | Bytes | When |
|---|---|---|---|
| `js/H/build-42/stderr/00000000000000000000` | 0 | 100 | 2 s after the poll that saw the first byte (`LOG_FLUSH`) |
| `js/H/build-42/stdout/00000000000000000000` | 0 | 1,048,576 | as soon as 1 MiB is waiting (`MAX_CHUNK`) |
| `js/H/build-42/stdout/00000000000001048576` | 1,048,576 | 1,048,576 | the same |
| `js/H/build-42/stdout/00000000000002097152` | 2,097,152 | 524,288 | 2 s after its first byte was seen, or at the job's end |

The stdout chunks add up: 1,048,576 + 1,048,576 + 524,288 = 2,621,440. What a reader does at each step, with the default page of
256 KiB (262,144 bytes):

1. **While the job runs.** `GetLogs(build-42, stdout, from_offset = 0, follow)` lists `js/H/build-42/stdout/`, finds the chunk at 0 and
   answers `data` = bytes `[0, 262144)`, `offset` 0, `next_offset` 262,144, `first_byte_at_ms` from that chunk, `eof` false. The client
   calls again from 262,144; four calls read the first chunk. A call from 2,621,440 before the job has ended finds nothing new and,
   with `follow`, waits for a new chunk, the job's end or the follow cap (section 8).
2. **The job ends.** The agent reports the result at once, with `logs` = `{stdout_bytes: 2621440, stderr_bytes: 100, stdout_dropped:
   0, stderr_dropped: 0}`. In the background it uploads anything not yet uploaded (here the last stdout chunk, if its 2 s had not
   passed), writes `js/H/build-42/stdout.log` (2,621,440 bytes) and `js/H/build-42/stderr.log` (100 bytes), then deletes the four chunks.
3. **Between the finals and the deletes**, a reader sees both a final and chunks. The final wins.
4. **After the end.** `GetLogs(build-42, stdout, from_offset = 2359296)` finds `js/H/build-42/stdout.log` and answers `data` = bytes
   `[2359296, 2621440)` (262,144 bytes) by a ranged read, `next_offset` 2,621,440, `eof` true (the job ended and `next_offset` equals
   `stdout_bytes`), and no `first_byte_at_ms` (a final served it).

**The same job past the cap.** Had it printed 70 MiB (73,400,320 bytes) to stdout with the 64 MiB cap, the capture would have kept
the first 67,108,864 bytes. The agent uploads 64 chunks of 1 MiB, the last at `js/H/build-42/stdout/00000000000066060288`, and at the end
writes the gap marker `js/H/build-42/stdout/00000000000067108864.gap` with the body `{"length":6291456}` (73,400,320 − 67,108,864 =
6,291,456). The result says `stdout_dropped: 6291456`. There is no `stdout.log` (D3): the chunks and the marker stay, and a reader that
reaches offset 67,108,864 is told of the gap `{offset: 67108864, length: 6291456}` and `eof`.

## 3. One job's logs

### 3.1 The start

A server with `sink = "object_store"` puts a `log_sink` in a job's start (`JobStartSpec.log_sink`, section 7) when the host's hello
reported the capability `log-upload` and the job's id qualifies (section 2). The sink says where to upload and nothing else: endpoint,
region, bucket, the job's prefix `P` and `path_style`. It holds no secret. The placement commit records on the job that its start
carries a sink, so that `GetLogs` can tell a job with live logs from one without (section 8).

The credential the uploads use is the host's, not the job's (section 4). The agent already holds it, or receives it in a later
heartbeat reply. A start never waits for it: the job starts, and its uploader waits as for a store that is down (section 5) until a
credential is there.

The agent runs a job with a sink with the larger local cap of `--log-file-cap` (default 64 MiB a stream, section 9). A job without a
sink keeps today's 1 MiB.

### 3.2 Where the agent reads the bytes (D7)

js-contain's capture writes the first `file_cap` bytes of each stream to a file in the job's lease directory (`stdout` and `stderr`,
`js-contain/src/oci/logs.rs`), with plain unbuffered writes, from the supervisor process that runs the container and outlives the
agent (`js-contain/src/oci/supervise.rs`, `js-agent/src/crun/mod.rs`). The agent's uploader tails those two files by byte offset; it
does not read the pipes. It polls the size of each file every `LOG_POLL` (250 ms). What follows from this:

- **The capture file is the local backlog.** "Keep the output locally up to the cap" is that file; the uploader keeps no copy of its
  own.
- **Bytes past the cap are discarded by the capture** and so are never uploaded. A reader sees them as a gap: the cap bounds live
  coverage of a stream in every case, not only while the store is down (section 10, "Live log coverage").
- **A chunk's first-byte time** is the first poll that saw the byte.
- **The job is never blocked by logging.** The uploader reads files and takes no lock that the capture or the agent's worker takes; a
  slow or absent store slows only the uploader.
- **A restarted agent can resume**, because the files and the supervisor survive the agent (3.5).
- **The uploader holds the two files open** from the job's start to the end of its upload, so removing the lease directory at cleanup
  does not cut a final write short. The disk space of the removed files is freed when the uploader closes them, at most
  `FINISH_BUDGET` after the job's end.

### 3.3 The chunk loop

For each stream the uploader keeps `next`, the offset of the first byte not yet uploaded. Each poll that finds the file longer than
`next` notes the poll time as the first-byte time of the waiting bytes if none is noted. The uploader puts a chunk when 1 MiB is
waiting (`MAX_CHUNK`) or the first waiting byte was seen `LOG_FLUSH` (2 s) ago, whichever comes first. The chunk is
`[next, next + min(waiting, MAX_CHUNK))`, read from the file, put at `P/<stream>/<offset20 of next>`. On success `next` moves past
it, the progress record is written (3.5), and the first-byte time of what still waits is the time of the poll that first saw the next
byte. On a failure the same chunk is put again after a backoff (1 s doubling to 60 s, jittered), and meanwhile the file goes on
filling up to its cap. The two streams upload independently; within a stream chunks go out one at a time, in offset order, so no chunk
is put before every chunk below it.

### 3.4 The end

The job's end is the supervisor's exit record: the container has exited and the capture has drained (`LogLimits::drain_grace`).

1. **The result is reported at once**, before any final upload and before cleanup, with the tails (unchanged) and `logs`: each stream's
   length (the bytes the capture read) and the bytes it dropped (the length minus the file's). The allocation and the cleanup never
   wait for the upload.
2. **In the background**, for each stream, within `FINISH_BUDGET` (5 minutes from the job's end): put what is still waiting as chunks;
   if bytes were dropped, put the gap marker at the file's length with `{"length": dropped}`; then, if the stream is whole in its file
   (nothing dropped) and at most `MAX_PUT_BYTES` (64 MiB, the store client's one-put limit), put the final `P/<stream>.log` from the
   file in one put (D3); once the final is stored, delete the stream's chunks and markers that this agent put (it keeps their offsets,
   in memory and in the progress record).
3. **A stream cut by the cap, with a gap, or longer than `MAX_PUT_BYTES` gets no final.** Its chunks and gap marker are kept and are the
   log. The final of the other stream does not depend on it.
4. **The budget ends the work.** At `FINISH_BUDGET`, or at the expiry of the credential when no newer one has come, the agent gives up
   whatever is left and closes the files. What was put stays; what was not is a gap to the reader (section 8). An expired credential
   is never used.

### 3.5 An agent restart, and the progress record

The supervisor keeps capturing while no agent runs, and a restarted agent adopts the container (`js-agent/src/crun/adopt.rs`). For
each job with a sink the agent keeps a **progress record** in its state root, beside the held results and retired ids it keeps there
today (`js-agent/src/lifecycle/machine/held.rs`): the job's `log_sink` (no secret) and, per stream, `next` and the offsets of the
chunks and markers it put. The record is written before the first put and replaced (temporary file, rename) after each successful put.

- **With a record**, the restarted agent resumes each stream at `next` and uploads what was written meanwhile, its first-byte time the
  resuming poll. A put that succeeded just before the restart but was not recorded is put again at the same key, so it is rewritten, not
  doubled.
- **Without a record** (lost or never written), the agent no longer has the job's sink, which came only in the start, and cannot ask
  the store what was uploaded: the credential has no read and no list. It uploads nothing more for that job. The range after the last chunk an earlier agent put has no object and
  no marker; once the job has ended the reader reports it as a gap after its settle window (3.6), up to the length the result reports.
  The chunks put before the restart stay, and the lifecycle rule removes them (D4).
- **A restart during the end** (3.4, step 2) after cleanup removed the lease directory finds no file to read. The finals are not
  written and the chunks stay.

### 3.6 The reader

`GetLogs` with a stream (section 8) reads the object store with the server's read key:

1. If the final `P/<stream>.log` exists, it serves the bytes from it by a ranged read. The job has ended, and `eof` is set when the page
   reaches the end.
2. Otherwise it lists the stream's keys from the one named for `max(0, from_offset − 1,048,575)`, since no chunk is longer than
   `MAX_CHUNK`. It finds the chunk that holds `from_offset` and the objects after it, and reads the page from them: a page is
   contiguous bytes, so it stops before the next gap or hole. A gap marker can be longer than a chunk, but the agent writes one only at
   the end, at the file's length (3.4): once the result reports `*_dropped` above 0, the reader knows the marker's key (the stream's
   length minus the dropped bytes) and that it covers the rest of the stream, without finding it by the listing.
3. A **hole** is a range no chunk or marker covers, below an object that exists or below the stream's reported length. It is not
   reported while it may still be filled; it is reported as a gap once the object after it was last modified `LOG_SETTLE` (30 s) before
   the read, or the job ended `LOG_SETTLE` before the read. Within a stream chunks are put in offset order, so a hole is filled only by
   an upload already in flight, and the settle window covers that and a store whose listing lags.
4. **The end of a stream** is known from the result: once the job has ended, `eof` is set when `next_offset` equals the reported length.
   A result without `logs` (an agent that never got to report one) leaves the length unknown: then `eof` is set once the job ended
   `LOG_SETTLE` before the read, at the end of the last object found.

### 3.7 The macOS native runtime

A native job has no supervisor and no capture pipe: the agent makes its standard output and error as two files in the run's scratch
directory (which only the agent can enter) and the helper gives them to the job, which writes them itself
(`js-agent/src/native/logs.rs`). Each file holds the whole stream, up to the run's file-size limit (`--native-max-file-mib`), whether
or not an agent runs. The uploader of a run whose start carried a sink is the same as `crun`'s (same requests, keys, offsets, chunk
loop and end), started once the program has started, with two differences:

- **It reads at most `--log-file-cap` bytes of each file.** What lies past the cap counts as dropped, so the store gets what `crun`'s
  capture would give for the same output: chunks up to the cap and, for a longer stream, the gap marker at the cap and no final. The
  result's tail is the end of each file, the same text as a capture's ring of the last bytes. A run without a sink is as before.
- **The end** is told at the first poll that reads the run's exit, before its result is reported, with each file's length as the
  stream's total; a run whose exit was never read is told it at its removal, after its processes were killed. The result never waits for the upload. The progress record is kept in the scratch directory and is stopped and
  removed before the directory is.

**A restart.** The helper keeps a native run's process while no agent runs, and the job keeps writing its files; the next agent
adopts the run (`js-agent/src/native/adopt.rs`) with its scratch directory. A run whose directory holds a progress record has its
upload go on from it as 3.5 says: a chunk in flight is put again at the same key, and the bytes written while no agent ran go out as
ordinary chunks, their first-byte time the resuming poll. Its end is then told as any run's. Without a record it uploads nothing more.

## 4. The host credential (D1)

**One per host.** The scheduler mints one temporary S3 credential per registered agent, that is per host, through the store's STS
`AssumeRole` call, with an inline session policy that allows `s3:PutObject` and `s3:DeleteObject` on `arn:aws:s3:::BUCKET/LH/*` and
nothing else: no read, no list, no other prefix. `L` is the configured log prefix with its trailing `/` (empty when there is none) and
`H` the host segment (section 2). The credential covers the log objects of every job of that host, and no other host's. A store with
no STS gets the same scope from a prefix grant instead (4.1).

**The call.** `AssumeRole` at `sts_endpoint`, signed (AWS Signature Version 4, service `sts`, the configured `region`) with the
mint key (`mint_key_file`), with:

- `RoleArn` = `role_arn`;
- `RoleSessionName` = `js-` and the first 32 hex digits of the SHA-256 of the host id (a host id may be longer than the call allows or
  hold characters it refuses), so the store's audit trail names the host;
- `DurationSeconds` = `credential_ttl` in seconds;
- `Policy` = the session policy below.

The answer's access key id, secret access key, session token and expiry are the credential. An answer whose credential expires within a
minute, or lives less than half of the `DurationSeconds` asked (a store that shortens sessions), is refused and counts as a failed mint
(section 5): the scheduler never hands out a credential about to die.

**The session policy**, exactly, for the bucket `logs`, the prefix `js` and a host whose segment is `H`:

```json
{
  "Version": "<policy language version>",
  "Statement": [
    {
      "Effect": "Allow",
      "Action": ["s3:PutObject", "s3:DeleteObject"],
      "Resource": "arn:aws:s3:::logs/js/H/*"
    }
  ]
}
```

`<policy language version>` stands for the version string of the IAM policy language that the AWS documentation names as current; the
minting code holds it as a constant, and this page does not spell it because it is written as a calendar date. For an empty prefix the
resource is `arn:aws:s3:::logs/H/*`. A session policy can only narrow what the role allows, never widen it, so the credential can do no
more than this whatever the role grants.

**What the operator configures in the store.**

- **The role** (`role_arn`): a permission policy that allows at least `s3:PutObject` and `s3:DeleteObject` on `arn:aws:s3:::BUCKET/L*`
  (more is harmless, since the session policy narrows it); a trust policy that lets the mint key's identity call `sts:AssumeRole` on it;
  a maximum session duration of at least `credential_ttl`. STS refuses a longer duration than the role allows, which reads as a mint
  failure (section 5).
- **The mint key** (`mint_key_file`): a long-lived key whose only permission is `sts:AssumeRole` on `role_arn`.
- **The read key** (`read_key_file`): a long-lived key with `s3:ListBucket` on `arn:aws:s3:::BUCKET` limited to the prefix `L`, and
  `s3:GetObject` on `arn:aws:s3:::BUCKET/L*`. Nothing else.
- **A lifecycle rule** on the prefix, at least `--job-retention` (D4, section 10).
- **The log prefix apart from every client's output keys.** A host credential may put and delete anything under its host's prefix, so
  a client must not let its jobs' outputs (design section 5.7) land under the log prefix.

**Lifetime and renewal, per host.** The credential lives for `credential_ttl` (default 1 h). The scheduler mints a host's first one when
an agent that reported `log-upload` registers, or beats for the first time after a server restart. It mints a new one when the one it
holds has passed half its life, and when the agent reports `stale` while holding the very credential the server holds (the store
refused it). Otherwise it resends the one it holds, in every heartbeat reply, until the agent's heartbeat reports it held. Mints run off
the tick's path, a few at a time, and a heartbeat never waits for one (design section 6.1).

**Why per host, not per job.** One `js-agent` runs every job on its host and does all the uploading; the job's container never sees a
credential; and an agent that is compromised already controls every job on its host, its output included. A credential per job would
buy nothing against that agent and would cost one `AssumeRole` call per job start. One per host costs one call per host per half
`credential_ttl`.

**How it reaches the agent.** In the heartbeat reply, per host (`HeartbeatReply.log_grant`, section 7), never in a job start. It has
the protection that a run's secret `files` have: the agent channel, as the operator set it up (design section 4.4; TLS off
loopback). The agent holds the credential in memory only, never writes it to disk, and never prints it: the secret and the session token
appear in no `Debug` output and no log line, on the agent or on the server. A restarted agent holds none and reports `stale`.

**What the agent may do with it**: put chunks, gap markers and finals under the job prefixes of its own jobs' sinks, and delete the
chunks and markers it put. Nothing else; the agent's code builds no other key.

**What a compromised host can do with it**, until it expires: put and delete any object under its own host's prefix. For its own jobs
that is no more than it can already do (it controls them and their output). It can also fill its prefix with bytes the operator pays to
store. It cannot touch the log objects of jobs on other hosts, cannot read or list any log, cannot touch any object outside its prefix,
and cannot reach the mint key, the signing key, the read key or another host's credential. One credential read off the agent channel
allows the same, until it expires.

**What the scheduler holds.** Two operator-provided keys, the mint key (or, with prefix grants, the signing key) and the read key, as
owner-only key files named in the configuration, and each host's current credential in memory. It never holds the credentials a client gives a job. A job and its
container never see any of these.

### 4.1 A store with no STS: prefix grants

**Why.** A store must serve `AssumeRole` for section 4, and the farm's own object store (tierstore) has none; neither has Google Cloud
Storage's interoperability API. With `credentials = "presigned_prefix"` the scheduler mints without the store: it derives each host's
credential from a signing key the store also holds, and the store checks it. Nothing else changes: one credential per host, the same
scope (the host's prefix, put and delete only), the same lifetime, renewal, delivery and failures (sections 4 and 5).

**The grant.** For a host, the scheduler builds the token

```text
jspp1:<signing key id>:<bucket>:<expiry>:<host prefix>
```

where the expiry is the scheduler's clock now plus `credential_ttl`, in whole seconds since the Unix epoch (cut down, never rounded
up), and the host prefix is `LH/` (section 2), the same the STS session policy names. The host's credential is:

- the session token: the token;
- the secret access key: the lower-case hex HMAC-SHA256 of the token under the signing key's secret;
- the access key id: `JSPP` and the first 32 hex digits of the token's SHA-256;
- the expiry: the token's.

The host never sees the signing secret. Changing the prefix, the bucket or the expiry changes the token, and so the secret it would
need. A mint makes no call, so it fails only for a request no grant can be built for, which the checks at start rule out.

**What the store checks**, for every request signed with such a credential, in either Signature Version 4 form (the agent's headers,
or a presigned URL whose query carries the signature): the token parses; its signing key is one the store holds for grants; the access
key id is the one the token gives; the signature is made with the secret derived from the token; the bucket is the token's; the time is
before the token's expiry (and, for a presigned URL, before its own `X-Amz-Expires`); and the call is a put or a delete of a key under
the token's prefix whose rest is plain segments (no empty, `.` or `..` segment). Anything else is refused. The signing key itself signs
no request.

**The signer.** `js-logstore`'s `PrefixSigner` presigns puts and deletes with a grant's credential, and refuses, before any request
exists, a key outside the prefix, a key whose rest is not plain segments, and any time at or after the expiry. The fake store of the
tests checks grants as above, so the minter, the signer and the agent's own client are tested against it.

**What the operator configures.** `signing_key_file`: an owner-only file holding the signing key, the same JSON as the other key files.
The same key must be configured in the store as a grant signing key; the read key and the lifecycle rule are as in section 4. Checking
grants is the store's work: tierstore's support for it, and the test against the real tierstore, belong to the tierstore side and are
not part of this repository.

## 5. Failures

| Failure | What happens | Who sees what |
|---|---|---|
| The store is unreachable or refuses a put while the job runs | The uploader retries the same chunk with backoff; the capture file keeps filling up to its cap. When the store answers again the stream goes out in order from `next`. Bytes past the cap were discarded by the capture and become a gap marker written at the end. The job is never blocked: the uploader reads files and takes no lock the capture or the worker takes. | A follower waits and then catches up; a reader sees a gap only for dropped bytes. |
| The store is unreachable at the end | The result is reported at once with the lengths. The agent keeps trying to write the remaining chunks, the marker and the finals for `FINISH_BUDGET` (5 minutes), never with an expired credential, then gives up and leaves the chunks. | The reader reads the chunks; a range never put is a gap after `LOG_SETTLE`. |
| The host credential is refused or has expired | The agent reports `log_grant_state.stale` in its next heartbeat and goes on reading from the files. The scheduler mints a new one on its next tick and resends it until the agent reports it; every uploader on the host switches to it. | A delay, no loss, unless the outage outlasts the cap. |
| The scheduler cannot mint for a host (at registration or at renewal) | The host's jobs start and run as usual, with their sink; their uploaders wait as for a store that is down (the capture file is the backlog). The mint is retried with backoff (5 s doubling to 5 minutes) until it succeeds, and never given up for good. No start waits for a mint (D5). | Live logs of that host lag; the result's tails are unaffected. |
| The agent restarts while a job runs | The capture goes on in the supervisor and writes its files. The restarted agent adopts the job and resumes the upload from its progress record, uploading what was written meanwhile. Without a record it has no sink for the job and uploads nothing more for it (3.5). | With a record, nothing is lost. Without one, the rest of the stream after the last chunk put is a gap once the job ended `LOG_SETTLE` ago. |
| The scheduler restarts | It forgets every host credential (they are in memory only) and mints them again on each agent's next beat. An agent keeps using the one it holds until it expires or a new one arrives. | Nothing, unless a mint fails (row above). |
| A stream is longer than the local cap | The capture keeps the first `file_cap` bytes; the rest is dropped and never uploaded. The agent writes a gap marker for it at the end and no final (D3). | The result's `logs.*_dropped`; a gap at the end of the stream. |
| A stream fits the cap but is longer than `MAX_PUT_BYTES` | No final; the chunks stay. | The reader reads the chunks. |
| The reader finds a chunk missing after the job ended | After `LOG_SETTLE` the missing range is reported as a gap. | A gap in `GetLogs`. |
| A log field the agent or the server cannot use | Ignored: a malformed `log_sink` is dropped by the agent and the job starts without live logs; a malformed `log_grant` is dropped and the agent reports `stale`; an inconsistent `logs` report (a dropped count above its length) is dropped by the server; a malformed `log_grant_state` reads as absent. A log field never refuses a job or a heartbeat. | Live logs missing for that job or host; everything else as usual. |
| The job's id does not qualify for a prefix (section 2) | No sink is sent; the job runs without live logs. | `GetLogs` with a stream answers `no-live-logs` (section 8). |

## 6. Configuration

`[logs]` is a table of the server's `--config` file, beside `[server]`, and has no flags: a server run from flags has no sink. An
unknown key is refused. The example is in `configuration.md`, "Live logs". The keys:

| Key | Type | Default | Bound and rule |
|---|---|---|---|
| `sink` | string | `"none"` | `"object_store"` or `"none"`. With `"none"`, any other key present is refused, naming the key. |
| `url` | string | none | Required with `object_store`. `s3://BUCKET` or `s3://BUCKET/prefix`; the bucket 3 to 63 bytes of lower-case letters, digits, `.` and `-`; the prefix as section 2 says, at most 476 bytes so that a host's prefix fits under it. |
| `region` | string | none | Required with `object_store`. 1 to 64 bytes of `[a-z0-9-]`. Signs every request. |
| `endpoint` | string | none | Required with `object_store`: the server names no cloud's host by default. At most 512 bytes; `https`, or `http` to a loopback address; no path, query or user. The agent puts here and the server reads here. |
| `path_style` | bool | `false` | Optional. `true` for stores that need `/bucket/key` URLs. |
| `read_key_file` | path | none | Required with `object_store`. An owner-only file holding JSON `{"access_key_id": ..., "secret_access_key": ...}`: the read key of section 4 (list and get on the prefix). |
| `credentials` | string | `"sts"` | How a host's credential is minted: `"sts"` (section 4) or `"presigned_prefix"` (4.1). Anything else is refused, naming the key. The keys of the other kind are refused, naming the key. |
| `sts_endpoint` | string | `endpoint` | Optional with `sts`. The same URL rule as `endpoint`. |
| `role_arn` | string | none | Required with `sts`. An `arn:` value of at most 2,048 bytes: the role `AssumeRole` assumes (section 4). |
| `mint_key_file` | path | none | Required with `sts`. An owner-only file, the same JSON as `read_key_file`: the key that calls `AssumeRole`. |
| `signing_key_file` | path | none | Required with `presigned_prefix`. An owner-only file, the same JSON as `read_key_file`: the key grants are derived from (4.1). |
| `credential_ttl` | duration | `"1h"` | From `"15m"` to `"12h"`, for either kind. With `sts`, at most the role's maximum session duration, which the server cannot check at start: a longer one is refused by STS and reads as a mint failure. |
| `max_follow` | integer | 256 | From 1 to 4,096. The most `GetLogs` calls with `follow` waiting at once (section 8). |

Keys are files, never inline secrets, and the files must be owner-only, the rule every key file of the server follows (read 0600 or
0400, owned by the service user). The files are read once at start, like the other key files; nothing is checked over the network at
start, so a wrong key, role or endpoint shows as a mint failure or a refused read, not as a start failure.

The agent has one new flag, `--log-file-cap` (section 9): the local cap of each stream of a job with a sink.

## 7. The wire (additive, major 1)

The fields are added to `js-agent-wire` in protocol major 1, announced by the hello capability **`log-upload`**. An agent reports it
only when its runtime uploads (the Linux `crun` runtime and the macOS native runtime, D6; a native agent always reports it). The server checks it before it sends any of the fields below: an agent
that did not report it gets no `log_sink` and no `log_grant`. Each field is optional and absent when empty, and an older receiver
ignores it (the compatibility rule of `agent-wire.md`, section 1).

| Message | Field | Type | Meaning |
|---|---|---|---|
| `JobStartSpec` | `log_sink` | optional `LogSink` | Where, and whether, to upload this job's logs. No secret. |
| `LogSink` | `endpoint` | string | The store's URL (the `endpoint` rule of section 6), at most 512 bytes. |
| | `region` | string | The `region` key. |
| | `bucket` | string | The bucket of `url`. |
| | `prefix` | string | The job's prefix `P` (section 2), ending in `/`, at most 577 bytes (512 + `/` + 63 + `/`). |
| | `path_style` | bool | The `path_style` key; absent means `false`. |
| `HeartbeatReply` | `log_grant` | optional `LogGrant` | The host's credential (section 4); resent until reported held. |
| `LogGrant` | `access_key_id` | string | At most 128 bytes. |
| | `secret_access_key` | string | At most 128 bytes. Secret. |
| | `session_token` | optional string | At most 4,096 bytes. Secret. |
| | `expires_at_ms` | u64 | The expiry, milliseconds since the Unix epoch. |
| `Heartbeat` | `log_grant_state` | optional `LogGrantState` | Sent by an agent that reported `log-upload`. |
| `LogGrantState` | `stale` | bool | The agent asks for a new credential: it holds none, or the store refused the one it holds. |
| | `until_ms` | optional u64 | The `expires_at_ms` of the credential the agent holds; the server resends its credential until this equals it. |
| `JobResult` | `logs` | optional `LogReport` | What the capture saw of each stream; absent from a runtime that does not upload. |
| `LogReport` | `stdout_bytes`, `stderr_bytes` | u64 | Each stream's length: what the job printed. |
| | `stdout_dropped`, `stderr_dropped` | u64 | The bytes of each stream the capture could not keep; never uploaded. |

**Leniency.** A log field never refuses a job or a heartbeat: each is decoded as `load` is (a malformed value reads as absent), so a
malformed `log_sink` leaves the job to start without live logs, a malformed `log_grant` leaves the agent to report `stale`, and a
`logs` report whose dropped count exceeds its length is ignored by the server. **Secrets** (`secret_access_key`, `session_token`) never
appear in `Debug` output or in any log line.

**Size.** In the heartbeat (the 65,536-byte body), a `logs` report adds at most 157 bytes of JSON to a result and `log_grant_state` at
most 66 to the beat. A `log_grant` adds at most 4,464 bytes to a reply, under the 1 MiB the agent reads, and a `log_sink` at most 1,298
to a start. The arithmetic is in `agent-wire.md`, section 2, "The live-log fields".

**Tests the wire change carries** (the rule of `agent-wire.md`, section 4): goldens of each new field; a previous release's agent reading
a reply and a start with the new fields, and the new server reading a previous agent's heartbeat and result without them; the leniency
cases above; and `Debug` of a grant asserted whole, by exact equality, to show no secret.

## 8. `GetLogs`

`GetLogs` stays one unary call (D2). The service definition's evolution rule (`crates/js-proto/proto/jobscheduler/v1/service.proto`)
forbids changing a shipped method's shape, so `follow` is a bounded long poll, never a stream: the server waits up to a fixed cap for
new bytes, then answers, and the client calls again with the `next_offset` it was given. The additions are new fields, so a request
without them means what it meant before.

**Request** (`GetLogsRequest`, beside `job_id`):

| Field | Type | Default | Bound and meaning |
|---|---|---|---|
| `stream` | enum `LogStream`: `UNSPECIFIED`, `STDOUT`, `STDERR` | `UNSPECIFIED` | `UNSPECIFIED` is the legacy answer: the tails, as before. |
| `from_offset` | u64 | 0 | The offset of the first byte wanted. |
| `max_bytes` | optional u32 | 262,144 (256 KiB) | At most 1,048,576 (1 MiB): a larger value is cut to it; 0 is refused. |
| `follow` | bool | `false` | Wait for new bytes, up to `MAX_FOLLOW_WAIT`, when none are there yet. |

**Response** (`GetLogsResponse`, beside `state` and the tails):

| Field | Type | Meaning |
|---|---|---|
| `data` | bytes | Contiguous bytes of the stream, at most `max_bytes`. |
| `offset` | u64 | The offset of the first byte of `data`: above `from_offset` when a gap was skipped. |
| `next_offset` | u64 | `offset` + the length of `data`: where the next call starts. |
| `gaps` | repeated `LogGap {offset: u64, length: u64}` | The gaps between `from_offset` and `offset`, in order. |
| `eof` | bool | The job ended and `next_offset` is the stream's length. |
| `first_byte_at_ms` | optional u64 | The `x-amz-meta-first-byte-at-ms` of the chunk holding `offset`; absent when a final served it. |

**Answers.**

- **Without a stream** the call answers the tails as before: the result's tails once the job has ended; empty tails and the job's state
  while it runs. It is never an error and never reads the store. The server holds no tail of a running job: a client that wants a
  running job's output asks for a stream.
- **With a stream** the server reads the object store as 3.6 says and answers one page. With `follow` and no new bytes, it waits,
  listing the stream again every `FOLLOW_POLL` (1 s), until new bytes appear, the job ends, or `MAX_FOLLOW_WAIT` (20 s) passes, and then
  answers (an empty `data` at the cap). A job that has ended answers at once. A queued job answers an empty page with `eof` false.
- **Refusals:** an unknown job, or one whose record was swept, is `NOT_FOUND` `unknown-job` whatever the store still holds (D4); a
  server with `sink = "none"` is `FAILED_PRECONDITION` `no-log-sink`; a job whose start carried no sink (the server had none when it was
  placed, its host did not report `log-upload`, or its id does not qualify) is `FAILED_PRECONDITION` `no-live-logs`, and its tails are
  in `GetResult`; `max_bytes` 0 is `INVALID_ARGUMENT`; a `from_offset` past the length of an ended job's stream is `OUT_OF_RANGE`; more
  than `max_follow` calls already waiting is `RESOURCE_EXHAUSTED` `too-many-follows`, which a client retries after a backoff; a store the
  server cannot read is `UNAVAILABLE` `log-store-unavailable`.

**The bytes pass through the service.** The server reads them from the store with its read key and returns them in bounded pages; it
stores none and keeps none after the answer. A client never needs a store credential.

## 9. Limits

| Limit | Value | What it bounds |
|---|---|---|
| `MAX_CHUNK` | 1,048,576 bytes (1 MiB) | one chunk |
| `LOG_FLUSH` | 2 s | how long a waiting byte waits for its chunk |
| `LOG_POLL` | 250 ms | how often the uploader reads the capture files' sizes |
| `--log-file-cap` | default 64 MiB, from 1 MiB to 1 GiB | the local file of each stream of a job with a sink, and so its live coverage; a job without a sink keeps 1 MiB |
| `MAX_PUT_BYTES` | 64 MiB | a final file, written in one put |
| `FINISH_BUDGET` | 5 minutes from the job's end | the end's uploads (3.4) |
| `LOG_SETTLE` | 30 s | when a hole is reported as a gap (3.6) |
| `MAX_FOLLOW_WAIT` | 20 s | one `GetLogs` long poll |
| `FOLLOW_POLL` | 1 s | how often a waiting `GetLogs` lists the stream |
| `max_bytes` | default 256 KiB, at most 1 MiB | one `GetLogs` page |
| `max_follow` | default 256, from 1 to 4,096 | `GetLogs` calls waiting at once |
| `credential_ttl` | default 1 h, from 15 minutes to 12 h | one host credential; renewed at half its life |
| result tail | 8 KiB a stream | unchanged |

The capture files are on the host's disk, in the lease directories: at the default cap a job with a sink can hold up to 128 MiB there
(two streams), and a host's worst case is that times the jobs it runs at once. Disk caps are the operator's (the `--log-file-cap` flag).

## 10. Where it is built, and what is later or open

**Crates (Planned, step 31 of design section 12.1).** `js-logstore`, the object-store client (put, delete, list, ranged get, Signature Version
4, `MAX_PUT_BYTES`); `js-logmint`, the `AssumeRole` call and the session policy; the log fields in `js-agent-wire` and `GetLogs`'s in
`js-proto` (with its comment rewritten to the answers of section 8); the
agent's `logs` module (the uploader, the progress record, the end); the server's `job_api/logs` (the reader, the long poll) and its host
credential table. Each new crate is added to `layers.toml` in the change that adds it. Built so far: `js-logstore`, `js-logmint`, the log
fields in `js-agent-wire`, the agent's `logs` module (the uploader, the progress record, the end) for `crun` and the native
runtime (3.7), the server's host credential table, and `GetLogs`'s fields in `js-proto` with the server's `job_api/logs` (the reader
and the long poll).

**Decided: an object store without `AssumeRole`** (design section 13, question 28). The scheduler has a second minting kind,
`credentials = "presigned_prefix"` (4.1): the farm's store (tierstore) has no STS, and a grant derived from a signing key needs no call
to the store. Each host's grant covers the host's prefix alone and expires at the scheduler's clock plus `credential_ttl`; one object
layout, with the host's segment in every job's prefix, serves both kinds (section 2), and the STS session policy is narrowed to the
same host prefix. Checking grants in tierstore, and testing against the real tierstore, are left to the tierstore side.

**Later and open.**

- **Other runtimes (D6).** The container-engine fallback does not report `log-upload` and ignores a sink if one came; its results
  carry no `logs`. Live logs for it are later work. The macOS native runtime uploads (3.7) and reports `log-upload`, but a sink comes
  only in a job start (`JobStartSpec.log_sink`), which the server sends only to a host that reports `per-job`, and a native agent does
  not report it yet: until it does, a native agent holds the host's credential and its runs (lease starts) carry no sink.
- **Who deletes old logs (D4).** The scheduler never deletes log objects: retention of finals, and of the chunks of streams that had no
  final, is the operator's bucket lifecycle rule on the prefix, which must be at least `--job-retention`. A job whose record was swept
  answers `unknown-job`. Whether the scheduler should delete instead is design section 13, question 26.
- **Live log coverage.** A stream is uploaded live only up to the local cap; bytes past it are a gap. The alternatives are a larger
  default, or a capture that frees what was uploaded so the cap bounds the backlog rather than the stream. Design section 13, question 27.
- **Presigned read URLs.** `GetLogs` returns the bytes through the service. An alternative is to return presigned read URLs of the
  objects, so the bytes go straight from the store to the client; it moves the work off the server and hands a client a URL to the
  store. Not chosen for v1.
- **The generated CLI's follow loop.** The `js` CLI is generated from the service definition; a `logs --follow` that calls `GetLogs`
  again with `next_offset` until `eof`, and backs off on `too-many-follows`, is a hand-written loop the generator does not give.
- **Per-chunk timestamps are lost in the final file.** A final has no first-byte times; a reader of an ended job gets none. Keeping them
  (an index object beside the final) is later work if a client needs times after the end.
