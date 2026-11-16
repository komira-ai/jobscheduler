# The Mac sandbox as an OCI runtime of its own

Status: proposed, nothing built. `macrun` is a placeholder name.

## What changes

Today only jobscheduler can use its Mac sandbox. The sandbox is a pool of reused macOS users, a Seatbelt profile per run, a pf anchor for egress, and inputs copied in read-only. It sits inside `js-agent` (`crates/js-agent/src/native`, about 3,300 lines) and a root helper daemon (`js-mac-session`), and the two talk over jobscheduler's own lease-keyed protocol.

After this change the sandbox is `macrun`, one binary with its own package. It implements the OCI runtime command line on macOS, so anyone can run `macrun create --bundle DIR ID`, then `start`, `state`, `kill` and `delete`, against a standard bundle. `js-agent` runs Mac jobs through it the way it runs Linux jobs through crun: it writes a `config.json`, runs the binary, and reads `state`.

What does not change: the isolation itself, which is the same users, profile, anchor, copies and TCC disclaim (`docs/design/spawn-disclaim.md`). The job wire, the server, placement, the refusal codes clients see, the VM path, and the rule that pull requests run on Linux only also stay as they are. A Mac still has no root filesystem, reads stay open, and writes stay confined to the run's home.

## 1. The CLI

The commands follow the OCI runtime command line in runc's spelling, which the `runc` client crate (`js-contain` already uses it for crun) can drive. The global flags are `--root DIR` (container state), `--log FILE`, `--log-format json`, and `--config FILE`. The config file holds the host's settings: the pool's uid range and size, the homes directory, the read denies, and the pf anchor. Nothing is read from the environment. The runtime runs as root, as `js-agent` on a Mac already does.

- **`create --bundle DIR [--pid-file F] ID`.**
  - Under the pool lock (`flock`), it runs the boot reset if the boot session changed (`kern.bootsessionuuid`), then lends the lowest free slot. `ID` is recorded and can never be used again.
  - It makes `exec/` and `tmp/` in the slot's home, copies the mounts in, resolves `cwd` and builds the profile.
  - It then forks a monitor in its own session. The monitor forks the process, which takes the slot's uid, enters `cwd`, sets the rlimits and blocks on a fifo.
  - It returns status `created` with a pid. stdio is `create`'s own, passed through.
- **`start ID`.** Opens the fifo. The process `posix_spawn`s itself into `/usr/bin/sandbox-exec -p PROFILE args…`, disclaimed.
- **`state ID`.** Prints the spec's state, which the monitor keeps on disk, so it survives the caller restarting. From `created` on it carries the annotation `ai.komira.macrun.work`, the host path of the working directory. Once stopped it also carries `ai.komira.macrun.exit` (`code:N` or `signal:N`) and pid 0. A reboot ends every container, and the boot reset cleans up after it.
- **`kill ID [SIG]`.** A signal goes to the process group. `KILL` is today's `kill-uid` (`docs/design/mac-lease-teardown.md`): every process of the uid, then a bootout of `user/<uid>`, judged by the outcome. On a stopped container `kill` is an error.
- **`delete [--force] ID`.** Only in status `stopped`; `--force` kills first. It resets the slot as `user-delete` does today, and an unclean reset replaces the user. `/work` is gone after `delete`, so a caller reads its outputs between `stopped` and `delete`.
- **`run`, `list --format json`, `features`, `--version`.** `run` (create, start, wait, delete) is for people and tools; `js-agent` does not use it, because it deletes before the outputs can be read. `list` emits the same objects as `state`. `features` prints the spec's `features.json`, the machine-readable form of section 2. `js-agent` checks the version and features at start, as it checks `crun --version` today.
- **The package's install steps, outside OCI.** `pool create` (today's `pool-create`, run by the package's postinstall) and `pf install|remove` (today's `js-mac-provision` verbs). There is no status or check verb: `create` checks the anchor itself.

There is no daemon, socket or caller code-signature check. The caller is root and runs the binary. The monitor holds the wall limit (`ai.komira.macrun.wall-ms`), as the helper's `deadline_ms` does today. `exec`, `pause`, `resume`, `checkpoint`, `update` and `ps` exit 2 and name what was refused.

## 2. config.json on macOS

