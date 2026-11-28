# A new account per native job on a Mac

Status: steps 1 and 3 are built (the release signs the macOS programs, a privacy profile grants `js-agent` Full Disk Access, the agent reports whether it holds the grant and refuses native jobs without it). Step 2, a new account for every native job that is deleted afterwards, is built in another shape than the one designed here: slot accounts in `crates/js-agent/src/mac_accounts`, with the native runtime running each job as one of them (`mac-one-program.md`, sections 11 and 13). Step 5 is built: the pool, its adoption and the reset in place went with the root helper `js-mac-session`, which is no longer a program (the text of steps 2 and 4 below says "the helper" where the built code is `mac_accounts` in `js-agent`; `mac-lease-teardown.md` has what the teardown does now).

Before step 2, a native job ran as one of a fixed pool of hidden accounts (`kbf-lease-pool-<slot>`), which the root helper `js-mac-session` reset in place after the job and lent again. That helper and its pool are retired and are no longer in the package (step 5, below). The pool existed because macOS 27 refuses root, without the privacy grant Full Disk Access, the deletion of an account record, of an account's home and of the per-user folders the system placed for it, so nothing of an account could be removed. A reset in place empties what it can and keeps the rest: an empty `~/Library/Preferences` that the system guards, the `/var/folders/<xx>/<hash>` entry with its `0`, `C` and `T`, the record itself and every attribute of it root may not write. A job leaves behind whatever the reset does not reach, and the next job of the same slot inherits it.

The aim is that no job inherits anything of an account: each job gets an account that did not exist before it, and the account, its home and its per-user folders are gone, and shown gone, before its uid can be given to anyone again.

## What changes for an operator

| | Before | After this change (steps 1 and 3) | After step 2 (as designed here; built in another shape, see the status) |
|---|---|---|---|
| Signature of `js-agent`, `js-mac-provision` (and, before step 5, `js-mac-session`) | the linker's ad hoc signature, an identifier that changes with every build | Developer ID Application, team `U7RTX4XD3X`, fixed identifiers, hardened runtime, trusted timestamp | same |
| Full Disk Access | held by nothing | granted to `js-agent` alone by a profile the owner installs through device management | same |
| What the host list says | nothing | the host's capability `full-disk-access`: `granted`, `not-granted` or `undetermined` | same |
| A native job on a Mac without the grant | runs as a pool account that is reset in place | refused before anything is made, `no-full-disk-access` | same |
| A native job on a Mac with the grant | runs as a pool account that is reset in place | same: the reset does not use the grant | runs as a new account, deleted after the job (home, `~/Library/Preferences` and per-user folders included) and shown gone before its uid is used again |

What does not change: the Seatbelt profile every job runs under, the kill of the uid's processes, the leak scan after each job and the taint it sets, the wire between the agent and the job service, Linux hosts.

## Step 1: who holds the grant, and how a host knows it

### The program that holds it

The account operations move into `js-agent` with the helper's work (`mac-one-program.md`, komira-ai/jobscheduler#991), so the program that deletes accounts is `js-agent`, a launchd daemon of its own (`js-agent`). macOS's privacy control judges an access by the responsible process, and a launchd daemon is its own, so the grant names the agent. The profile names `js-agent` already, ahead of the switch release, so the owner installs it once and it holds through the switch: a profile names a program by its path and its signature, not its version. Everything the agent starts for itself (its probes, `pfctl`, `launchctl`, `getconf` as the lease user) is started disclaimed (`spawn-disclaim.md`): the started program answers for itself. A native job and its supervisor are started without the disclaim, so they answer for the agent and hold the grant too; the slot user and the Seatbelt profile are what keep a job from using it beyond the build trees (`mac-one-program.md`). The profile names that one program and no other.

### Signing

On a push to main the package workflow (`.github/workflows/js-agent-macos-pkg.yml`) builds the programs in a job that holds no key, signs them in a job that runs no code of the repository (`sign-code`: environment `pkg-signing`, the secrets `MACOS_APPLICATION_P12` and `MACOS_APPLICATION_P12_PASSWORD`, a keychain of the job's own that only `/usr/bin/codesign` may use, Apple's Developer ID intermediate fetched and held to its pinned SHA-256), then packages the signed programs in a third job that again holds no key. Each program gets a fixed identifier:

