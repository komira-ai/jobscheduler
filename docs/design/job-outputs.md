# Job outputs: files a job leaves, read back through the object store

Status: **Built** (this page is the design of what the change that adds it builds).

A job's output reaches a client as its two streams (live logs, `live-logs.md`) and the 8 KiB tails
of its result. A CI step also leaves files: a test report, a built binary, a package. This page is
how a job declares such files, how the host's agent puts them in the object store when the job
ends, and how a client reads them back: `GetOutputs`, the generated `js job outputs`.

## 1. What is decided

- **A job declares its outputs** in `JobSpec.outputs`: paths relative to the job's working
  directory, each naming a file or a directory. The working directory is `/work` in a Linux
  container (the directory a job starts in) and the run's `exec/` directory for a macOS native
  job. A path the job did not make is not an error: it is listed as missing.
- **The agent uploads them, once, after the job's process has ended** and before the files the
  job wrote are removed, to the object store live logs use, under the job's own prefix `P`
  (`{prefix}/{host}/{job}/`, live-logs section 2): a file at `P/outputs/<path>`. The upload uses
  the host's log credential (live-logs section 4), which may already put any key under the host's
  prefix: no new credential, policy or grant.
- **A manifest is written last**, `P/outputs.json`: each file uploaded with its length and
  SHA-256, the declared paths that were missing, and each path that could not be uploaded with
  why. Its presence says the upload is over; its absence, that it is still going on, or that it
  never will (the host went away, or the upload ran out of its time).
- **`GetOutputs(job_id)` reads the manifest** with the server's read key and answers it, with a
  presigned GET URL for each file, valid for one hour, signed with the same read key. A client
  downloads each file straight from the store; no file's bytes pass through the service. The
  generated `js job outputs --job-id ID` prints the answer, `--json` as proto3 JSON.
- **A job with outputs needs the object store.** A server without `[logs] sink = "object_store"`
  refuses its submit, `FAILED_PRECONDITION` `no-log-sink`, as `GetLogs` answers such a server.
  The job requires the host capability `job-outputs`, which an agent reports only with
  `log-upload` (it uploads with that credential); it is placed and started only there: a start
  is not sent to a host whose hello no longer reports it, nor one that would carry no sink. Admission at submit does not look at the
  capability: while no host reports it, such a job waits in the queue and fails to start when
  the pool's queue wait passes.

## 2. The declaration

`repeated string outputs = 62` in `JobSpec`. The service refuses (`INVALID_ARGUMENT`, `field:
spec.outputs[i].path`) a list that breaks any of:

- at most 64 paths;
- each 1 to 512 bytes of segments joined by `/`, each segment 1 to 255 bytes of `[A-Za-z0-9._-]`
  and neither `.` nor `..` (no leading or trailing `/`, no empty segment): a key segment of the
  store under every kind of credential (a presigned-prefix grant checks segments, live-logs 4.1);
- no path twice, and none inside another.

A file inside a declared directory whose name is not such a segment is not uploaded: the manifest
lists it as failed (`name`), as the agent cannot put it.

## 3. The agent

Built in `crates/js-agent/src/outputs` (`gather.rs`, `upload.rs`). When the run is removed, after
the job's process has ended and before what it wrote is removed, the runtime hands the agent the
job's working directory on the host: for `crun`, the writable layer of `/work` in the lease
directory, before the container and the lease directory are removed; for the native runtime,
the run user's `exec/`, before the user and its home are removed. The agent opens each declared
path below it, following no symbolic link at any step (each name looked at with
`AT_SYMLINK_NOFOLLOW`, then opened with `O_NOFOLLOW`, from the working directory down, and what
was opened held to the kind that was looked at):

- a regular file with one link is an output; a file with more links, a symbolic link (as the
  output or as a directory on the way to it), a device, a socket or a pipe is refused
  (`not-a-file`) and never read;
- a directory is walked in the order of its names, and each regular file in it is an output; a
  name that is not an output path's segment is refused (`name`, each byte outside
  `[A-Za-z0-9._-]` shown as `?`); a tree holds at most 16 levels of directories, the declared one
  the first, and a directory past them is refused (`over-limit`);
