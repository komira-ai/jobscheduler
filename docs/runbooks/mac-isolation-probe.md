# The Mac isolation probe

`js-isolation-probe` (crates/js-isolation-probe) is a program that runs **as a job** on a Mac pool host and says
what a job can write, read, reach and leave behind. It is the acceptance job an operator runs on a Mac after an
install, and the smoke job of a Mac roll: every answer it gives is compared with the answer pinned in
[`crates/js-isolation-probe/expected.toml`](../../crates/js-isolation-probe/expected.toml), and the job fails, naming
each probe, when one differs or did not run.

It is written to change nothing it cannot put back, as a job's user (it refuses to run as root, whose writes would
succeed where a job's must not):

- it writes only files it names (`.js-isolation-probe-<tag>`, empty) and removes them; `probe` removes at once
  whatever else it managed to make (a launchd job, a crontab, an `at` job, an attribute on its own record);
- it makes no folder: a place that is absent is `n/a` (a folder a job makes under `~/Library` is one neither the
  job nor the slot's reset can remove), and a place the job may not even look at is `denied`, with the error number
  of that look;
- it touches the job account's own state only: its pasteboard (overwritten, then emptied by `find`), its preferences
  domain `com.komira.js-isolation-probe` and its keychain (each only when the home already has the folder it is kept
  in), and its crontab, which it uses only when the account has none;
- it only connects to and looks up the addresses it is given (no scan), and sends nothing;
- it never prints the contents of a file it opened, only whether it opened and its size;
- the markers `leave` keeps are removed by `find` on the same slot when the job's removal did not remove them. The
  application and plists `leave` writes into the run directory go with the account's home.

The pins are for the Mac job shape: a fresh slot account per job, whose home (`/Users/<account>`, 0700) is deleted
with it; the job's `HOME` and `TMPDIR` below the run directory (`<account home>/exec/.home`, `.tmp`); the agent's one
Seatbelt profile; a supervisor that gives the job exactly its account's group; network `none`.

## Output

One line per probe, then the verdict, all on standard output (the job result's 8 KiB stdout tail holds all of it):

    # js-isolation-probe probe as <user> (uid <uid>, gid <gid>, egid <egid>, groups <gid> ...), home <HOME>, account home <home>, in <run dir>
    write:run-dir: allowed; <run dir>
    write:private-tmp: denied (EPERM); /private/tmp
    ...
    pinned [probe]: <n> as pinned, 0 differ, 0 not run, 0 unpinned
    verdict: PASS

An answer is one of `allowed`, `denied` (with the error number when the system refused: `EPERM` is the sandbox's
answer, `EACCES` the file modes'), `n/a` (the place does not exist on this host, or the probe cannot look), `found`
and `gone` (a marker an earlier job left), `limited` and `unlimited` (a resource limit), `info` (free space, never
judged) and `error` (a timeout, an unexpected failure). `DIFFERS <name>` and `NOT RUN <name>` lines fail the job;
`UNPINNED <name>` lines (a probe the pins do not name) are printed and fail nothing. The first line names the
process's real and effective group ids and its supplementary groups, and the account's home as the account database
names it. The exit status is 0 for a
pass, 1 for a failure, 2 when the probe could not run.

## The phases

| phase | what it does | judged by |
|---|---|---|
| `probe` | a file in the run directory and in every place a job must not write (`/private/tmp`, `/private/var/tmp`, `/Users/Shared`, `/Library/Caches`, `/opt/homebrew`, `/usr/local`, `/private/etc`, `/Library/LaunchDaemons`, `$HOME`'s `Library/LaunchAgents` and `Library/Preferences`, the account's own home and its `Library/Preferences`, its `/private/var/folders` temporary directory, and each `--write`); opening `/private/etc/sudoers`, the privacy database's folder and `TCC.db`, Time Machine's preferences, the account's own home, and each `--read`; one TCP connect per `--connect`, one connect per `--connect-unix` and one lookup per `--lookup`; `sudo -n`, `su` with no password, membership of `admin`, whether the process holds the `wheel` or `admin` group id, the setuid and setgid programs in `/bin`, `/sbin`, `/usr/bin`, `/usr/sbin` and `/usr/libexec` it could execute (listed, never run), a write of its own record's shell with the same value and of an attribute of its own, a launchd job in `user/<uid>` and `gui/<uid>`, a crontab and an `at` job (each removed at once when allowed); the file size, process, descriptor and memory limits; free space; and 10,000 small files made and removed | `[probe]` |
| `leave --token T` | one marker of every kind it is allowed to make, kept: a named semaphore, a System V semaphore and shared memory segment, a preferences key, a Launch Services registration, a pasteboard entry, a keychain item, a background process in its own session, a crontab, an `at` job, launchd jobs, an attribute on its own record, and a file in every place `probe` writes; prints the `find` arguments | `[leave]` |
| `find --token T --left-by-uid ... --left-by-user ... --left-by-home ...` | looks for each marker and says whether it is found or gone; on the slot that left them (the same uid) removes each one it finds, on another slot removes nothing | `[find-same-slot]` or `[find-other-slot]` |
| `hold --secs N` | waits N seconds and ends: a job that holds its slot, so that the next job is lent another | not judged |

A token is 1 to 16 lower-case letters and digits; use a new one for each `leave`.

## Getting the bundle onto the Mac

The bundle is one tree holding the program, built for `aarch64-apple-darwin` with `MACOSX_DEPLOYMENT_TARGET=11.0`
(as the release builds the Mac programs) and packed with `js-bundle pack`, as the smoke bundle's recipe packs its tree:

    cargo build --locked --release -p js-isolation-probe --bin js-isolation-probe    # on a Mac
    mkdir -p tree/bin && cp target/release/js-isolation-probe tree/bin/
    js-bundle pack --out layout --name js-isolation-probe --bundle-version 1 --os-version 11.0 \
      --tree js-isolation-probe=tree --license js-isolation-probe=Apache-2.0 \
      --source-url js-isolation-probe=https://github.com/komira-ai/jobscheduler/tree/main/crates/js-isolation-probe \
      --entrypoint 'js-isolation-probe=${tree:js-isolation-probe}/bin/js-isolation-probe' \
      --annotation org.opencontainers.image.source=https://github.com/komira-ai/jobscheduler --network on

`pack` prints the manifest digest. `--network on` lets the job's own network level decide; the pins are for a job whose
network is `none`. Then either push it to a registry the host may pull from (`js-bundle push`) or place the layout into
the host's bundle store, and add the digest to the host's `--bundle-allow-file` (read at every job start). The image is
`<registry>/<path>/js-isolation-probe@<digest>`, under one of the host's `--image-allow` entries.

## The jobs

The arguments name what differs per host: another user's home, root's home, the bundle store, the agent's files and
its accounts ledger, the helper's files and socket, another service's key, three files the operator makes for the run,
the job service, a host on the same network, the cloud metadata address, a public address, a port that listens on
loopback, an address the host's packet filter lets jobs with an allow-list reach, and a public host name. Every name
here is pinned, so each must be given.

The three files, made as root in a folder every user may pass (`0755`) and removed after the run: a control file
(`root:wheel`, `0644`, which a job must read: it proves the folder is reachable), and two files of mode `0040` owned by
root, one of a slot account's group and one of group `everyone` (12), which a job must not read: a job holds its own
account's group and no other.

```
js job run --server <job service> --token-file <client token> --id <id>-probe \
  --spec.image <image>@<digest> --spec.command @bundle/js-isolation-probe \
  --spec.constraints.os macos --spec.constraints.arch arm64 --spec.constraints.exec sandbox-exec \
  --spec.needs.cpu-millis 1000 --spec.needs.memory-bytes 268435456 --spec.wall-limit-seconds 600s \
  --spec.args probe \
  --spec.args --write --spec.args other-home=<another user's home> \
  --spec.args --write --spec.args root-home=/var/root \
  --spec.args --write --spec.args bundle-store=<the bundle store> \
  --spec.args --read --spec.args other-home=<another user's home> \
  --spec.args --read --spec.args root-home=/var/root \
  --spec.args --read --spec.args agent-flags=/etc/js-agent/flags \
  --spec.args --read --spec.args agent-token=/etc/js-agent/token \
  --spec.args --read --spec.args agent-state=/var/db/js-agent/state \
  --spec.args --read --spec.args accounts-ledger=<the agent's accounts ledger> \
  --spec.args --read --spec.args helper-state=/var/db/js-mac-session \
  --spec.args --read --spec.args host-key=<another service's key file> \
  --spec.args --read --spec.args slot-group-file=<the 0040 file of a slot account's group> \
  --spec.args --read --spec.args everyone-group-file=<the 0040 file of group everyone> \
  --spec.args --read --spec.args control-file=<the 0644 control file> \
  --spec.args --connect --spec.args job-service=<host:port> \
  --spec.args --connect --spec.args lan-host=<host:port> \
  --spec.args --connect --spec.args metadata=<metadata address>:80 \
  --spec.args --connect --spec.args public=<public address>:443 \
  --spec.args --connect --spec.args loopback=127.0.0.1:<listening port> \
  --spec.args --connect --spec.args pf-endpoint=<an allow-listed host:port> \
  --spec.args --lookup --spec.args public-name=<public host name> \
  --spec.args --connect-unix --spec.args helper-socket=<the helper's socket>
```

Then the persistence check, in this order (the pool lends the lowest free slot, so a job alone gets the same slot
as the one before it):

1. `leave`: the same job with `--spec.args leave --spec.args --token --spec.args <token>` and the three `--write`
   arguments. Its output's `# next:` line gives the `find` arguments.
2. `find` on another slot: start a job that holds the slot (the same image and command with
   `--spec.args hold --spec.args --secs --spec.args 120`), and while it runs, the `find` job with the `# next:`
   arguments and the three `--write` arguments.
3. `find` on the same slot, alone: it says, for each marker, whether the job's removal took it, and removes what is
   left.

The same `find` judges the removal whatever it does (empty the account's places, or delete the account and make a
new one for the slot): each marker is `found` or `gone`. A named semaphore is the one marker only its name can
remove; it lasts until the Mac restarts unless a `find` on its slot removes it.

## Changing an answer

A pinned answer is what a job can do. A change that makes a job able to do more (or less) changes the pin in the same
pull request, says why in the comment beside it, and is a change of the Mac's isolation, reviewed as one. Run the
crate's tests (`cargo test -p js-isolation-probe`) after editing `expected.toml`: they read it.
