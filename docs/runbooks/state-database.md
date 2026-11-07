# The state database: role, URL file, backup and restore

This runbook sets up the Postgres store the lease server is given (`--store`, `server.store`), backs it up and restores it, and does
the same for the files the server keeps its state in today. It describes the server as built; where the design goes further, it says
so.

## What is kept where, as built

| Store | What it holds | Opened when |
|---|---|---|
| The Postgres store (`--store <file>` or `server.store`; schema `kbf_state`) | The writer lock (a session-level advisory lock) and the writer generation (one row of `writer_generation`). With agent registration, the host bindings (`hosts/binding/<host id>`, with each host's taint and cordon) and the digests of the join tokens and host credentials (`hosts/join/<digest>`); with an elastic pool, the scaler's records (`acquires/scaler/records`) and the hosts to remove (`acquires/scaler/unremoved`); and the marker of the first start's import (`meta/server-state-imported`); all in the `kv` table. | The setting is given. The server takes the lock before it reads anything else and before it binds a listener. |
| The job store, `<state file>.jobs` (SQLite, owner-only) | Every job and every allocation. | The job service is on (`--job-listen`, `server.job_listen`). |
| The lease file (`--state-file`) and the binding file (`<state file>.agents`) | The leases, the term and the next sequence; which credential and instance each registered host was admitted with. | Always (the binding file, and `<state file>.joins`, with agent registration and no store setting). |

Two limits of the code as built shape everything below:

- **The store holds the server's state but the leases.** With agent registration or an elastic pool, the server keeps its host
  bindings, join tokens, scaler records and hosts to remove in the store. Its first start over an empty store imports
  `<state file>.agents`, `.joins`, `.scaler` and `.scaler.unremoved` once (`docs/design/configuration.md`, "The first start"), and
  leaves them in place; they are stale from then on. The lease file and the job store, `<state file>.jobs`, stay files.
- **The server creates no schema.** The schema is created and migrated by the installer, which is not in this repository. A database
  without the schema is refused when the lock is taken: `--store: the store's writer lock could not be taken: the state schema has no
  writer generation table (it is older than version 2): run the installer to bring it forward`.

## 1. A least-rights role

The server never creates or alters a table. Give it a login role of its own that owns nothing. An administrator creates the role; the
owner of the schema (the role the installer ran as) grants it what the server's statements need:

```sql
-- As an administrator (a role with CREATEROLE):
CREATE ROLE js_lease_server LOGIN PASSWORD '<a long random password>' CONNECTION LIMIT 20;

-- As the schema's owner, in the state database:
REVOKE ALL ON DATABASE <database> FROM PUBLIC;
GRANT CONNECT ON DATABASE <database> TO js_lease_server;
GRANT USAGE ON SCHEMA kbf_state TO js_lease_server;
GRANT SELECT, INSERT, UPDATE, DELETE ON kbf_state.kv, kbf_state.feed, kbf_state.counters TO js_lease_server;
GRANT SELECT, UPDATE ON kbf_state.head, kbf_state.writer_generation TO js_lease_server;
GRANT SELECT ON kbf_state.schema_version TO js_lease_server;
GRANT USAGE ON SEQUENCE kbf_state.rev_seq TO js_lease_server;
```

Why each grant is there:

- `UPDATE` on `writer_generation`: taking the lock runs `UPDATE writer_generation SET generation = generation + 1`, and every commit
  reads the row `FOR SHARE`, which needs `UPDATE` too.
- `UPDATE` on `head`: a commit locks it (`SELECT ... FOR UPDATE`, `LOCK TABLE head IN SHARE UPDATE EXCLUSIVE MODE`) and moves it.
- `SELECT` on `schema_version`: the first connection checks that the schema's version is one this build supports.
- `USAGE` on `rev_seq`: each commit takes its revision with `nextval('rev_seq')`.
- The advisory lock (`pg_try_advisory_lock`) needs no grant. It is scoped to the database, so two schemas in one database share one
  writer lock.

No grant gives `CREATE`, ownership or superuser. A server running as this role cannot change the schema; only the installer, as the
owner, can.

The role may hold at most 8 pooled connections plus the lock's own session per server. A limit of 20 leaves room for a second server
during a cutover; the second server is refused at the lock, but it connects first.

Leave `synchronous_commit` at `on` (or `remote_apply`) for the database and the role. Every session checks it. A store whose session
reports anything else is refused at start, naming `--store`: the store could not be opened, because `synchronous_commit` is not `on`
and an acknowledged write could be lost.

## 2. The database URL, as a file

The server reads the URL from a file, never from an environment variable or the command line. `--store` (or `server.store` in the
configuration file) names the file:

```toml
[server]
store = "/run/credentials/js/store"   # owner-only; one line
```

- **One line**: `postgres://<role>:<password>@<host>:<port>/<database>` (or `postgresql://`). Blank lines and the whitespace around
  the line are ignored; anything else is refused with `<file> must hold one line: postgres://..., postgresql://... or sqlite:<absolute
  path>`. The password is in the URL: no password file is read.
- **Owner-only**: a regular file owned by the server's user with mode `0600` or `0400`. Any other mode or owner is refused at start,
  naming `--store` (or `server.store`).
- **TLS** is chosen by the URL's `sslmode` (`disable`, the default, `prefer`, `require`, `verify-ca`, `verify-full`). The two `verify`
  modes need `sslrootcert=<PEM file>`; the system trust store is never read. Use `verify-full` across any network you do not own.
- **Never printed**: no refusal or log line quotes the file's content. A URL that does not parse is refused as one that does not
  parse (`the connection URL does not parse`), and a refusal by the database is reported by its SQLSTATE alone, such as `the server
  refused (SQLSTATE 28000)`.
- The server identifies itself as `application_name = js-lease-server`, so its sessions can be found in `pg_stat_activity`.

The `sqlite:<absolute path>` form is for tests and single-node development and is accepted only with `--allow-sqlite-store`
(`server.allow_sqlite_store = true`).

## 3. Taking a backup

**The Postgres store.** Take a custom-format dump of the schema as its owner. `pg_dump` reads one snapshot, so the server may run
meanwhile:

```sh
pg_dump --format=custom --schema=kbf_state --file=<backup dir>/state-<n>.dump '<owner URL>'
```

Record the writer generation beside the dump. A restore needs it (step 4):

```sql
SELECT generation FROM kbf_state.writer_generation;
```

**The job store.** Stop the server cleanly (`SIGTERM`), then copy `<state file>.jobs` while no process holds it. A clean stop leaves
no `<state file>.jobs-wal` beside it; if one is there (the server was killed), start and stop the server once before copying. No command in this repository copies it
while the server runs: the library has an online backup (`js_state::sqlite::backup`, which the restore test uses), but no verb calls it.
A file taken while the server runs, by any tool other than SQLite's own backup, is not a backup.

**The lease file and the binding file**, with the server stopped, together with the job store: the three are one state and are
restored together. With a store setting there is no binding file (nor join token, scaler records or unremoved file): their state is
in the Postgres store, and its backup goes with the lease file and the job store. The files the first start imported are left beside
the lease file, stale; restoring them does not restore the store.

Keep the file mode: the job store and the binding file hold job specs and credentials, and the server makes them owner-only (0600).

## 4. Restoring a backup

1. **Stop every server on the store.** `pg_restore --clean` takes locks the running server's commits wait on, and a server that kept
   running after a restore would hold a lock on the restored database while its generation no longer matches.
2. **Read the generation before the restore**, if the database is still readable, and keep the larger of it and the one recorded with
   the dump.
3. **Restore the Postgres store** as the schema's owner. `--clean` drops the schema's objects and creates them again from the dump, with
   their grants:

   ```sh
   pg_restore --clean --if-exists --single-transaction --exit-on-error --dbname='<owner URL>' <backup dir>/state-<n>.dump
   ```

4. **Raise the generation again.** The dump holds the generation it was taken at, which is below the ones the lost boots took. Set it
   back up, to at least the value kept in step 2:

   ```sql
   UPDATE kbf_state.writer_generation SET generation = greatest(generation, <kept generation>) RETURNING generation;
   ```

   The generation fences commits: a store opened by a lock checks, in every commit, that the generation is still the one its lock took.
   A generation that went back could match one a stale process still holds.
5. **Restore the job store.** Delete `<state file>.jobs-wal` and `<state file>.jobs-shm` if they are there (an old write-ahead log
   would otherwise be replayed onto the restored file), copy the backup to `<state file>.jobs`, and make it the server user's with mode
   0600. The server makes the file owner-only again when it opens it.
6. **Restore the lease file and the binding file**, and, before the first boot, set `term` in the restored lease file to at least the
   highest term any lease id has carried (a lease id is `<term>-<sequence>`; the replaced file's `term`, if it is still readable, is
   the highest). A boot takes the term after the file's. No flag does this: a restored lease file whose `term` is below the lost boots'
   would issue `(term, sequence)` ids that stale holders still carry.
7. **Start the server** with `--boot resume`.

## 5. After the restore

What the server does by itself, and what to check:

- **The start.** With a Postgres store the log says `the store is open and its writer lock is held` once. A second process is refused
  with `--store: another process holds the store's writer lock` and exits 2.
- **`/healthz` is not a lock check.** It answers 200 while the tick loop runs and says nothing about the writer lock: a server that has
  not taken the lock has not started, and one that loses it stops serving and ticking (its listeners close, so `/healthz` is refused
  rather than answered), logs `the store's writer lock was lost, so the server stopped: ...` and exits 1. There is no readiness route. Do not read a 200 as "this server holds the
  lock"; read the log line, or the lock in `pg_locks` (an advisory lock held by a session whose `application_name` is
  `js-lease-server`).