- at most 256 files and 1 GiB in all; a file over 64 MiB (one put, the store client's limit)
  is refused (`too-large`); a file past the bytes is refused (`over-limit`) and a smaller one
  after it still taken; the file past the files is refused (`over-limit`), not opened, and the
  walk stops there;
- at most 4,096 entries looked at (files, directories and anything else, every declared path
  together): a directory with more names than are left is refused whole (`over-limit`; no more
  than one name past them is read), and once they are spent each further declared path is
  refused (`over-limit`);
- at most 256 failures listed; the one past them is listed `over-limit` and the rest are not.
  With the 256 files and the 64 declared paths, the manifest stays far under the 1 MiB a reader
  takes;
- what cannot be looked at, opened or listed is refused (`read`); a declaration the store's rules
  refuse (which the server checks at submit) fails every path (`path`).

The files are opened before the job's files are removed and read from the open descriptors after,
so the removal never waits for the upload and never cuts it short (as the log uploader holds its
capture files, live-logs 3.5); the 256 files bound the descriptors one job's upload holds. One
thread per job puts the files in the order of the declaration, each in one put, then the
manifest. Each try reads the file again from its first byte, so no more than one try's bytes
(64 MiB at most) are held; the length and SHA-256 of the first read are kept, and a file whose
bytes differ at a later try is failed (`read`). With no
credential held, the upload waits; a put the store answers `Unavailable` is tried again after 1 s,
doubling to 60 s; one answered `Expired` waits for the host's next credential (or the same
backoff); one refused otherwise (`Denied`, `NotFound`, `Invalid`) is not tried again and the file
is listed failed (`store`), and a refused manifest ends the upload with none. The whole upload has
15 minutes from the run's removal; past them nothing more is put, and no manifest is written. An
agent restarted while it uploads does not take the upload up again: the job's outputs then have no
manifest.

The manifest (`P/outputs.json`, JSON, at most 1 MiB):

```json
{"version": 1,
 "files": [{"path": "out/report.json", "bytes": 1534, "sha256": "<64 hex>"}],
 "missing": ["coverage.lcov"],
 "failed": [{"path": "out/core", "reason": "too-large"}]}
```

A reader ignores fields it does not know.

## 4. The read

`rpc GetOutputs(GetOutputsRequest) returns (GetOutputsResponse)`, `GET /v1/jobs/{job_id}/outputs`,
a read of the job's own (role `client`: its own jobs; `operator`: any). It answers:

- `unknown-job`, `expired` as `GetLogs` does;
- `FAILED_PRECONDITION` `no-log-sink` on a server without the object store, and `no-outputs` for a
  job that declared none (or whose id gives no object prefix, so it never had a sink);
- `UNAVAILABLE` `log-store-unavailable` when the store cannot be read (nothing of the store's
  answer is passed on);
- otherwise the job's state, `ready` (the manifest was found), and from the manifest the files
  (path, bytes, SHA-256, URL), the missing paths and the failures, with the time the URLs expire.
  Before the job has ended, or while its upload goes on, `ready` is false and the lists are empty;
  a client asks again.

A manifest the server cannot read (not JSON, an unknown version, a path that is not a declared
output or inside one, over 1 MiB) is answered `UNAVAILABLE` `outputs-unreadable`, and the server
logs why.

## 5. Compatibility (B8)

Every change is additive: a new optional field of `JobSpec` (62) and of the job store's row (a new
tag, absent in an old row), a new call, a new optional field of the agent's job start (`outputs`,
left out when empty), a new host capability and new object keys under a job's prefix. An agent
that predates the field would ignore it, so the server sends a job with outputs only to a host
that reports `job-outputs`. A server that predates the call answers it `UNIMPLEMENTED`.

## 6. Later and open

- Files over 64 MiB need a multipart or streamed put (the store client has neither).
- An agent restart during the upload loses it (no progress record of outputs).
- Outputs of a job that runs in the container-engine fallback (no `log-upload`) are not built.
- A macOS job's outputs need native job starts (komira-ai/jobscheduler#888): the native runtime
  uploads the outputs of a job start, but a native agent does not take job starts yet.
