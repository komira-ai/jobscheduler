# A new account per native job on a Mac

Status: steps 1, 2, 3 and 5 are built. Steps 1 and 3: the release signs the macOS programs, a privacy profile grants the root helper Full Disk Access, the agent reports whether the grant is in force and refuses native jobs without it. Step 2, with step 5 (the pool goes): the change that lands it makes a new account for every native job and deletes it afterwards. The decisions it made where this design left a gap are listed in "Decisions made in building step 2" below. Where that section and the text above it differ, that section describes what the code does.

Before step 2 a native job ran as one of a fixed pool of hidden accounts (`kbf-lease-pool-<slot>`), which the root helper `js-mac-session` reset in place after the job and lent again. The pool existed because macOS 27 refuses root, without the privacy grant Full Disk Access, the deletion of an account record, of an account's home and of the per-user folders the system placed for it, so nothing of an account could be removed. A reset in place emptied what it could and kept the rest: an empty `~/Library/Preferences` that the system guards, the `/var/folders/<xx>/<hash>` entry with its `0`, `C` and `T`, the record itself and every attribute of it root may not write. A job left behind whatever the reset did not reach, and the next job of the same slot inherited it.

The aim is that no job inherits anything of an account: each job gets an account that did not exist before it, and the account, its home and its per-user folders are gone, and shown gone, before its uid can be given to anyone again.

## What changes for an operator

| | Before | After this change (steps 1 and 3) | After step 2 |
|---|---|---|---|
| Signature of `js-agent`, `js-mac-session`, `js-mac-provision` | the linker's ad hoc signature, an identifier that changes with every build | Developer ID Application, team `U7RTX4XD3X`, fixed identifiers, hardened runtime, trusted timestamp | same |
| Full Disk Access | held by nothing | granted to `js-mac-session` alone by a profile the owner installs through device management | same |
| What the host list says | nothing | the host's capability `full-disk-access`: `granted`, `not-granted` or `undetermined` | same |
| A native job on a Mac without the grant | runs as a pool account that is reset in place | refused before anything is made, `no-full-disk-access` | same |
| A native job on a Mac with the grant | runs as a pool account that is reset in place | same: the reset does not use the grant | runs as a new account, deleted after the job (home, `~/Library/Preferences` and per-user folders included) and shown gone before its uid is used again |

What does not change: the Seatbelt profile every job runs under, the kill of the uid's processes, the leak scan after each job and the taint it sets, the wire between the agent and the job service, Linux hosts.

## Step 1: who holds the grant, and how a host knows it

### The program that holds it

The account operations run in the root helper `js-mac-session`, a launchd daemon of its own (`js-agent.session`). macOS's privacy control judges an access by the responsible process, and a launchd daemon is its own, so the grant must name the helper. The agent never touches an account: it asks the helper over its socket. Everything the helper starts (the job's `sandbox-exec`, `launchctl`, `getconf`) is started disclaimed (`spawn-disclaim.md`): the started program answers for itself, so the grant never reaches a job. The profile names that one program and no other: the agent, which talks to the network, gets no grant it does not need.

When the helper's work moves into the agent (`mac-one-program.md`, komira-ai/jobscheduler#991), the program that deletes accounts becomes `js-agent`. The switch release changes the profile's one entry to `js-agent`'s path and identifier, and the owner installs the new profile before that release reaches a Mac; the profile's pin test (below) makes the switch change both together.

### Signing

On a push to main the package workflow (`.github/workflows/js-agent-macos-pkg.yml`) builds the programs in a job that holds no key, signs them in a job that runs no code of the repository (`sign-code`: environment `pkg-signing`, the secrets `MACOS_APPLICATION_P12` and `MACOS_APPLICATION_P12_PASSWORD`, a keychain of the job's own that only `/usr/bin/codesign` may use, Apple's Developer ID intermediate fetched and held to its pinned SHA-256), then packages the signed programs in a third job that again holds no key. Each program gets a fixed identifier:

| Program | Identifier |
|---|---|
| `js-agent` | `com.komira.js-agent` |
| `js-mac-session` | `com.komira.js-agent.session` |
| `js-mac-provision` | `com.komira.js-agent.provision` |

`cargo xtask release check-code-signature <binary> --program <name>` reads a program back and refuses it unless its designated requirement is exactly the pinned one, `identifier "<id>" and anchor apple generic and certificate 1[field.1.2.840.113635.100.6.2.6] /* exists */ and certificate leaf[field.1.2.840.113635.100.6.1.13] /* exists */ and certificate leaf[subject.OU] = U7RTX4XD3X`, signed with the hardened runtime and a trusted timestamp. The package job runs it on every staged program before the package is built. The requirement names no hash, so every release meets it.

