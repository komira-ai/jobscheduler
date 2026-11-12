# A job over a FUSE file system, end to end, on one Linux node

This is build item 8 of [`docs/design/job-rootfs.md`](../design/job-rootfs.md): one `crun` node runs one job whose
files come from a lazy FUSE file system, `tierstore fs`, in each of the ways a job can use one:

- a **layer** of the job's root from a FUSE host path (read-only; the job's writes copy up into its own scratch);
- a **`COPY_ON_WRITE` mount** whose lower is a FUSE host path (the same: reads come from FUSE, writes stay in scratch);
- a **`READ_WRITE` mount** of a FUSE directory whose writes are the file system's own (tierstore's put directory, where
  a `rename` is the upload of an object);

with the image pulled from a registry on `127.0.0.1` and an upload to a service on a host port at network level `none`.
It is a runbook, run by hand on a node, not a test of the pull request set (B9): it needs root, a kernel FUSE mount and
`crun`, and it measures throughput.

Each check below is marked as `first-live-launch.md` marks them:

- **CI**: a test in this repository proves it, against fakes or in the `crun-root` job's root suites. It says nothing
  about FUSE or about this node.
- **LIVE**: only this run proves it. Nothing in CI covers it.

Examples use `jsjob` as the job user, `e2e` as the pool, and `<work>`, a directory of the operator's. Replace them.

## 1. The rule this run depends on: `fs.owner` is the job's host uid (LR5)

**The tree's `fs.owner` must be the host ids a job runs as: `sub_uid + 1000` and `sub_gid + 1000`, where `sub_uid` and
`sub_gid` are the first ids of the job user's subordinate ranges.** On this node that is
`fs.owner = "<sub_uid + 1000>:<sub_gid + 1000>"` in `tierstore fs`'s `[fs]` table.

Why, in the order a job meets it:

- A job runs as id 1000 of its own user namespace, which the agent maps to the job user's subordinate range (id 0 to
  `sub_uid`, id 1000 to `sub_uid + 1000`; `Oci::container_ids`, `crates/js-contain/src/oci/lease.rs`). The job user's
  own uid is not mapped in that namespace.
- `tierstore fs` presents every file and directory of a tree, and every directory and file of its put directory, as
  owned by `fs.owner`, and it mounts with `default_permissions`, so the kernel checks those modes against the caller's
  host ids. A tree's directories are 0755 and its files 0644 (0755 when executable); a put directory is 0700.
- An overlay (the layered root and a copy-on-write mount) lets a job write over a lower directory only where the job
  has write permission on it as the lower presents it. Owned by `sub_uid + 1000`, the tree's directories are the job's,
  so it can create, change and delete in them (each change copied up). Owned by anyone else, they read as owned by
  65534 inside the job, and every write below them fails `EACCES`.
- A put directory is mode 0700. Only its owner (or root) can create a file in it. Owned by `sub_uid + 1000`, it is the
  job's; owned by anyone else, the job cannot write its outputs.
- The agent checks, at assembly, that each mount's source can be searched by the job's ids (`job-rootfs.md` 4.2).
  `crun` binds the source inside the job's user namespace, where `sub_uid + 1000` is mapped. **CI**
  (`crun::fs::assemble_tests::a_mounts_source_is_searched_as_the_job_not_the_job_user` in `js-agent`;
  `job_fs::a_private_source_owned_by_the_job_is_bound_and_written` in `js-contain`'s `crun_root` suite, where a
  read-write and a copy-on-write mount of a 0700 directory owned by `sub_uid + 1000` are bound and written by the job).

The agent never changes the ownership of a host path (decision LR5), and it has no id-mapped mounts yet (build item 14
of `job-rootfs.md`). So the provider presents the ids, and one `tierstore fs` serves the jobs of one job user.

To read the two numbers, take the first line of `/etc/subuid` (and of `/etc/subgid`) whose owner is the job user, by
name or by uid, with a count of at least 65536. That is the line the agent takes (`first_big_range`,
`crates/js-contain/src/subids.rs`):

```text
grep '^jsjob:' /etc/subuid /etc/subgid
/etc/subuid:296608:65536
/etc/subgid:296608:65536
```

Here `fs.owner = "297608:297608"`. While a job runs, `stat -c '%u:%g'` of its scratch directory
(`/var/lib/js-agent/scratch/runs/<run>`) prints the same pair.