runtime-spec has no darwin section. As crun does with its `run.oci.*` annotations, `macrun` reads its own settings from annotations under `ai.komira.macrun.` and passes every other annotation through to `state`. Any field that is present and not honoured below is refused at `create` with exit 2 and a log record naming it. Nothing is ignored silently.

**The policy every run gets.**
- **Read:** everything, except the read denies in `--config` (the caller's state, secrets and mount roots, `docs/design/job-rootfs.md` 5.4).
- **Write:** only the slot's home, `/dev/null` and `/dev/fd`.
- **Copies:** the copied mounts are write-denied.
- **Exec:** anything readable, except mounts with `noexec`.
- **Network:** by level. `off` and `loopback` are Seatbelt rules. `allow-list` means Seatbelt allows the network and the pf anchor, keyed on the pool's uids, limits it to the anchor's endpoints. `open` means no filter.
- **Always denied:** the profile's fixed floor (mach-register, mounts, launchd jobs and the rest of `js-mac-pool`'s `FLOOR`).

| Field | Honoured | Refused |
|---|---|---|
| `ociVersion` | 1.0.0 to 1.2.x | others |
| `root` | `path` `/` with `readonly` true or absent: the host's file system | other paths, `readonly: false` (`rootfs-unsupported-here`) |
| `process.args` | run after `sandbox-exec`, no shell; `args[0]` absolute | relative `args[0]` |
| `process.env` | the job's environment as given, plus `HOME`, `USER`, `LOGNAME` and `TMPDIR` set to the slot's | a value for those four |
| `process.cwd` | `/work` or below, which is `exec/` in the home; walked without leaving it (`cwd.rs`) | outside `/work` |
| `process.user` | any non-zero `uid`/`gid` and `umask`; the process runs as the slot's user, as in a one-entry user namespace | `uid` 0 (privileged work takes the VM path), `additionalGids` |
| `process.rlimits` | `RLIMIT_FSIZE` and `RLIMIT_NPROC` (per uid, so per container). Later `RLIMIT_CPU`, `RLIMIT_CORE`, `RLIMIT_NOFILE` and `RLIMIT_STACK`, each once a `js-mac-probe` probe asserts macOS enforces it | every other type, including ones macOS accepts but does not enforce (`RLIMIT_AS`, `RLIMIT_RSS`) |
| `process.terminal` | `false` | `true`, `consoleSize` |
| `mounts`: `bind`, `ro`, destination below `/work` | copied into `exec/<rest>` (0500/0400, owned by the user, links not followed) and write-denied, as `files.rs` does today; `job-rootfs.md` 5.2 may later make it a link without changing the field | destination `/work` itself or outside it |
| `mounts`: `bind`, `rw` | not yet: waits for 5.2's link with a write grant | until then |
| `mounts`: type `clone` | an APFS clone owned by the user: today's writable bundle trees (`bundle.rs`). The copy-on-write mounts of 5.3 stay unchosen | across volumes |
| mount options | `noexec` (denies `process-exec*` and `file-map-executable`), `nosuid`, `nodev` | others, and every other mount type |
| `ai.komira.macrun.network` | `off`, `loopback`; `allow-list` while the anchor is in force; `open` when `--config` has no anchor | anything else (`egress-unchecked` for `allow-list` without the anchor) |
| `process.noNewPrivileges`, `capabilities`, `apparmorProfile`, `selinuxLabel`, `oomScoreAdj`, `scheduler`, `ioPriority`, `hostname`, `domainname`, `hooks`, `linux`, `windows`, `solaris`, `vm`, `zos` | no darwin equivalent | when present |

`args` and `env` are not rewritten, and `/work` does not exist for the process. A caller uses paths relative to the working directory (the convention of `job-rootfs.md` 5.1), or the `work` path from `state`. Declared ports stay refused until the inbound pf test passes, as they are today.

**What stays in jobscheduler.** The Mac runtime in `js-agent` becomes a driver about the size of `CrunRuntime`'s calls. It keeps what is jobscheduler's: the tool-root check, the network ceiling and its refusal codes, the reserved names of `js_types::env`, `@bundle/` resolution, live logs, and output collection (`outputs.rs`, between `stopped` and `delete`). Restart adoption becomes `list` plus `state`. The journal, the adopt and old-helper code, and the socket client go. A run reported `Gone` because the helper restarted can no longer happen. The read allow-list model and the Xcode survey of `js-mac-pool`, which use `js-bundle` types and which the native runtime does not use, stay in jobscheduler. So do the helper's non-sandbox verbs: the VM user, image set-up and the leak scan.