### The profile

`packaging/js-agent-macos/mdm/js-agent-full-disk-access.mobileconfig` is one Privacy Preferences Policy Control payload with one `SystemPolicyAllFiles` entry: the helper by its path, `/Library/Application Support/js-agent/bin/js-mac-session`, and its designated requirement. A program that is not an app bundle is named by its path (Apple's schema for the payload). The path is fixed: the package lays its programs there and an update replaces them in place. The requirement keeps another program put at that path from inheriting the grant. A test renders the profile from the signing table and holds the committed file to it byte for byte.

### Knowing

The helper answers a verb, `full-disk-access`, by listing the privacy database's directory (`/Library/Application Support/com.apple.TCC`), which only a process with Full Disk Access may list: success is `granted`, a refusal is `not-granted`, any other error `undetermined` (logged). It opens no file and reads no database. The agent asks at its start, before its first hello, and reports the answer as the detected capability `full-disk-access`, which the job service's host list (`GET /v1/hosts`) shows. A helper from before the verb is `undetermined`. A helper the agent cannot reach ends the agent's start, as a helper that cannot be reached for the boot reset does, and launchd starts it again.

The agent asks once per start: a profile installed while the agent runs takes effect at the agent's next start (an update, or `launchctl kickstart -k system/js-agent`). The helper itself needs no restart: the privacy control judges each access as it is made.

## Step 3: without the grant, one mode

A Mac agent whose helper does not hold the grant (`not-granted` or `undetermined`) refuses every native job, by the code `no-full-disk-access`, at admission and before anything is made for it: no account, no journal line, no bundle. The alternative was to keep today's reset as a named weaker mode. It is not kept, because two modes drift and the weaker one could then run by accident, on a Mac whose profile was never installed or was removed, with nothing but a log line to say so. The four Macs that run native jobs will hold the grant; a Mac without it says so in the host list and in the refusal of each job sent to it.

## Step 2: a new account per job

### The account

`user-create` makes the account when the job starts and `user-delete` deletes it when the job ends: the two verbs the agent already calls, with the meaning they had before the pool. No new verb.

- Name `kbf-lease-<term>-<seq>`, the lease's own (at most 31 bytes; the agent's term has 40 random bits).
- Never an administrator: no grant path exists (a job that needs administrator rights needs a VM).
- Hidden (`IsHidden` 1), shell `/usr/bin/false`, no password (no `ShadowHash` in its authentication authority), so no login and no secure token.
- Primary group the lease group (`--lease-gid`, default staff) and no other group. The bundle store is readable and searchable by others (`crates/js-agent/src/bundle/layout.rs`), so a job needs no group for it.
- Home `<homes>/<name>` (`--homes`, default `/Users`, on the data volume), made by the helper with mode 0700.

### Choosing a uid

The uid is chosen from the helper's range (`--uid-range`, default 600-699), never outside it. A uid is free when the ledger holds no live lease on it, the directory has no record with it, and nothing owned by it is left in the places the proof looks at (below). The helper takes the next free uid after the one it gave last (wrapping at the end of the range), rather than the lowest free one: a uid is then used again as late as the range allows, so anything a proof could not see has the longest time to show in a leak scan before the same uid runs again. The last uid given is the uid of the ledger's last `created` record, so no new state is kept.

A uid whose deletion has not been shown complete is held: its lease stays live in the ledger, and no job gets the uid.

### After the job: delete and show it gone

1. `kill-uid`, unchanged: kill every process of the uid, boot out its launchd domains, remove its crontab and `at` jobs, look until none is left.
2. Remove its System V IPC objects, unchanged.
3. Sweep the shared places of the uid's files (`/Users/Shared`, `/private/tmp`, `/private/var/tmp`, the launchd per-uid records), unchanged.
4. Delete the record (OpenDirectory), then remove the home whole and the uid's entry under `/private/var/folders` whole.
5. Show it gone. Each of these must hold:
   - no record by the name, and none by the uid (`getpwnam`, `getpwuid`);
   - no process of the uid, and its launchd domains read as gone, as `kill-uid` judges them;
   - the home path does not exist;
   - no directory two levels down in `/private/var/folders` is owned by the uid;
   - none of the swept places holds an entry owned by the uid;
   - no System V IPC object of the uid.
6. Only then is the lease's `deleted` written to the ledger, and the uid is free.

