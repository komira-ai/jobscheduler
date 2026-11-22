# Restarting a Mac and updating its macOS (js-agent in pool mode)

A Mac that runs js-agent in native mode (`sandbox-exec`, an account made for each job, uids 600-699) is built to
come back into service after a restart with no step by hand: launchd starts the three daemons of
the package again, the agent says hello again, and the lease server places on it again. This page
says why (and which parts have not yet run on a Mac, [below](#what-has-been-checked-on-a-mac)), what a Mac must keep for that to stay true, how to check a Mac after a restart, how to
take one out of service for a restart or a macOS update and put it back, and what is still to be
proven on a Mac. The package itself is in
[packaging/js-agent-macos/README.md](../../packaging/js-agent-macos/README.md) and its design in
[docs/design/mac-hostkeeper-package.md](../design/mac-hostkeeper-package.md).

In short:

| case | step by hand |
|---|---|
| a restart of an idle Mac (planned, a power cut, a panic) | none |
| a planned restart of a Mac that runs jobs | cordon it first and lift the cordon after (below) |
| a macOS update through hostkeeper's maintenance run | none while the run succeeds; a halted run is put back in hostkeeper |
| a restart while a job is being cleaned up | the host is tainted `check-owed`; clear it (below) |
| a background item macOS has switched off | allow it at the Mac's console (below) |

## What comes back by itself, and in what order

The package installs three LaunchDaemons into `/Library/LaunchDaemons`, and launchd loads every
plist of that directory at boot, before anyone logs in. Nothing else starts them: hostkeeper
enables the labels when it installs the package (`launchctl enable`, which launchd keeps across
restarts) and starts nothing at its own start for a `macos-pkg` service, and the update hook's
`launchctl bootout` of an update is not kept across a restart. What each daemon does at boot
(`crates/js-mac-provision/src/daemon.rs`, `crates/js-mac-provision/src/launchd.rs`):

- `js-agent.pf-pool` runs at load and every 30 seconds (`RunAtLoad`, `StartInterval`), and loads
  the pool's pf anchor from the agent's flags when it is not in force. A restart drops the anchor
  and resets pf; the first run puts both back.
- `js-agent.session`, the root helper, and `js-agent`, the agent, do not run at load: each has
  `KeepAlive` with `PathState` on `/etc/js-agent/flags`, so launchd starts it at load when the
  flags file is there, and again after every exit, at most once in 30 seconds
  (`ThrottleInterval`). The flags file is the group's configuration package's, and a restart
  keeps it.

The three start together; launchd orders none of them. The agent's start
(`crates/js-agent/src/platform/macos.rs`, `crates/js-agent/src/native/mod.rs`) mounts the scratch
volume `js-agent-scratch` on `/var/db/js-agent/scratch` (no `/etc/fstab` line names it, so every
boot needs this mount), dials the helper's socket, asks the helper for its boot reset, and, with
`--native-network=on`, checks that the pool's anchor is in force. An agent that starts before the
helper has made its socket, or before the pf job has loaded the anchor, ends with the reason in
`/Library/Application Support/js-agent/log/js-agent.err.log`, and launchd starts it again 30
seconds later. So the first one or two starts after a boot may fail and the host says its hello
within about a minute; this is expected and needs nothing.

The helper's boot reset (`crates/js-mac-session/src/helper/switch.rs`, `boot_reset`) ends every
lease its ledger holds except the one the session switch names, deleting each lease's account and showing it gone (a lease whose account is not shown gone stays live, its uid held, and the agent retries it), and, with Full Disk Access, deletes any `kbf-lease-*` account the ledger does not know, the pool accounts of an earlier release among them. A
job that ran when the Mac stopped has no process after the boot: the agent finds its run gone
(`crates/js-agent/src/native/adopt.rs`), ends it and tells the server.

The server answers the agent's first hello `host-live` (409) while the session from before the
restart is still within its grace (15 seconds unless the pool's configuration says otherwise);
the agent retries with backoff (from 1 second, doubling, at most 60 seconds) and is admitted once
the old session has lapsed. The host keeps its id, and a Mac joins with hostkeeper's host
certificate, whose credential is the host's own, so it is not refused `host-bound`. The jobs the
restart ended are given up as for any host that stops
([docs/design/absence.md](../design/absence.md)), and each goes back to the queue or fails as its
retry policy says.

## What the Mac must keep

- **FileVault off.** With FileVault on, every restart stops at the pre-boot unlock screen until
  someone types a password at the Mac, and no daemon runs until then; a restart nobody planned (a
  power cut, a panic) cannot be unlocked from afar. `fdesetup status` says `FileVault is Off.`
- **No sleep, and restart after a power cut.** hostkeeper's daemon, at each of its starts, sets
  whichever of its keep-awake settings differ (`sleep 0`, `disksleep 0`, `autorestart 1`, `womp 1`
  and the rest of its list, with `pmset -a`), unless its configuration turns that off. `pmset -g`
  shows the result.
- **No auto-login, and no console session needed.** Native mode runs every job as an account that
  the root helper makes for it and spawns; nothing of it needs a user logged in at the console or a GUI
  session, and the daemons run as root from boot. The helper clears auto-login in its boot reset
  (`defaults delete /Library/Preferences/com.apple.loginwindow autoLoginUser` and the removal of
  `/etc/kcpassword`, `crates/js-mac-session/src/helper/switch.rs`, `boot_reset`), because the
  session-switching modes it carries use them; the agent asks for that reset when it starts
  against a helper that has not made it yet, so once after every start of the helper. Do not set
  auto-login on a Mac of the pool: it is removed after the next restart. A user who is logged in
  at the console does no harm.
- **The host setup.** The update hook's `prepare` makes the group `_jsagent`, the state root
  `/var/db/js-agent/state` and the volume `js-agent-scratch` (see the design's host setup). A
  restart keeps all three. Without one of them the agent or the helper ends at every start, and
  launchd starts it again every 30 seconds until the next `prepare` makes what is missing.
- **The daemons allowed to run in the background.** macOS lists each of the package's daemons
  under System Settings, General, Login Items & Extensions, "Allow in the Background". An item
  switched off there is not started at boot, whatever its plist says. See
  [Background items](#background-items).

## Checking a Mac after a restart

On the Mac, as an administrator (`launchctl print` of a system daemon needs no root; read the
logs with `sudo` if their mode does not let you):

```text
launchctl print system/js-agent | grep -E 'state =|runs =|last exit code'
launchctl print system/js-agent.session | grep -E 'state =|runs =|last exit code'
launchctl print system/js-agent.pf-pool | grep -E 'state =|runs =|last exit code'
tail -n 20 '/Library/Application Support/js-agent/log/js-agent.err.log'
tail -n 20 '/Library/Application Support/js-agent/log/js-mac-session.err.log'
tail -n 20 /var/log/js-mac-provision.log
```

The agent's reasons for ending are in its `err.log`, the helper's in `js-mac-session.err.log`, and
the pf job's in `js-mac-provision.log`.

`js-agent` and `js-agent.session` are `state = running`; `js-agent.pf-pool` is `not running`
between its runs with `last exit code = 0`. A `runs` count above 1 for the agent with a running
state is the start order of the boot above, not a fault. `Could not find service` for a label
whose plist is in `/Library/LaunchDaemons` means launchd did not load it: see
[Background items](#background-items).

On the operator's side:

```text
js host list --server https://<job service> --token-file <operator token>
```

The host is `HOST_STATE_LIVE`, its `taint_reason` is empty, it has no `cordon_expires_at_ms`, and
`agent_version` is the version it ran before the restart.

## A planned restart of a Mac that runs jobs

A restart ends the jobs the Mac runs: their processes stop with the Mac, and the server gives
them up once the host is silent past its grace. A stop inside a job's post-job checks also taints
the host (`check-owed`, below). To restart without either, keep new jobs off the host and wait
for the ones it runs to end:

```text
js host cordon --server https://<job service> --token-file <operator token> --host-id <id> --cordon-secs 12h
js host list --server https://<job service> --token-file <operator token>
```

The host's id and the operator's token are as in [cordon-a-node.md](cordon-a-node.md). Ask for
longer than the longest job on the host plus the restart (at most seven days per call; repeat the
call to extend it). The cordon is the server's and holds through the restart. Wait until the
host's `booked` list is empty, restart the Mac, check it as above, check that
`cordon_expires_at_ms` is still in the future (a cordon that lapsed lets jobs land before the
check), then put it back:

```text
js host uncordon --server https://<job service> --token-file <operator token> --host-id <id>
```

The cordon is the one step by hand of a planned restart: a host that rejoins is placed nothing
until the cordon lapses or is lifted.

## A macOS update

hostkeeper's maintenance run is the way to update a Mac's macOS (hostkeeper repository,
`docs/design/maintenance-updates.md`). The js-agent package's manifest names the update hook's
`drain` and `resume` (`crates/xtask/src/release/desired/manifest.rs`, `HOOK_DRAIN`, `HOOK_RESUME`),
so the run drains the agent before the update and puts it back after:

1. `drain` writes the hold file `/var/db/js-agent/state/hold`, cordons the host in the lease
   server and returns once the host runs no job. Nothing is stopped: the jobs end on their own,
   and a run that names a drain deadline is deferred (exit 75, the host put back) when they have
   not ended by then.
2. hostkeeper installs the update and restarts the Mac.
3. After the boot the agent comes back as above, but **held**: while the hold file is there it
   answers every start `host-held` and keeps its host cordoned from its first hello on
   (`crates/js-agent/src/lifecycle/hold.rs`). The hold file is kept across the restart, so no job
   lands on a Mac whose update has not been checked.
4. hostkeeper checks the update, then runs `resume`, which removes the hold file and lifts the
   cordon `drain` placed (an operator's cordon that was in force before it is kept).

A step that fails halts the host in the run: `resume` does not run, and the Mac stays held and
cordoned. Put it back in hostkeeper (its maintenance page, "A halted host": Retry or Put back,
which runs `resume`), not by removing the hold file by hand.

What an update can change, and what puts it right:

- `/etc/pf.conf` is the system's and an update may write it again; the pf job loads the anchor
  again within 30 seconds.
- The major version the agent reports (`os_version`) changes with a major upgrade; a pool or a
  job that names a least `os_version` places by the new one at the next hello.
- Mac administrators have reported updates that reset the background item settings of
  third-party daemons (Apple's developer forums); see [Background items](#background-items) and
  check after every update.

## A restart while a job is being cleaned up (`check-owed`)

The agent writes `owed-<run>` in its state root before the first step of a job's cleanup (the
removal of its lease's account and its run directories) and removes it once the job's post-job
checks have run (they check nothing on a Mac). A restart between the two leaves the record, and the next start
taints the host `check-owed`, which the agent never clears itself
(`crates/js-agent/src/lifecycle/taint.rs`): the host is listed with that `taint_reason` and runs no
job. An idle Mac has no such record. `js-agent clear-taint`
([clear-a-taint.md](clear-a-taint.md)) clears a Linux host's taint and does not run on a Mac (it
reads the crun runtime's ledger and stops a systemd unit). On a Mac, once the cause is understood
and the host is known to be clean, the renames the verb makes are done by hand, as root, in its
order. They are not for a `host-job` taint (that machine is reimaged), and the host must run no
job: a native run outlives the agent's stop, so check first that its `booked` list is empty
(`js host list`).

1. `launchctl bootout system/js-agent` (it returns when the agent has exited).
2. In `/var/db/js-agent/state`, rename each `owed-<run>` to `owed.<run>.cleared-<unix seconds>-by-<you>`,
   then `taint.pinned` (if there) to `taint.pinned.cleared-<unix seconds>-by-<you>`, then
   `taint.json` to `taint.json.cleared-<unix seconds>-by-<you>`. Remove nothing. The verb also
   writes a record of the clear (`taint.cleared-<unix seconds>-by-<you>.json`); by hand, the
   renamed files are the record, and the reason goes where the operator records the clear.
3. `launchctl bootstrap system /Library/LaunchDaemons/js-agent.plist`.

The agent starts with no marker and ends the server's taint at its first heartbeat.

## Background items

macOS 13 and later record each LaunchDaemon a package installs as a background item, and an item
switched off ("Allow in the Background" off) is not loaded at boot. On a Mac without device
management a newly installed item is expected to be allowed (the drill below confirms it for
this package); only a person at the Mac's console can switch it back on (System Settings, General,
Login Items & Extensions), and only a device management profile (Apple's Managed Login Items
payload) can allow it in advance. The package is signed; its programs are signed ad hoc by the
linker ([packaging/js-agent-macos/README.md](../../packaging/js-agent-macos/README.md)).

Read the state as root:

```text
sfltool dumpbtm
```

Each of `js-agent`, `js-agent.session` and `js-agent.pf-pool` is listed with its plist under
`/Library/LaunchDaemons` and a disposition that includes `enabled` and `allowed`. Check it once
after the first install and after every macOS update. A `disallowed` item, or a label whose plist
is installed and that `launchctl print` cannot find after a boot, needs the switch at the console;
`sfltool resetbtm` is not a fix: it resets the state of every item on the Mac.

## What has been checked on a Mac

launchd starts a job whose `KeepAlive` is `PathState` on a path through a symbolic link, as
`/etc/js-agent/flags` is (`/etc` is `/private/etc`), at load when the path is there and again after
a non-zero exit at its throttle, and does not start it while the path is absent and does once it
appears: a job of that shape in a user's launchd domain, on macOS 26.4 and 27.0.1.

Not yet run on a Mac: a restart of a Mac with js-agent installed (the drill below), the boot order
above against a real helper and pf, a macOS update through hostkeeper's maintenance run, and the
background item state of the package's daemons after an install and after an update.

## The restart drill

On one idle Mac of the pool, once js-agent runs there and says its hello:

1. Read the state: the checks above, `sfltool dumpbtm`, `fdesetup status`, `pmset -g`, and that
   the host is live, untainted, uncordoned and runs no job (`js host list`).
2. Restart it (`shutdown -r now` as root).
3. Touch nothing. Once the Mac answers again, read the same state. The drill passes when, with no
   step by hand, the host is `HOST_STATE_LIVE` with no taint and no cordon at the same
   `agent_version`, both gated daemons run, the pf job's last exit is 0, and no background item
   is switched off.