- **Agents come back.** Their first heartbeat is answered 409 `unknown-host`; they say hello again and list the jobs they hold.
- **Jobs the restored store does not know are stopped.** A host that lists a job the restored job store has no live allocation for on
  that host (one accepted after the backup) is told to stop it, on every heartbeat while it lists the job; the job's result is
  acknowledged `STALE`, no row is made for it, and it is never revived. Such a host takes no new start until a listing of it is clean.
  A job the restored store holds, on the host its allocation names, keeps its state and its allocation.
- **Nothing is booked twice.** Placement rebuilds every host's bookings from the restored allocation table before it places, so a
  host whose restored allocation is live is not placed on beyond it, and a host still running a forgotten job is not counted until it
  stops listing it.
- **No id is issued again.** Minted job and allocation ids are 128 bits drawn from the system's random source, never from the store,
  so a restore cannot wind them back; a job id drawn that names a job the restored store holds is drawn again. A client-chosen job id that the
  restore forgot can be submitted again: that is how a client resubmits a job accepted after the backup. Jobs a client accepted after
  the backup read 404 until resubmitted.
- **Leases.** A client's renew of a lease the restored lease file does not hold fails 404 `unknown-lease`, so the client leases
  again; a run a host lists that the server has no record of is answered `Cancel`.

The test that restores an older job store under live agents and asserts the stop, the bookings and the ids is
`restart::a_restored_older_job_store_stops_unknown_jobs_and_reissues_no_id` in `crates/js-lease-server/tests/server/restart.rs`.

## Not built yet

- A server that waits for the writer lock instead of refusing to start, and a readiness route that stays 503 until the lock is taken.
  Until then, during an update cutover or a node reboot, a server refused at the lock stays down until it is started again.
- The rest of the server state in the Postgres store. Until it moves there, the backup that matters for a server that runs jobs is the
  job store, the lease file and the binding file (or, with a store setting, the store that holds the bindings and the scaler's
  records).
- A restore verb, a term floor flag and a check of a restored file pair before boot; steps 4 to 6 of section 4 are done by hand.