The ledger's `deleted` keeps the meaning it has always had: the account is gone. Its format does not change: `created <lease> <uid>` before the account is made, `deleted <lease>` once it is shown gone. A previous release reads a ledger this one wrote and deletes any account still live in it, as it did before the pool. The pool file is no longer written; a rollback to a release with the pool finds it as it was and makes each slot's account again.

### A deletion that does not complete

A deletion that fails at any step, or whose proof fails, leaves the lease live and its uid held. The helper names the step in its refusal and the agent keeps the lease in its journal and repeats the delete at the next job's end and at its next start. (A refused `kill-uid` is not a held account to the agent: a process of the uid may be alive, so the removal fails as any removal does and the lifecycle tries it again; see "Decisions made in building step 2".) While an account is held, the agent admits one native job fewer at a time (the account takes its slot out of service). Two held accounts taint the host (`account-not-deleted`), which stops all placement on it until an operator has looked: one failure may be a process that came back once; two are a fault of the host. Never is a held uid or slot used silently again.

### A crash between create and delete

- After `created` is written and before the account is made: the start's boot reset finds a live lease with no account, the delete finds no record, the proof holds, `deleted` is written.
- With the account made and the job running or ended: the boot reset deletes it as above (a lease the server still holds for the node is kept until the server lets it go, as the boot reset keeps a held lease today).
- After the record is deleted and before `deleted` is written: the delete is repeated (deleting a missing record is not an error), then the proof, then `deleted`.
- A `kbf-lease-*` account in the range whose lease the ledger does not know (a lost ledger): the boot reset records it as `created` and deletes it.
- The pool's own accounts (`kbf-lease-pool-<n>`) left by a release before step 2: the first boot reset with the grant deletes each the same way.

## Decisions made in building step 2

The design left these open; the code does this.

- The boot reset keeps only the lease the session switch's intent record names. A lease the server still holds is kept only when it is that lease, which is what the boot reset has always done; every other live lease in the ledger is killed and its account deleted.
- The pool file (`<state>/pool`) is removed, and the directory synced, before the first `kbf-lease-pool-<n>` account is deleted. A rollback to a release with the pool then finds no pool file and makes its pool afresh, instead of reading slots whose accounts are gone.
- An account found by name with the prefix `kbf-lease-` in the helper's uid range, whose lease the ledger does not hold as live (the pool's own accounts, an unparseable name, a lease the ledger has as deleted), is deleted by name with the same steps and proof and no ledger record. If that deletion fails, the boot reset fails ("the boot reset is incomplete: <account>: <step>"): these exist only at the first start after the pool or after a hand-made account, and a Mac whose grant does not let the helper delete must not start running accounts per job. Without the grant none of this runs, and native jobs are refused anyway. A `kbf-lease-<term>-<seq>` account whose lease the ledger does not know is recorded as `created` and then handled with the ledger's leases.
- A held lease does not fail the boot reset. A ledger lease whose account is not shown gone stays live with its uid held, is logged, and is returned in the boot reset's reply in a new field `held` (a list of dotted lease ids, present only when something is held). An agent from before the field cannot read a reply that has it, which is what such an agent does today when the boot reset fails. The agent journals each held lease it did not know and retries.
- One native job fewer per held account means one uid fewer in the helper's range. A Mac agent has no native slot count of its own (its hello carries no `max_leases`), so nothing in the agent counts the held account; the helper does not give its uid out, and the range holds one uid fewer to give.
- A held account, to the agent, is a `user-delete` the helper refused after a `kill-uid` that went through: no process of the uid is left, and the run is forgotten. A `kill-uid` the helper refuses is an ordinary removal failure: the run stays in the runtime, the lifecycle keeps the job in removal and tries again, and the host is tainted `cleanup-failed` as for any container that cannot be removed. A failed start whose cleanup has its `kill-uid` refused leaves the lease live in the journal and holds nothing; its `user-delete` refused after a `kill-uid` that went through holds the account. The agent's retry of a lease it already holds (after a job, at its start) may find its `kill-uid` refused and keeps the lease live and held: no run is attached to it.
- The host is tainted `account-not-deleted` from two held accounts wherever the runtime counts them, not only when a job's removal is refused: the lifecycle reads the runtime's count (`Runtime::held_accounts`) when it starts, after every reply it applies and after every step. So accounts held at the agent's start (its reclaim, the boot reset's `held` list), by a failed start, or by a boot reset asked for again taint before another job runs. The taint names no run. A count that falls back after a retry does not clear it: only an operator does. The rescue at an agent's end holds no lifecycle and writes no marker; the leases it leaves held are in the journal, and the next start counts them.
- `--pool-size` is accepted and ignored for one release, so a flags file that still carries it starts the helper. It does nothing and is documented as such.
- `pool-create` and `pool-status` are removed from the wire. An old agent that asks for `pool-status` gets "bad request: unknown variant", which it already reads as a helper that does not know the pool, and falls back to the per-lease name `kbf-lease-<term>-<seq>`: the per-job name.
- The account is made with no password and the shell `/usr/bin/false`. The helper's `session-login` verb is unchanged: when a lease asks for a GUI session it gives that lease's account a random password for the auto-login, as it did for a pool account.

