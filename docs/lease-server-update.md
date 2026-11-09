# Updating the lease server

The host updater moves a host's `js-lease-server` from one release to the next through the
server's update hook, `js-lease-server update-hook`, the way it moves `js-agent` through the
agent's. The hook is the service's side of the updater's contract: the updater (another project)
runs one step per process, reads the exit status and the last line of standard output, and decides
what runs next. This page says what the server's hook does, what it needs, and how it fails.

## What an update does

An update restarts the server in place. There is no cordon of any host, no drain and no standby:
`cutover` stops the server's unit, waits for the store's writer lock to be let go, moves the release
link to the new release and starts the unit again. The outage is the time the unit takes to stop
and start, a few seconds to at most the bounds below.

What a restart keeps is what the server keeps on disk; what it holds only in memory is lost with
the process. At this release:

- **Kept:** the files beside the state file (cordons, taints, registered hosts, the
  scaler's records), the job store (`<state file>.jobs`, with `--job-listen`) and the usage file.
  A lease file left by an earlier release is neither read nor written, and stays as it was.
- **Lost:** nothing that the server holds only in memory is a run: it has none. A host of an
  older agent that lists a run after the restart is told to cancel it.

A job outlives the restart: it is in the job store, and its host keeps running it. The store (`--store`)
holds the host bindings, the join tokens and the scaler's records; the hook uses it for its writer
lock (below).

## What the hook needs

- **A server started with a store setting** (`--store` or `server.store`). The new process proves it
  started by taking the store's writer lock, which a server takes first thing at start, before it
  reads anything or binds a listener; `verify` waits for a holder of the lock other than the one
  `prepare` saw. A server without a store setting takes no lock, and `verify` of its update fails
  `no_lock`. At this release a server that registers agents (`--agent-pool-config`, or an elastic
  pool) is refused a store setting, so the hook can update only a server that does not; updating
  the others stays a hand step (stop the unit, swap the release, start it) until their state is kept
  in the store.
- **`/healthz` over plain HTTP on the loopback.** `verify` asks `GET /healthz` at the address
  `--healthz-url` names, which must be `http://` and a loopback address and port. A server whose
  every listener serves TLS cannot be checked that way, and its `verify` fails `not_serving`.
- **A unit and a release link.** The server runs as a unit (`js-lease-server.service` unless
  `--unit` names another) whose program is reached through the release link, so that moving the
  link changes what the next start runs. This repository ships no unit for the server: whatever
  supervises it is the operator's.

## The command

```text
js-lease-server update-hook [--name=value ...] <prepare|cutover|verify|rollback|cleanup> [<artifact>] --params <file>
```

The verb is answered before any of the server's flags is read and before any listener or signal
handler. The flags before the step come from the declaration's `hook_args`; the updater refuses a
string in the parameters of a hook that runs as root, so the text values are flags:

| Flag | Value |
|---|---|
| `--release-link` | the symbolic link the unit runs the server through, an absolute path directly inside a declared directory |
| `--unit` | the unit, `js-lease-server.service` when absent |
| `--store` | the file that holds the store's connection, the one the server's own `--store` names: a regular file only its owner can read or write. Nothing of it is ever printed: a store that cannot be reached is said to be, and no more |
| `--healthz-url` | the server's own address, `http://127.0.0.1:<port>`: an IPv4 loopback address, since the updater refuses a `hook_args` word with brackets (the hook itself also reads `http://[::1]:<port>`) |

The `--params` file is the table the updater writes: its reserved keys `updater.staged_release`,
`updater.previous_release` (absent on a first install), `updater.target_version`,
`updater.previous_version`, and the declaration's own `defer_retry_secs`, what a `prepare` that
could not reach the store asks the updater to wait before it orders the step again. `prepare` is
given the staged release directory as its artifact too, and refuses one that is not the table's.

## The steps

| Step | What it does |
|---|---|
| `prepare` | Reads: the release link (a link to the previous release the table names, beside the staged one, or no link on a first install), the store's schema version against the range the new release runs on, and the holder of the store's writer lock, probed without taking it (nobody, when the server is stopped). Writes the hook record. Stops nothing. |
| `cutover` | Marks the stop, stops the unit and waits for it to be down, waits for the writer lock to be free, moves the link to the staged release, starts the unit and waits for it to be active. The stop is the commit: from it on every ending says `reversible: false`. A holder of the lock left after the stop fails the step with the link unmoved. |
| `verify` | Looks every 10 s until its bound for proof: the unit active, the writer lock held by a holder other than the one `prepare` recorded, and `/healthz` answering 200. A unit that is down, failed or waiting to restart after a crash ends the step at once. Changes nothing. |
| `rollback` | From the record and what the machine shows: nothing was stopped, so nothing to undo; a first install that moved the link has no previous release to restore; the unit up on the staged release is stopped, the lock waited for, the link moved back and the unit started; up on the previous release, nothing to do; up on any other release, halt. It ends when the previous release holds the writer lock. |
| `cleanup` | Deletes the hook record, once `verify` succeeded. |

Each step takes the lock of the hook's state directory first and records each act before it makes
it, so a step that is killed and run again goes on from where it stopped and makes no act twice:
it asks the unit manager and the link what was done, and believes them over the record.

## Exit codes and reasons

Exit 0 is done, 1 failed, 75 deferred (nothing irreversible happened, try later) and 76 halt (no
version can repair this). The last line of standard output is a JSON object with `class`
(`release`, or `environment` when the release may be fine), `reason`, `reversible` and `detail`.

| Reason | Exit | Class | When |
|---|---|---|---|
| `bad_args` | 1 | release | the command line was refused |
| `bad_params` | 1 | release | the params file, the release link or the store file is not usable |
| `locked` | 1 | release | another step holds the state directory |
| `state_invalid` | 1 | release | the state directory, the record or the declaration is not as the hook requires; a step run out of order |
| `state_invalid` | 76 | release | `prepare`: the link is not a symbolic link, or points outside the release directory or at another release than the table's previous one; `rollback`: the unit runs a release that is neither of the update's |
| `store_unreachable` | 75 | environment | `prepare`: the store did not answer |
| `store_unreachable` | 1 | environment | another step: the store did not answer its last probe |
| `schema_unsupported` | 1 | release | `prepare`: the store holds no schema, or one older than the new release runs on |
| `stop_failed` | 1 | release (environment when the unit manager could not be read) | the unit could not be stopped, or did not stop in time |
| `lock_not_released` | 1 | environment | the unit is down and the writer lock is still held |
| `link_failed` | 1 | release | the release link could not be read or moved |
| `start_failed` | 1 | release (environment when the unit manager could not be read) | the unit could not be started, ran and died, or was not active in time |
| `no_lock` | 1 | release | `verify`: no new process took the writer lock by the bound |
| `not_serving` | 1 | release | `verify`: a new process holds the writer lock and `/healthz` did not answer 200 by the bound |
| `no_previous_release` | 1 | release | `rollback` of a first install that moved the link |
| `rollback_not_serving` | 1 | release | `rollback`: the previous release did not take the writer lock by the bound, or is not running |
| `not_verified` | 1 | release | `cleanup` before a `verify` succeeded |

## The declaration and its timeouts

`packaging/js-lease-server/updater.d/js-lease-server.toml` is the host updater's declaration of the
hook (root-owned, mode 0644, installed as `/etc/updater.d/js-lease-server.toml` by the updater's
own install step). The Linux `js-lease-server` release archives carry it as
`updater.d/js-lease-server.toml` beside the binary. The hook runs as root with no capability, may
write exactly three directories, none inside another (the release directories, the directory of the
release link, and its own state directory, `/var/lib/js-lease-server-update`), and manages the one
unit the declaration names.

The declared timeouts are built from constants of the hook
(`crates/js-lease-server/src/update_hook/declaration.rs`), and the hook refuses a declaration whose
timeout of a step is shorter than its sum. There is no drain term:

| Step | Timeout | Built from |
|---|---|---|
| `prepare` | 90 s | the store's reads 30 + a margin 60 |
| `cutover` | 300 s | the stop 90 + the lock's release 30 + the start 120 + a margin 60 |
| `verify` | 300 s | the looks 285 + a margin 15 |
| `rollback` | 420 s | the stop 90 + the lock's release 30 + the start 120 + the previous release taking the lock 120 + a margin 60 |
| `cleanup` | 60 s | the record's delete |

`verify_deadline_secs` is 720, the `verify` and `rollback` bounds together.
`crates/xtask/tests/js_lease_server_declaration.rs` loads the file with the updater's own validator
and with the hook's reader, and pins these numbers to the constants.