## 3. A separate repository or a crate here

| | Separate repository | Crate in this workspace |
|---|---|---|
| Release | New work: its own release workflow with a macOS build, a `.pkg`, sums and attestation, modelled on this repository's Mac leg of `artifacts.yml`, `js-agent-macos-pkg.yml` and `macos-probe.yml`. Neither repository publishes its Mac package as a release asset yet, and Macs are not yet installed from packages, so that path is built once for both. | One more binary in the Mac archives and the js-agent `.pkg`. |
| Developer ID signing | The Developer ID Installer secrets are granted to one more repository, one administrator action, and the `sign` job is copied. The binary needs no Application signature, because the socket's code-signature check goes away. | Already set up. |
| Pull request checks | Linux x86_64 only, as here. The profile text, the config mapping, the ledger and the pf rules are pure and already tested on Linux. Real `sandbox-exec`, pool and pf tests run in its release's beta stage. | Unchanged. |
| Review | Small, against a public standard and section 2's table. | Shared with the agent. A cross-change is easy, and so is blurring the CLI boundary. |
| Dependencies | None on jobscheduler crates, as crun depends on no container engine. `disclaim` becomes its own crate there, and jobscheduler pins it by git revision, as it pins `hostkeeper-hook`. | The same rule, held by a test. |
| Versions | `js-agent` requires a minimum `macrun` version and its features. jobscheduler's beta stage runs its Mac tests against the pinned released `macrun`. | One version. |

**Recommendation: a separate repository, reached in two steps.** First, build the tool in this workspace as `crates/macrun`, with a test that forbids it any `js-*` dependency, and prove it through this repository's existing beta stage. Then move it, with its history, to its own repository. By then only the CLI and `config.json` cross the boundary, so the move is mechanical, and the release and signing work is paid once, for something already shown to work. The cost is one release workflow, one secret grant and a second package per Mac. In return the sandbox gets its own cadence and review, and it becomes usable outside jobscheduler.

## 4. Migration, each step shippable

1. **`disclaim` as a crate.** It moves out of `js-mac-session` unchanged. `js-agent`, `js-mac-session` and `js-mac-provision` use it, and `spawn_sites` checks it as before.
2. **The runtime crate.** `crates/macrun` gets section 1's CLI over code moved from `js-mac-pool` (the profile), `js-mac-session` (pool, spawn, teardown, sweep) and `js-mac-provision` (pf). Its ledger is a file of its own, keyed by container id with the same single-use rule. The helper's ledger stays the helper's. `macrun` ships unused.
3. **The driver.** `js-agent` gets `--runtime macrun` beside the native runtime, and the package installs `macrun`. A `macrun` subject for `runtime_suite` is new work, and its cgroup, memory-kill and GPU cases do not apply. `native_isolation` runs against both runtimes on a provisioned Mac. Both runtimes use their own uid ranges, so a host can try `macrun` without the helper's pool.
4. **The switch.** `macrun` becomes the Mac default, and that release ships the helper without its pool. Its sandbox verbs are refused with a message naming `macrun`, and they stay on the wire. The pool's uid range passes to `macrun` through its `pool create`, which resets every user as a boot reset does. The baseline window of the leak scan and the GUI-session exclusion read the pool today. They either move to `macrun`'s ledger or retire with their last caller; that pull request measures which. A cross-version test runs the previous release's helper on a host `macrun` has used, and `macrun` on the helper's state.
5. **Deletion.** `crates/js-agent/src/native` and its journal, `js-mac-pool`'s profile code and the pf part of `js-mac-provision` are deleted.
6. **The move.** `crates/macrun` moves to its own repository with its release workflow. jobscheduler pins `disclaim`, and the Mac package names `macrun` as the runtime to install.

Steps 1 to 3 can land while the Macs take no jobs. Step 4 waits for `native_isolation` to pass under `macrun` on a real Mac.

## 5. Open source

Yes: Apache-2.0, like this repository. It is a generic OCI runtime for macOS that depends on no jobscheduler crate and holds nothing of any one deployment, so it can be published as it is, under the same public-repository rules as this one.