## Step 4: what it costs

Measured on macOS 27.0.1, as root, from the helper's log: the reset in place of a pool account takes 42 to 59 ms from the lease's end to the slot free; `sysadminctl -deleteUser` took 70 ms to kill and 3 ms to remove a home before the record's deletion was refused. Nothing has been measured with the grant: neither the time to make an account (record, home, first per-user folder) nor a deletion that succeeds, nor whether the grant lets root remove the `/var/folders` entries the system marks `sunlnk`. Step 2 is built before those answers: until they are measured on a Mac with the profile in force, a deletion the grant does not allow fails its proof and holds the account, as a failed deletion does. The measurement is a root script on a Mac whose root shell holds the grant: it makes five hidden accounts the way step 2 would, times each make and each delete with its proof, and deletes all of them; without the grant it makes nothing and says so.

Accounts made ahead of time (a few, each used once and then deleted) would take the making off a job's start. They are worth it only if making an account takes a noticeable part of a short job's start; under a second, a job's start already spends more on its bundle and its sandbox. The measurement decides; the default is none.

## Step 5: what goes and what stays

Goes, with step 2:

- the pool: its file, `pool-create`, `pool-status`, the slots, the adoption of left accounts;
- the reset in place: emptying the home and the per-user folders while keeping them, the list of places kept, and the record write-back (setting the shell, the hidden flag and the primary group back, and refusing a record whose uid or home differs).

Stays:

- the Seatbelt profile every job runs under: a new account does not change what a job may reach while it runs;
- `kill-uid` and its judgment of what is left;
- the sweep of the shared places and the IPC removal, which reach what the job left outside its account;
- the leak scan after each job and the taint it sets: a login item, a Background Task Management entry or a launch daemon a job left outside its account is not undone by deleting the account.

It fits the one-program design: no new daemon, and no new helper verb but `full-disk-access`, which goes with the socket when the helper's work moves into the agent (the agent then checks its own grant in-process).

## Step 6: the proof

The isolation probe's two-phase persistence half (a job leaves a marker of every kind it may make; a later job looks for each) judges a new account per job: the `find` job runs as another account by construction, and every marker kind must come back `gone`. One row stays open: a POSIX named semaphore outlives every account change until the Mac restarts, because macOS has no interface that lists or removes another process's semaphores by owner; the profile cannot deny creating one without breaking Python's `multiprocessing` locks. A semaphore holds a count, not data.

The tests of the change run on Linux against the helper's fakes (the account's shape, the uid choice, each step's failure, each crash point, the ledger read by the previous release). The root test that installs the package on the hosted macOS runner proves the side it can: the runner's helper is unsigned and holds no grant, so the host list says `not-granted`. A deletion with the grant can only be shown on a Mac with the profile.

## Step 7: the owner's steps

1. Merge; the release on main signs the programs. Nothing on a Mac changes until the profile is installed.
2. In Apple Business, upload `packaging/js-agent-macos/mdm/js-agent-full-disk-access.mobileconfig` as a custom profile and assign it to the Macs that run native jobs. hostkeeper ships its own profile for its own program; one profile can carry both entries (one `SystemPolicyAllFiles` list with two entries), but each repository's test pins its own file, so a merged file would be pinned by neither. Upload the two files in the same sitting: their payload identifiers differ, so neither replaces the other.
3. On each Mac, once the release with the signed programs is installed and the profile is listed (`profiles list` as root): restart the agent (`launchctl kickstart -k system/js-agent`). The helper logs `full disk access: granted`, and the host list shows `full-disk-access: granted`. `codesign --display --requirements - "/Library/Application Support/js-agent/bin/js-mac-session"` prints the pinned requirement.
4. After step 2 is released: run one native job, then, as root, look for its account by its uid: no record (`dscl . -search /Users UniqueID <uid>`), no home, no directory under `/private/var/folders` owned by the uid (`find /private/var/folders -maxdepth 2 -uid <uid>`).