| Program | Identifier |
|---|---|
| `js-agent` | `com.komira.js-agent` |
| `js-mac-provision` | `com.komira.js-agent.provision` |

`cargo xtask release check-code-signature <binary> --program <name>` reads a program back and refuses it unless its designated requirement is exactly the pinned one, `identifier "<id>" and anchor apple generic and certificate 1[field.1.2.840.113635.100.6.2.6] /* exists */ and certificate leaf[field.1.2.840.113635.100.6.1.13] /* exists */ and certificate leaf[subject.OU] = U7RTX4XD3X`, signed with the hardened runtime and a trusted timestamp. The package job runs it on every staged program before the package is built. The requirement names no hash, so every release meets it.

### The profile

`packaging/js-agent-macos/mdm/js-agent-full-disk-access.mobileconfig` is one Privacy Preferences Policy Control payload with one `SystemPolicyAllFiles` entry: the agent by its path, `/Library/Application Support/js-agent/bin/js-agent`, and its designated requirement. A program that is not an app bundle is named by its path (Apple's schema for the payload). The path is fixed: the package lays its programs there and an update replaces them in place. The requirement keeps another program put at that path from inheriting the grant. A test renders the profile from the signing table and holds the committed file to it byte for byte.

### Knowing

The agent lists the privacy database's directory (`/Library/Application Support/com.apple.TCC`) itself, which only a process with Full Disk Access may list: success is `granted`, a refusal is `not-granted`, any other error `undetermined` (logged). It opens no file and reads no database. The agent does this at its start, before its first hello, and reports the answer as the detected capability `full-disk-access`, which the job service's host list (`GET /v1/hosts`) shows.

The agent looks once per start: a profile installed while the agent runs takes effect at the agent's next start (an update, or `launchctl kickstart -k system/js-agent`).

## Step 3: without the grant, one mode

A Mac agent that does not hold the grant (`not-granted` or `undetermined`) refuses every native job, by the code `no-full-disk-access`, at admission and before anything is made for it: no account, no journal line, no bundle. The alternative was to keep today's reset as a named weaker mode. It is not kept, because two modes drift and the weaker one could then run by accident, on a Mac whose profile was never installed or was removed, with nothing but a log line to say so. The four Macs that run native jobs will hold the grant; a Mac without it says so in the host list and in the refusal of each job sent to it.

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

A deletion that fails at any step, or whose proof fails, leaves the lease live and its uid held. The helper names the step in its refusal and the agent keeps the lease in its journal and repeats the delete at the next job's end and at its next start. While an account is held, the agent admits one native job fewer at a time (the account takes its slot out of service). Two held accounts taint the host (`account-not-deleted`), which stops all placement on it until an operator has looked: one failure may be a process that came back once; two are a fault of the host. Never is a held uid or slot used silently again.

### A crash between create and delete

- After `created` is written and before the account is made: the start's boot reset finds a live lease with no account, the delete finds no record, the proof holds, `deleted` is written.
- With the account made and the job running or ended: the boot reset deletes it as above (a lease the server still holds for the node is kept until the server lets it go, as the boot reset keeps a held lease today).
- After the record is deleted and before `deleted` is written: the delete is repeated (deleting a missing record is not an error), then the proof, then `deleted`.
- A `kbf-lease-*` account in the range whose lease the ledger does not know (a lost ledger): the boot reset records it as `created` and deletes it.
- The pool's own accounts (`kbf-lease-pool-<n>`) left by a release before step 2: nothing deletes them now. The agent leaves alone a record it did not make (its slots must not overlap their uids, `mac-one-program.md`, section 11), and the helper whose boot reset would have deleted them is retired; an administrator removes them by hand.

## Step 4: what it costs

Measured on macOS 27.0.1, as root, from the helper's log: the reset in place of a pool account takes 42 to 59 ms from the lease's end to the slot free; `sysadminctl -deleteUser` took 70 ms to kill and 3 ms to remove a home before the record's deletion was refused. Nothing has been measured with the grant: neither the time to make an account (record, home, first per-user folder) nor a deletion that succeeds, nor whether the grant lets root remove the `/var/folders` entries the system marks `sunlnk`. Step 2 waits for those three answers. The measurement is a root script on a Mac whose root shell holds the grant: it makes five hidden accounts the way step 2 would, times each make and each delete with its proof, and deletes all of them; without the grant it makes nothing and says so.

Accounts made ahead of time (a few, each used once and then deleted) would take the making off a job's start. They are worth it only if making an account takes a noticeable part of a short job's start; under a second, a job's start already spends more on its bundle and its sandbox. The measurement decides; the default is none.

## Step 5: what goes and what stays

Built, with the retirement of the helper program (komira-ai/jobscheduler#1163, `mac-one-program.md`, section 13).

Went, with step 2:

- the pool: its file, `pool-create`, `pool-status`, the slots as lent users, the adoption of left accounts;
- the reset in place: emptying the home and the per-user folders while keeping them, the list of places kept, and the record write-back (setting the shell, the hidden flag and the primary group back, and refusing a record whose uid or home differs);
- the helper's `full-disk-access` verb, with its socket: the agent checks its own grant in-process (`crates/js-mac-session/src/full_disk_access.rs`, a library module the agent calls).

Stays:

- the Seatbelt profile every job runs under: a new account does not change what a job may reach while it runs (`crates/js-agent/src/native/fence.rs`);
- the kill of the uid and the judgment of what is left (`crates/js-agent/src/mac_accounts/teardown.rs`, `crates/js-agent/src/mac_sweep`);
- the sweep of the shared places and the IPC removal, which reach what the job left outside its account (`crates/js-agent/src/mac_accounts/tree.rs`, `crates/js-agent/src/mac_accounts/ipc.rs`);
- the leak check after each job and the taint it sets, in `crates/js-agent/src/leak_scan.rs`: a login item, a Background Task Management entry or a launch daemon a job left outside its account is not undone by deleting the account. It is not wired in: a Mac agent's post-job check is `NoChecks`, and the helper's scan that fed it is gone.

It fits the one-program design: no new daemon, and no new helper verb: the agent checks its own grant in-process.

## Step 6: the proof

The isolation probe's two-phase persistence half (a job leaves a marker of every kind it may make; a later job looks for each) judges a new account per job: the `find` job runs as another account by construction, and every marker kind must come back `gone`. One row stays open: a POSIX named semaphore outlives every account change until the Mac restarts, because macOS has no interface that lists or removes another process's semaphores by owner; the profile cannot deny creating one without breaking Python's `multiprocessing` locks. A semaphore holds a count, not data.

The tests of the change run on Linux against the fake account host of `js-agent`'s tests (the account's shape, the uid choice, each step's failure, each crash point, the ledger read by the previous release). The root test that installs the package on the hosted macOS runner proves the side it can: the runner's agent is unsigned and holds no grant, so the host list says `not-granted`. A deletion with the grant can only be shown on a Mac with the profile.

## Step 7: the owner's steps

1. Merge; the release on main signs the programs. Nothing on a Mac changes until the profile is installed.
2. In Apple Business, upload `packaging/js-agent-macos/mdm/js-agent-full-disk-access.mobileconfig` as a custom profile and assign it to the Macs that run native jobs. hostkeeper ships its own profile for its own program; one profile can carry both entries (one `SystemPolicyAllFiles` list with two entries), but each repository's test pins its own file, so a merged file would be pinned by neither. Upload the two files in the same sitting: their payload identifiers differ, so neither replaces the other.
3. On each Mac, once the release with the signed programs is installed and the profile is listed (`profiles list` as root): restart the agent (`launchctl kickstart -k system/js-agent`). The agent logs `full disk access: granted`, and the host list shows `full-disk-access: granted`. `codesign --display --requirements - "/Library/Application Support/js-agent/bin/js-agent"` prints the pinned requirement.
4. After step 2 is released: run one native job, then, as root, look for its account by its uid: no record (`dscl . -search /Users UniqueID <uid>`), no home, no directory under `/private/var/folders` owned by the uid (`find /private/var/folders -maxdepth 2 -uid <uid>`).
