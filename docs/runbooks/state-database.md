# The state database: role, URL file, backup and restore

This runbook sets up the Postgres store the lease server is given (`--store`, `server.store`), backs it up and restores it, and does
the same for the files the server keeps its state in today. It describes the server as built; where the design goes further, it says
so.

## What is kept where, as built

| Store | What it holds | Opened when |
|---|---|---|
| The Postgres store (`--store <file>` or `server.store`; schema `kbf_state`) | No lock and no generation: any number of servers write it at once. (The table `writer_generation` from schema version 2 stays in the schema and is not read or written.) With agent registration, the host bindings (`hosts/binding/<host id>`, with each host's taint and cordon) and the digests of the join tokens and host credentials (`hosts/join/<digest>`); with an elastic pool, the scaler's records (`acquires/scaler/records`) and the hosts to remove (`acquires/scaler/unremoved`); and the marker of the first start's import (`meta/server-state-imported`); with the job service on, every job, allocation, mark, taint and the change feed (`job/`, `alloc/`, `mark/`, `taint/`, `idx/` and the feed rows; schema marker `meta/jobs/schema`), and the delivery of each job's completion callback (`callback/`, which holds the callback's signing key until its delivery ends); with usage on, the usage rows (`usage/`); all in the `kv` table. | The setting is given. The server opens it before it reads anything else and before it binds a listener. |
| The job store, `<state file>.jobs` (SQLite, owner-only), only without a store setting | Every job and every allocation. With a store setting the server does not make, open or read the file; one an earlier start left stays where it was, as a backup. `js-lease-server job-store copy` copies it into the store, and `job-store export` makes one from the store (section 6). | The job service is on (`--job-listen`, `server.job_listen`) and there is no store setting. |
| The binding file (`<state file>.agents`) | Which credential and instance each registered host was admitted with. There is no lease file: the server never opens one, and a lease file left by an earlier release stays as it was. | Always (the binding file, and `<state file>.joins`, with agent registration and no store setting). |

Two limits of the code as built shape everything below:

- **The store holds the server's host state.** With agent registration or an elastic pool, the server keeps its host
  bindings, join tokens, scaler records and hosts to remove in the store. Its first start over an empty store imports
  `<state file>.agents`, `.joins`, `.scaler` and `.scaler.unremoved` once (`docs/design/configuration.md`, "The first start"), and
  leaves them in place; they are stale from then on. The job store and the usage rows are in the store too, from their first start over it: the server's start copies nothing from `<state file>.jobs` or `usage.db`, and the files stay as a backup. Section 6 copies a job store's file into the store before that first start, and exports the store's job and usage rows back to files for a rollback.
- **The server creates no schema.** The schema is created and migrated by the installer, which is not in this repository. A database
  without the schema is refused at start: `--store: the store could not be opened: ...`, with the reason.

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
GRANT SELECT, UPDATE ON kbf_state.head TO js_lease_server;
GRANT SELECT ON kbf_state.schema_version TO js_lease_server;
GRANT USAGE ON SEQUENCE kbf_state.rev_seq TO js_lease_server;
```

Why each grant is there:

- `UPDATE` on `head`: a commit that writes locks it as its last step (`LOCK TABLE head IN SHARE UPDATE EXCLUSIVE MODE`, then `UPDATE head`) and moves it; a placement's decision locks it first.
- `INSERT`, `UPDATE` and `DELETE` on `kv`: besides the writes themselves, a transaction reserves a key that has no row with an uncommitted placeholder row and locks a key that has one with `INSERT ... ON CONFLICT DO UPDATE ... WHERE false`; it deletes its placeholders before it commits.
- `SELECT` on `schema_version`: the first connection checks that the schema's version is one this build supports.
- `USAGE` on `rev_seq`: each commit takes its revision with `nextval('rev_seq')`.
- The server takes no advisory lock.

No grant gives `CREATE`, ownership or superuser. A server running as this role cannot change the schema; only the installer, as the
owner, can.

The role may hold at most 8 pooled connections plus the lock's own session per server. A call that finds all 8 in use waits up
to 10 s for one and then fails as busy (the job API answers `UNAVAILABLE`, 503), and a statement past `statement_timeout`
is busy too: neither wrote anything. A limit of 20 leaves room for a second server
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

**The job store, with a store setting,** is in the Postgres dump above; there is no file to copy. To have it as the SQLite files a server without the setting runs on, export it (section 6).

**The job store, without a store setting.** Stop the server cleanly (`SIGTERM`), then copy `<state file>.jobs` while no process holds it. A clean stop leaves
no `<state file>.jobs-wal` beside it; if one is there (the server was killed), start and stop the server once before copying. No command in this repository copies it
while the server runs: the library has an online backup (`js_state::sqlite::backup`, which the restore test uses), but no verb calls it.
A file taken while the server runs, by any tool other than SQLite's own backup, is not a backup.

**The binding file**, with the server stopped, together with the job store: the two are one state and are
restored together. With a store setting there is no binding file (nor join token, scaler records or unremoved file): their state is
in the Postgres store, and its backup goes with the job store. The files the first start imported are left beside
the job store, stale; restoring them does not restore the store.

Keep the file mode: the job store and the binding file hold job specs and credentials, and the server makes them owner-only (0600).

## 4. Restoring a backup

1. **Stop every server on the store.** `pg_restore --clean` takes locks the running servers' commits wait on, and a server that kept
   running through a restore would write into the restored database from its own picture of the old one.
2. **Restore the Postgres store** as the schema's owner. `--clean` drops the schema's objects and creates them again from the dump, with
   their grants:

   ```sh
   pg_restore --clean --if-exists --single-transaction --exit-on-error --dbname='<owner URL>' <backup dir>/state-<n>.dump
   ```

3. **Restore the job store (only without a store setting; with one the restore in step 2 restored it).** Delete `<state file>.jobs-wal` and `<state file>.jobs-shm` if they are there (an old write-ahead log
   would otherwise be replayed onto the restored file), copy the backup to `<state file>.jobs`, and make it the server user's with mode
   0600. The server makes the file owner-only again when it opens it.
4. **Restore the binding file.** There is no term to set: the server issues no `(term, sequence)` id, and a lease file in the restored directory is ignored.
5. **Start the server** with `--boot resume`.

## 5. After the restore

What the server does by itself, and what to check:

- **The start.** With a Postgres store the log says `the store is open` once, and the server serves at once, whatever other servers
  run on the store: there is no lock to take and none to wait for, so `GET /readyz` on the `--ready-listen` address answers 503
  `starting` while it binds its listeners, 200 `ready` while it serves, and 503 `draining` from SIGTERM until it exits. (A SQLite store
  is the exception: its file lock refuses a second process, or, with `--ready-listen`, is waited for, with 503 `waiting for the
  store's writer lock`.)
- **`/healthz`** answers 200 while the tick loop runs and the store answers. Two servers on one store both answer 200; which one placed
  a job is in the job's own record.
- **Agents come back.** Their first heartbeat is answered 409 `unknown-host`; they say hello again and list the jobs they hold.
- **Jobs the restored store does not know are kept running, not stopped.** A host that lists a job the restored job store holds no
  record of (one accepted after the backup) is told to stop nothing for it: each heartbeat reply names it in `unknown_jobs`, the host's row
  keeps it as an orphan with the time it was first listed, and the job runs on until it ends on the host (its result is acknowledged
  `STALE`, no row is made for it, and it is never revived), until it has been listed for longer than `--max-job-wall`, or until an
  operator ends it (`docs/runbooks/orphan-jobs.md`). Such a host takes no new start until a listing of it is clean, and the host listing
  shows its `orphans`. A host whose listing is mostly such jobs is told by its heartbeat replies to stop nothing at all, a cancel the
  restored store holds included (a stop already in the host's dispatch queue is still handed out), no orphan of it is ended at
  `--max-job-wall`, and the server logs an error and counts it (`js_unknown_listing_guarded_total`): look at the restore before
  anything else. A job the restored store holds, on the host its allocation names, keeps its state and its allocation.
- **The agents' own limit while the server is down.** The jobs survive the outage of the restore only as long as each agent's
  `--silence-stop` (300 s by default): an agent that has not heard the server for that long stops every job it holds, before any of
  the above applies. Keep the server's outage shorter than that, or raise `--silence-stop` on the agents first, until the agent keeps
  its running jobs through a silence (komira-ai/jobscheduler#1278).
- **Nothing is booked twice.** Placement rebuilds every host's bookings from the restored allocation table before it places, so a
  host whose restored allocation is live is not placed on beyond it, and a host still running a forgotten job is not counted until it
  stops listing it.
- **No id is issued again.** Minted job and allocation ids are 128 bits drawn from the system's random source, never from the store,
  so a restore cannot wind them back; a job id drawn that names a job the restored store holds is drawn again. A client-chosen job id that the
  restore forgot can be submitted again: that is how a client resubmits a job accepted after the backup. Jobs a client accepted after
  the backup read 404 until resubmitted.
- **Runs.** A run an older agent's host lists is answered `Cancel`: the server holds none.

The test that restores an older job store under live agents and asserts that nothing is stopped, the bookings and the ids is
`restart::a_restored_older_job_store_keeps_unknown_jobs_running_and_reissues_no_id` in `crates/js-lease-server/tests/server/restart.rs`.

## 6. Moving the job store between its file and the store

`js-lease-server job-store` moves the job store's rows across, with no server running on either side: the cutover is a short pause.
Its `--store` is a file holding the store's one line, as the server's `--store` (owner-only). It binds no listener, and it opens a SQLite
file under the file's lock, so a file a running server holds is refused. Exit status 0 when it did what it says, 1 when it refused or
failed (the line on standard error says why), 2 for a usage error.

**Into the store (before the first start over it).**

1. Stop the server that runs on the file (`SIGTERM`).
2. Run the dry run, and keep what it prints:

   ```sh
   js-lease-server job-store copy --from <state file>.jobs --store <store file>
   ```

   It prints, for the file, one line per key prefix with its row count and a SHA-256 of its rows (keys and values in key order; a
   row's time to live is not in it), then how many rows the store does not hold, holds with the same value and holds with another, and
   writes nothing.
3. If it names rows the store holds with another value (`different: <key>`), the store already holds a job store (a server started
   over it). The copy refuses them, with or without `--write`, and writes nothing: decide which job store to keep before going on.
4. Write: the same command with `--write`. It writes the rows the store does not hold, a page of 512 per transaction, each only if
   its key is still absent; a cordon is written with what is left of its time. A copy stopped part way is run again: it leaves the rows
   it wrote.
5. Run the dry run again. It must report `0 rows to write` and every row `already there with the same value`, with the same per
   prefix lines as step 2.
6. Start the server with the store setting. The usage file (`usage.db`) is not copied: node minutes and learned profiles start
   empty in the store.

**Back to files (a rollback to a server without the setting).**

1. Stop every server on the store.
2. Export to two new files (neither may exist; both are made owner-only):

   ```sh
   js-lease-server job-store export --store <store file> --jobs-to <dir>/jobs.new --usage-to <dir>/usage.new
   ```

   The jobs file gets the job store's rows (jobs, allocations, marks, taints, cordons, their indexes, the dispatch
   messages, the callbacks, the presence rows and the `usage/ended` history the job store writes beside each free); the usage file
   gets the node minutes, profiles and tails. It prints each file's per prefix lines and how many rows it wrote. The host bindings, join
   tokens and scaler records are not exported: a server without the setting reads them from the files the store's first start
   imported, as they were then.
3. Move the old `<state file>.jobs` (with any `-wal` and `-shm` beside it) and the old usage file aside, move the two new files into
   their places (`<state file>.jobs`, and `usage.db` or what `--usage-db` names), and start the server without the store setting.

The tests: `a_job_store_copied_into_the_store_and_exported_back_serves_its_jobs` (a job store the server wrote, copied and exported
back, gives the same rows and a file a server without the setting serves the job from) and
`a_round_trip_through_postgres_keeps_every_row` in `crates/js-lease-server/src/cutover_tests.rs`.

## Not built yet

- The rest of the server state in the Postgres store. Until it moves there, the backup that matters for a server that runs jobs is the
  job store and the binding file (or, with a store setting, the store that holds the bindings and the scaler's
  records).
- A restore verb and a check of a restored file pair before boot; steps 4 to 6 of section 4 are done by hand.