## 2. What you need

- One Linux node with root, kernel 5.12 or later (6.9 or later for `tierstore fs`'s passthrough mode; without it the
  mount runs in its cached mode, which is enough here).
- The node set up for `crun` ([crun-host-setup.md](crun-host-setup.md)): the job user `jsjob` with its ranges, the
  agent's state and scratch roots, and the `js-agent-crun` unit.
- `tierstore` on the node: a storage node with a cache (`[hot_store]`, `[cold_store]` and `[local_cache]` of
  directories is enough; tierstore's `docs/examples/cache-hot-cold.md`), whose S3 front listens on `127.0.0.1`, and
  `tierstore fs` beside it under its `tierstore-fs@a` unit (tierstore's `crates/tierstore/docs/operations.md`).
- An OCI registry listening on `127.0.0.1:<registry port>`, holding a small image with `/bin/sh`, `cat`, `head`, `mv`
  and `nc` (a busybox image), pushed as `e2e/busybox` and known by its digest.
- An S3 client for the tierstore front (`aws` with path-style addressing, as tierstore's `docs/clients.md` shows), and
  `socat` and `pv` on the host for the upload's receiver.

## 3. Set up

### 3.1 `tierstore fs`

`/etc/tierstore/fs.toml`, with the owner of section 1:

```toml
[fs]
client_group = "tierstore"
owner = "297608:297608"
fill_socket = "/var/lib/tierstore/fill.sock"
cache_path = "/var/cache/tierstore"

[fs.put]
enabled = true
prefixes = ["cas/b/"]
stage_dir = "/var/cache/tierstore/put-stage"
```

`fill_socket` and `cache_path` are the node tierstore's `local_cache.fill_socket` and `local_cache.path`. The mount is
at `/run/tierstore/fs` (the default `fs.mountpoint`), with `allow_other`, which tierstore always sets: the agent reads
the trees as root, and the job reads and writes as `sub_uid + 1000`. Start it (`systemctl start tierstore-fs@a`) and
check that `systemctl status tierstore-fs@a` says it is running and that its journal names the mode
(`tierstore fs: kernel offers ... mode=...`).

### 3.2 The tree

A tree is a root manifest and one manifest per directory, each stored as the object `cas/l/<its sha256>`, and each file
with no `source` stored as `cas/l/<sha256 of its bytes>` (tierstore's `crates/tierstore/docs/tree-manifest.md`). This run
uses a tree with one directory, `e2e`, holding three files:

| file | bytes | the job |
|---|---|---|
| `e2e/job.sh` | the job's script (3.5) | runs it |
| `e2e/read.txt` | `read by the job` and a line end | reads it, and changes it through each overlay |
| `e2e/unread.bin` | 64 MiB of anything | never opens it |

Upload each file as `cas/l/<sha256>` (`aws s3api put-object --bucket <bucket> --key cas/l/<sha256> --body <file>`;
the front refuses a body whose digest is not its name). Then write `e2e`'s manifest: one line, children sorted
bytewise by name, no white space:

```text
{"v":1,"children":[{"name":"job.sh","kind":"file","exec":true,"sha256":"<hex>","size":<n>},{"name":"read.txt","kind":"file","sha256":"<hex>","size":16},{"name":"unread.bin","kind":"file","sha256":"<hex>","size":67108864}]}
```

Upload it as `cas/l/<its sha256>`, then the root manifest the same way:

```text
{"v":1,"children":[{"name":"e2e","kind":"dir","sha256":"<sha256 of e2e's manifest>","size":<its bytes>}]}
```

The sha256 of the root manifest's bytes is the tree, `<tree>`. `ls /run/tierstore/fs/trees/<tree>/e2e` as root lists
the three files. The lookup attaches the tree (tierstore's implicit attach). **LIVE**.

### 3.3 The lease server and the agent

One `js-lease-server` on the node, with the job service on the loopback:

```text
js-lease-server --listen 127.0.0.1:7300 --job-listen 127.0.0.1:7302 --boot initialise \
  --state-file <work>/server/state --token-file <work>/client.token \
  --agent-token-file <work>/join.token --agent-pool-config <work>/pool.toml
```

`<work>/pool.toml` is one pool of one machine, whose shape is at most the node's:

```toml
name = "e2e"
class = "burst-host"
max_machines = 1

[shape]
cpu_millis = 4000
memory_bytes = 8589934592

[capabilities]
arch = "x86_64"
```

The agent's flags file, `/etc/js-agent/flags` ([farm-node-enroll.md](farm-node-enroll.md) says its rules), with the
join token in `/etc/js-agent/token`:

```text
--server=http://127.0.0.1:7300
--pool=e2e
--host-id=e2e-node
--mount-root-ro=trees=/run/tierstore/fs/trees
--mount-root=put=/run/tierstore/fs/put
--image-allow-loopback=127.0.0.1:<registry port>/e2e
--host-port=<service port>
```

- `--mount-root-ro` admits the layer and the copy-on-write mount under `trees/` (a copy-on-write mount does not write
  its source, so a read-only root takes it); `--mount-root` admits the read-write mount under `put/`. Each root is
  opened when the agent starts, so start `tierstore fs` first.
- The roots are on the FUSE file system, not on the state root's file system, which the agent refuses.
- `--host-port` is the one port the job may reach on the host; no `--egress` flag is needed, since a host port works at
  network level `none`.

Start the agent (`systemctl start js-agent-crun`). Its journal has no warning that the kernel lacks a call, and
`GET /v1/hosts` on `127.0.0.1:7300`, with the client token, shows the host `live` with
`capabilities."layered-rootfs": "true"`. A job with layers, mounts or host ports is placed only on such a host.
**CI** (`a_job_with_layers_is_started_only_on_a_host_that_reports_layered_rootfs`), **LIVE** (the kernel's calls).

### 3.4 The service on the host port

On the host, a receiver that counts what it is sent and how long it took:

```text
socat -u TCP-LISTEN:<service port>,bind=127.0.0.1,reuseaddr STDOUT | pv -f -b -t -a > /dev/null
```

Start it again before each job; it ends with the connection. `pv` prints the bytes, the time and the average rate.

### 3.5 The job

`e2e/job.sh` (in the tree, so the job's script is itself read through the layer):

```text
set -e
cat /e2e/read.txt
cat /work/read.txt
echo changed > /e2e/read.txt
echo changed > /work/read.txt
echo new > /work/new.txt
cat /e2e/read.txt /work/read.txt /work/new.txt
echo result > /out/result.part
mv /out/result.part /out/result
head -c 1073741824 /dev/zero | nc 127.0.0.1 <service port>
echo done
```

`<work>/layers.json`, the image's tree with the tree over it:

```text
[{"hostPath":"/run/tierstore/fs/trees/<tree>"}]
```

`<work>/mounts.json`, with a nonce of the run's choosing (a new one for each job, since a key is created once):

```text
[{"hostPath":"/run/tierstore/fs/trees/<tree>","subPath":"e2e","target":"/work","mode":"MOUNT_MODE_COPY_ON_WRITE"},
 {"hostPath":"/run/tierstore/fs/put/cas/b/e2e/<nonce>","target":"/out","mode":"MOUNT_MODE_READ_WRITE","noExec":true}]
```

`<work>/host-ports.json`:

```text
[{"hostPort":<service port>}]
```

Submit it and wait for it:

```text
js job run --server http://127.0.0.1:7302 --token-file <work>/client.token --id e2e-1 \
  --spec.image 127.0.0.1:<registry port>/e2e/busybox@sha256:<digest> \
  --spec.layers-file <work>/layers.json --spec.mounts-file <work>/mounts.json \
  --spec.host-ports-file <work>/host-ports.json --spec.network none \
  --spec.command /bin/sh --spec.args /e2e/job.sh --spec.wall-limit-seconds 15m
```

## 4. What to check

Before the job, note which blobs the node's cache holds: `ls /var/cache/tierstore/obj/*/*/` (each blob is named by its
sha256), and the node tierstore's `tierstore_cache_fills_total` on its metrics route.

| Check | How | Mark |
|---|---|---|
| The job succeeds | `js job run` exits 0 (the job succeeded); the job's output is `read by the job` twice, then `changed`, `changed`, `new`, then `done` | **LIVE** |
| The job reads lazily | after the job, the cache holds the blobs of `job.sh` and `read.txt` and not the blob of `unread.bin`; `tierstore_cache_fills_total` rose by the files read and the manifests filled, not by the three files | **LIVE** |
| Writes copy up | `cat /run/tierstore/fs/trees/<tree>/e2e/read.txt` still prints `read by the job`, and no `new.txt` is there: both writes went to the job's own layers | **LIVE**; **CI** for the overlays over plain directories (`job_fs::a_root_of_layers_reads_through_and_keeps_its_writes`, `job_fs::each_mode_writes_where_it_says`) |
| The read-write mount's commit is an upload | `aws s3api get-object --bucket <bucket> --key cas/b/e2e/<nonce>/result <file>` gets `result` and a line end; the `mv` returned only after the upload | **LIVE** |
| The upload reaches the service | the receiver's `pv` says 1.00 GiB | **LIVE** |
| The run's directories are removed | the agent's journal has `js-agent: job: ContainerRemoved`, `ScratchRemoved` and `ChecksPassed` lines for the run; then `ls -A /var/lib/js-agent/scratch/runs` shows neither `<run>` nor `.lease-js-<tag>-<run>`, `ls -A /run/js-agent/mounts` is empty, and `grep -c js-agent/mounts /proc/self/mountinfo` prints 0 | **LIVE**; **CI** (the unmounts and `remove_tree`, `job_fs::remove_tree_does_not_cross_a_bind_of_the_same_file_system`) |
| The integrity check passes | `js-agent: job: ChecksPassed { run_id: "<run>" }` and no `Tainted` or `Finding` line; `GET /v1/hosts` shows no `taint` | **LIVE** |

### The FUSE service restarts while no job runs

With no job running, `systemctl restart tierstore-fs@a`. The unit keeps the FUSE connection in systemd's file
descriptor store, so the mount stays and the new process serves it. Then:

1. `ls /run/tierstore/fs/trees/<tree>/e2e` as root lists the three files.
2. Submit the job again with `--id e2e-2` and a new nonce in `mounts.json`. It succeeds as the first did, and its
   checks pass. **LIVE**.

The agent holds its two roots open from its start. They stay good across a restart that keeps the connection. If
`tierstore fs` instead ends the connection and mounts afresh (its `fs.recover_wait` abort, or a stop and start without
the unit), the roots the agent holds are of the old mount: restart the agent (`systemctl restart js-agent-crun`) while
no job runs.

### The forwarder's throughput (job-rootfs.md Q2)

The receiver's average rate for the 1 GiB upload is the rate through the agent's forwarder (`js-ports`, two threads,
which copy every byte). For comparison, run the same upload from the host straight to the receiver
(`head -c 1073741824 /dev/zero | nc 127.0.0.1 <service port>`). Record both. If the forwarder's rate is short of what
the uploads it is built for need, the next step is `splice(2)` between the two sockets, then host sockets (Q2). Nothing
in CI measures it.

## 5. If something fails

| What you see | What it means |
|---|---|
| `js job get` shows `job.state: failed-to-start` and `job.start_failure: input-unavailable`, and the agent's refusal reads `the job cannot reach the source of /out: Permission denied (os error 13)` | `fs.owner` is not `sub_uid + 1000:sub_gid + 1000` (section 1) |
| The job's write under `/e2e` or `/work` fails `Permission denied` | the same: the tree's directories are not the job's |
| `job.start_failure: agent-refused`, detail `input-not-allowed` | a host path is under no root, or the read-write mount is under `--mount-root-ro` |
| `mv: can't rename '/out/result.part': I/O error` or `Device or resource busy` | the upload did not finish within `fs.put.commit_deadline_secs`; the file stays staged, and the same `mv` may be tried again while the job runs |
| `mv` fails `File exists` | the key was committed before with other bytes: use a new nonce |
| The job fails and its result has `input_failure: input-unreadable` | a source no longer answered after the job ended (`ENOTCONN`, `EIO`, `ESTALE`): `tierstore fs` went away under the job |
| The agent exits at start naming a root | `tierstore fs` was not mounted when the agent started |

## 6. Results

Record each run here: the node's kernel, the agent's and `tierstore`'s versions, each check's outcome, and the two
rates.

| Kernel | Agent | tierstore | Checks | Forwarder rate | Host rate |
|---|---|---|---|---|---|
| (not run yet) | | | | | |
