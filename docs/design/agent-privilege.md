# What js-agent runs as when jobs go through the rootless crun runner

Status: **decided: option B** (the owner's ruling): js-agent stays root and runs each job's
`crun` container as a dedicated non-root job user. Option A (a non-root agent with a root helper)
is kept below as the hardening path for later. This document shows both with what the kernel does
for each duty, proven by tests that run in CI. It changes no behaviour of the podman path.

## 1. The question

Today `js-agent` is root. Podman is rootful, the agent hands each run's scratch directory to
the job's mapped ids, it reads `/proc/1/ns`, it sets the scratch quota, it reads root-only
watch directories, and it refuses to start when it is not root (`--container-subuid`:
"the agent must run as root"). The container runner of `js-contain` (`crates/js-contain/src/oci`)
is built the other way: "nothing running as root", every container made by `crun` from a
user's own account, with the job's ids mapped onto that user's subordinate ids.

When the agent launches jobs through that runner, two shapes are possible:

- **A. A non-root agent.** It runs as its own user with an explicit capability set, held
  ambient, and gets what it cannot have from a small root helper unit.
- **B. A root agent, with the jobs run as a dedicated non-root job user.** The agent keeps the
  privileges it has today; `crun` and everything that parses job-controlled data run as the job
  user, which is the account whose subordinate ids the jobs are mapped onto.

The shape changes who may do each duty below. The directory layout (section 5, duty 4) is the
same under both, and is implemented in this change.

## 2. How the claims are proven

`crates/js-agent/tests/privilege/` is an `#[ignore]`d test target. The `agent-privilege` job of
`ci.yml` builds it as the runner user and runs it under `sudo`, after mounting a loop-backed XFS
volume with `prjquota` (the volume is made by the workflow and only read by the tests; a missing
`JS_PRIV_XFS_ROOT` fails a test, never skips it).

For shape A a test starts the test binary again as uid 2000 with gid 2000, no supplementary
groups, and exactly the capabilities the test names: the bounding, permitted, effective,
inheritable and ambient sets are all that list (`harness.rs`). The child runs one probe against
the real kernel and prints what happened; the parent also reads the child's own `CapEff` and
`CapAmb` and fails the test unless they equal the requested set, so a test cannot pass because the
child silently got more. For shape B the test runs in the (root) test process.

Every test name says what it proves. Where a result below is marked **proven**, the test is named
in that row. Where it is marked **UNVERIFIED**, CI cannot show it (it needs the farm's kernel,
AppArmor policy or a systemd unit) and a named later PR or the canary host must.

## 3. Capabilities are not a small privilege here

The tests show that every capability shape A would need is, in practice, root:

| Duty | Capability A needs | What that capability allows |
|---|---|---|
| read `/proc/1/ns` | `CAP_SYS_PTRACE` | ptrace any process of the same user namespace, including root's: a compromised agent injects code into a root process and is root |
| set and read project quota | `CAP_SYS_ADMIN` | mount, namespaces, `quotactl`, and most of root |
| read root-only watch directories | `CAP_DAC_READ_SEARCH` | read any file on the host; `open_by_handle_at` reaches files outside any mount namespace |
| read the firewall, in the egress check | `CAP_NET_ADMIN` | also change it |
| set a project id on a directory the agent gave away | `CAP_FOWNER` | act as the owner of any file |

So "A with ambient capabilities" does not separate privilege: the agent that talks to the lease
server and parses its commands would hold the keys to root, with extra moving parts. A separates
anything only in its **helper** form: the agent holds no capability at all, and a root helper
does the five things above for it. The rest of the document compares B with that form of A.

## 4. Duties, option by option

"A" below means the agent with no capability and a root helper unit; "A (caps)" the agent with
the capability named. The last column names the tests.

### Duty 1. The start check that the agent sees the host (`/proc/1/ns/{pid,mnt}`)

`integrity::require_host_namespaces` reads the namespace links of pid 1 and of itself, and
pid 2's `stat` and pid 1's `cgroup`.

| | Needs | Proven by |
|---|---|---|
| B | nothing (root) | `init_ns::a_root_agent_passes_the_init_namespace_check` |
| A (caps) | exactly `CAP_SYS_PTRACE`, ambient; no other capability stands in for it (`DAC_READ_SEARCH`, `DAC_OVERRIDE`, `SYS_ADMIN`, `CHOWN`, `FOWNER` together fail) | `init_ns::a_non_root_agent_with_only_ambient_cap_sys_ptrace_passes_the_init_namespace_check`, `init_ns::no_other_capability_stands_in_for_cap_sys_ptrace_on_the_init_namespace_check` |
| A (no cap) | the unprivileged agent reads its own links but not pid 1's, and the start check refuses with "cannot tell whether the agent sees the host" | `init_ns::a_non_root_agent_without_capabilities_cannot_read_the_init_namespace_links` |

For A without a capability the helper writes the two link targets of pid 1 once at its own start
to a root-owned, world-readable file, and the agent compares them with `/proc/self/ns/*`. That
file replaces the read, not the other checks (pid 2, pid 1's control group, which need nothing).
This is design only: the code change belongs to the PR that lands the helper.

The same tests establish that the process table, mount table, user and group databases, module list,
pid 2's `stat` and pid 1's `cgroup` all read without any capability
(`init_ns::a_non_root_agent_reads_every_other_host_input_without_a_capability`). A host that
mounts `/proc` with `hidepid=2` shows an unprivileged agent only its own processes, with no
error, which would blind the process checks (they would see no stray process of another user);
nothing here tests for that, and the farm's mount options are UNVERIFIED. A start check on
`/proc`'s mount options belongs with the PR that lands A.

### Duty 2. Post-job watch directories

`SUGGESTED_WATCH_DIRS` names `/root/.ssh`, `/etc/sudoers.d` and others a non-root user cannot
enter. The code path is `ProcHost::watched` through `Integrity`.

| Property | Result | Proven by |
|---|---|---|
| An unreadable watch directory is an **error**, never an empty listing | `Permission denied` is returned | `watch::a_non_root_agent_without_a_capability_gets_an_error_for_a_root_only_watch_dir` |
| At start an unreadable directory stops the baseline: the agent does not start, naming `watch-unreadable` | yes | `watch::an_unreadable_watch_dir_stops_the_agent_from_taking_its_baseline` |
| A directory readable at the baseline and unreadable after a job is an **unchecked** finding with code `watch-unreadable` (the slot is not freed, the host is tainted), run against the real `/proc` | yes | `watch::a_watch_dir_closed_after_the_baseline_taints_the_host_as_unchecked` |
| Under B | root lists a directory that belongs to another user with mode 0 | `watch::a_root_agent_reads_a_closed_watch_dir_of_another_user` |
| Under A (caps) | `CAP_DAC_READ_SEARCH` alone lists a root-only directory; `CAP_FOWNER` and `CAP_CHOWN` do not | `watch::cap_dac_read_search_alone_reads_a_root_only_watch_dir` |
| The real `/root` (parent of `/root/.ssh`) on the runner | closed without the capability, open with it | `watch::the_real_root_home_needs_cap_dac_read_search_for_a_non_root_agent` |

So under A a stock watch list makes the agent **refuse to start**; it is never a silent
degradation. The remedy is the capability (A caps), an ACL that gives the agent search and read on
each watched directory, or (A helper) a helper that watches the root-only directories and answers
"changed or not". One quiet gap exists in the code already, and shape A makes it reachable: a
directory the agent may **list but not search** (`r` without `x`, as a group or other ACL
grant that forgets `x`) is not an error. Its entries cannot be examined, so they are digested by name
only and a rewrite of a file in it is not seen
(`watch::a_watch_dir_the_agent_may_list_but_not_search_is_digested_by_names_only`; with
`CAP_DAC_READ_SEARCH` the same rewrite changes the digest). Any ACL route therefore grants `rx`,
and a follow-up should make a listing whose entries cannot be examined a `watch-unreadable` finding.

### Duty 3. The per-run scratch quota

Today the lifecycle applies an XFS project quota to the empty run directory before it is given to
the job, reads the usage when it claims a slot, and takes the limits off after the tree is removed
(`quota/mod.rs`). Measured on a loop-mounted prjquota XFS volume:

| Call | Kernel interface | A without a capability | A with `CAP_SYS_ADMIN` | B |
|---|---|---|---|---|
| set project id and inherit flag on the run directory | `FS_IOC_FSSETXATTR` | allowed to the directory's owner (the probe does it) | allowed (owner) | root |
| set the limits | `quotactl Q_XSETQLIM` | **EPERM** | allowed | root |
| read a project's usage (`claim`) | `quotactl Q_XGETQUOTA` | **EPERM** | allowed | root |
| take the limits off after the run (`clear`) | `quotactl Q_XSETQLIM` | **EPERM** | allowed | root |
| set the project id on a directory the agent does not own (already given to the job's ids) | `FS_IOC_FSSETXATTR` | EPERM | **EPERM** | root |

The last row needs `CAP_FOWNER` as well. The first row is shown by the agent's own probe, which
reads the quota state, makes a directory and sets a project id on it, and reports `project` to a
non-root agent with no capability.

Proven by `quota::a_non_root_agent_without_a_capability_cannot_set_the_limits_of_its_own_directory`,
`quota::cap_sys_admin_alone_bounds_a_directory_the_agent_owns`,
`quota::reading_the_usage_of_a_project_needs_cap_sys_admin`,
`quota::removing_the_limits_after_the_run_needs_cap_sys_admin`,
`quota::setting_the_project_of_a_directory_the_agent_does_not_own_needs_cap_fowner`, and, for B,
`quota::a_root_agent_bounds_and_clears_a_directory_that_belongs_to_the_job`. Each asserts the limits
the file system holds afterwards, read as root, so a call that returned an error but changed
something would fail it.

Consequences: the whole quota duty needs `CAP_SYS_ADMIN` at **both ends** of every run, not only
at its start, so A means a root helper that takes `(slot, limits)` and applies, reads and clears
them for directories directly below the scratch root (the helper checks that, as `XfsQuota` does);
and the ordering rule of the quota module (apply before the directory is given away) is a
requirement under A, not a nicety: after the give, the project id cannot be set without
`CAP_FOWNER`. Under B nothing changes.

### Duty 4. Directory layout

`ScratchRoot` and `FilesRoot` accept only a root with no permission for group or others
(`mode & 0o077 == 0`), and `js-contain`'s `Oci::new` accepts a scratch path only if **every**
ancestor is searchable by others (`mode & 0o001`), because `crun` makes the bind mounts as the
job's host id, which is not the agent's. Both cannot hold. The kernel's side:

- Before: below a `0700` root the job's ids cannot even `stat` a directory the agent gave them
  (`layout::below_a_private_root_the_job_ids_cannot_reach_their_own_directory`).
- After: below a `0711` root with `0711` lease directories they reach and write their own
  directory, and cannot list the root, cannot list the lease, and cannot read a `0700` directory
  of the agent's in the lease
  (`layout::below_a_searchable_root_the_job_ids_reach_their_directory_and_nothing_else`).

**The rule (implemented here as `DirLayout`).** `Private` is the podman rule, unchanged and the
default. `Searchable` accepts a root that the agent owns, whose mode has no read or write bit for
group or others and the search bit for others (`mode & 0o066 == 0 && mode & 0o001 != 0 && mode & 0o700 == 0o700`, in
practice `0711`: the owner keeps `rwx`, so a `0311` or `0611` root the agent cannot use itself is refused), and whose every ancestor is searchable by others; the agent checks that itself at
start, naming the closed directory, instead of failing at `Oci::new`. The ancestor check
(`require_reachable`) reads mode bits only, as `Oci::new`'s own `search_check` does: it sees no ACL and
no mount flag (`noexec`, `nosuid`, a read-only mount), so a path that the bits allow but an ACL or a
mount denies is found only when `crun` fails. Each run's directory
(`ScratchRoot::create`) is `0711`, the agent's, and holds the subdirectories the agent hands to the job's
ids; the job cannot list the root, so it does not learn the name of another lease, and the other
leases' contents are not visible in its mount namespace anyway. The state root keeps the private
rule (it is not bound into a job).

**Run files (the secrets).** A `Searchable` `FilesRoot` is `0711` and writes each run's directory
`0700` and its files `0400`, the agent's, so a job that has not been handed its files, and every
other user, reads nothing: it fails **closed**. `FilesRoot::hand_over(run, uid, gid)` gives the
directory and files to the job's ids and closes the directory to `0500`. The hand-over is by path:
`lchown` on each entry (which does not follow a link in the last component), then `chmod` and `lchown`
of the directory. That is safe because the root is `0711` and the run's directory is `0700` and the
agent's until the final `lchown`, so no other user can put a link or swap a component while it runs;
it is not a descriptor-relative walk and does not claim to be. After the hand-over the job owns the
directory and can plant links in it, so the removal and the sweep are the places that must not follow
them: they walk by descriptor and unlink the link itself. Proven by
`layout::run_files_are_unreadable_until_handed_over_and_then_readable_by_the_job_alone` (the job's
ids get `EACCES` before the hand-over, read the contents after, a different user gets `EACCES`
after, and nobody lists the root), and
`layout::links_planted_by_the_job_in_its_run_files_are_removed_and_never_followed` (after the
hand-over the job, as its owner, reopens the directory and plants links to a root-owned file, to a
root-owned directory with a file in it, and to `..`, one level down as well; `remove` and `sweep` as
root delete them and the targets keep their content, owner and mode). A 0700 secrets directory therefore stays 0700, and is bound from
a path the job's ids can reach.

Shape differences: B calls `hand_over` itself (it is root). A cannot give files to a subordinate
id without `CAP_CHOWN`; A hands the directory over through `js-contain`'s give helper (the same
helper container that gives the lease directories) and removes it through `Oci::remove_dir`,
because an agent that is not the owner of a `0500` directory cannot remove it. `FilesRoot::remove`
in B is unchanged (root restores the mode as it does now); under A it is expected to fail on a
handed-over directory (the agent neither owns it nor can restore its mode) and must not be used
for it. The unit test `a_handed_over_run_directory_is_removed_and_swept` covers the owner case only. The change to the removal belongs to the PR that lands the
crun runtime (P12a).

**Gate.** Nothing selects `Searchable` yet. `ScratchRoot::open` and `FilesRoot::open` mean
`Private`, exactly as before; the `Lifecycle` construction sites are untouched. The crun runtime
(P12a) and its unit (P14) choose `open_with(.., DirLayout::Searchable)` with the runtime, so a host
on podman is never opened with the wider rule. The before/after unit tests in
`tests/agent/lifecycle/layout.rs` pin both rules:
`the_default_layout_still_refuses_a_root_others_can_search`,
`the_searchable_layout_takes_0711_and_refuses_every_listable_writable_or_closed_mode`,
`a_searchable_root_below_a_directory_others_cannot_search_is_refused_by_name`,
`a_private_scratch_root_makes_each_runs_directory_0700`,
`a_searchable_scratch_root_makes_each_runs_directory_0711`,
`a_private_files_root_writes_a_job_readable_closed_directory`,
`a_searchable_files_root_keeps_the_run_closed_until_it_is_handed_over`,
`a_handed_over_run_directory_is_removed_and_swept`.

### Duty 5. Ownership of the state directory

`StateRoot::open` (and every `private_dir` root) requires a directory the **running user** owns,
with no permission for group or others.

| | Result | Proven by |
|---|---|---|
| B on today's root-owned directory | opens | `state_dir::a_root_agent_opens_a_root_owned_state_dir_and_refuses_the_agents` |
| A on that same directory | refused: "not owned by the user running the agent" | `state_dir::a_non_root_agent_refuses_the_state_dir_of_a_root_agent` |
| A after the directory is handed to its user (mode 0700 kept) | opens | `state_dir::a_non_root_agent_opens_a_state_dir_handed_to_its_user` |
| B on a directory the A-era user owns (a rollback) | refused | same test as the first row |

A is therefore a one-time migration: the unit's `StateDirectory=` (which hands the directory to the
unit's user) or a held script does the chown, and a rollback from A to B hands it back. The scratch
root, run-files root and image store are the same: owned by the agent's user, with the layout above.
The image store (`<scratch>/oci-images`) is made by the daemon user either way.

### Duty 6. Egress

Today the agent reads the firewall back before every job (`nft -j list table`) and refuses the job
when it is not as expected; the network and table are set up by an operator script.

- Read side: without `CAP_NET_ADMIN` a non-root user cannot read the firewall at all
  (`egress::a_non_root_agent_without_cap_net_admin_cannot_read_the_firewall`); with only
  `CAP_NET_ADMIN` it can (`egress::cap_net_admin_alone_reads_the_firewall`); root can
  (`egress::a_root_agent_reads_the_firewall`). The same capability allows rewriting the rules.
- Set-up side: **UNVERIFIED.** What the crun runner needs (a network namespace the job joins, or
  `pasta`, with a rule set the job cannot edit) is the question of the egress spike (P5). Both
  shapes need host privilege for it; A needs it in the helper.

### Duty 7. Running js-contain's `Oci` from a root daemon (option B)

js-contain was written for a non-root daemon. `root_daemon.rs` (js-contain, run under sudo in the
`agent-privilege` job) shows its `Oci` working when the daemon is root and starts every program as
a dedicated job user, with a real `crun` and `newuidmap`:
`a_root_daemon_runs_a_container_mapped_onto_the_job_users_subordinate_ids`, and the privilege drop and
the AppArmor control below. The first proves:

- the user namespace maps container ids `0..65536` onto the **job user's** subordinate range
  (`/proc/self/uid_map` inside reads `0 <first id of that range> 65536`, one line, root unmapped),
  the job is the unprivileged id 1000 with no capability (`CapEff` zero), and the files `crun`
  makes for the lease belong to the job user;
- `Oci::new` accepts a root-owned scratch root of mode `0711` and refuses a `0700` one
  (`NotSearchable`); and a root daemon without the new `OciConfig::job_user` is refused explicitly
  (`SubidError::Root`) rather than by the accident of root having no `subuid` entry. The refusal is
  made in `subids::user_ids` for uid 0 or gid 0 whatever the files hold, and `Oci::new` reaches it for
  `job_user: None` as a root process and for `Some((0, 0))`, `Some((uid, 0))` and `Some((0, gid))`.
  Unit tests: `subids::tests::root_is_refused_as_the_user_whatever_the_files_hold` (the files give root a
  range by name and by number) and `oci::tests::a_root_daemon_or_a_root_job_user_is_refused`.

**The privilege drop is production code, not a test hook.** When `OciConfig::job_user` is set and the
daemon is root, every `crun` the backend starts (the async spawner for `run`, `kill` and the helper
run, and the blocking `delete`, `state` and helper run) is started with `setgroups([])`, then
`setresgid`, then `setresuid` (real, effective and saved) in `pre_exec`, from `/`, with the environment
cleared to `PATH` (`OciConfig::id_tool_path`, where `newuidmap` is found) and `HOME=/`. `XDG_RUNTIME_DIR`
and the rest of the daemon's environment are not inherited. The caller's `scrub_env` still runs first
and a non-root daemon never drops. `every_crun_command_of_a_root_daemon_runs_as_the_job_user_with_a_clean_environment`
runs a stand-in `crun` as a root daemon and reads, from the stand-in itself (`/proc/self/status`), the
ids, groups and environment each of `kill`, `delete` and the helper run had: the job user's three ids
each, no supplementary group, `HOME` and `PATH` only. The keeper and the unpack helper
still need the same drop (P12a): they are not started through `Crun`.

**Every site of js-contain that assumes a non-root daemon:**

| Site | Assumption | Root daemon | Status |
|---|---|---|---|
| `subids::daemon_ids`, `check_daemon_user` (`getuid`) | the daemon user owns the subordinate range | read the job user's range instead: `OciConfig::job_user` (new, `user_ids`) | proven (the test) |
| `crun` and every helper command (`EnvScrub` hook, `Crun::*`) | the daemon user is the one that runs `crun` | `OciConfig::job_user` makes `Crun` drop to the job user (groups, gid, uid, then a clean environment) in `pre_exec` for every command | proven (`every_crun_command_of_a_root_daemon_...`) |
| `Oci::new` `search_check` | every ancestor of scratch is searchable by others (the container's ids are not the daemon's) | holds with a `0711` root; `0700` is refused | proven; the layout rule (`DirLayout::Searchable`) is the minimal change |
| `newuidmap`/`newgidmap` (`id_tool`) | `crun`, running as an unprivileged user, maps its ids through the setuid tools | `crun` runs as the job user, so it does exactly that | proven (the test runs them) |
| Lease directory ownership; the "give" helper container (`helper::give_job`, `Mapping::Setup`) | files the daemon user owns are given to the job's ids from a user namespace that maps both | root `chown`s the lease directories to the job user and the job-written directories to subordinate id 1000 directly; no helper container is needed. The give/clean helpers stay usable | proven for the give (the test does it); the clean (`remove_dir`) is root `unlinkat`, unchanged |
| Cgroup delegation (`delegate::delegate`, `cgroup`) | the daemon owns its delegated subtree and `crun` (the same user) moves the container into `<lease>/ctr`; the kernel needs write on the common ancestor's `cgroup.procs` | the subtree and its control files must be `chown`ed to the job user (a unit's `Delegate=yes` does this only for the unit's `User=`, which is root here) and the root daemon sits in a leaf of it. `delegate()` makes the cgroups as root but chowns nothing | proven by hand in the test (`chown` of the directory and `cgroup.procs`/`threads`/`subtree_control`); `delegate()` must do it (P12a/P14) |
| Keeper (`keeper.rs`) | a process of the daemon's own binary, run as the daemon user, writes `cgroup.kill` of the lease when the daemon dies | root can write `cgroup.kill`; it would run as the job user if started through the same hook, and needs write on the file | UNVERIFIED; the test does not start a keeper; it needs the same drop (P12a) |
| Unpack and helper containers (`helper::unpack_job`, `Mapping::Setup`) | id 0 of the helper namespace is the daemon user | id 0 is the job user, because they are started through the same hook; the stage directory is opened `0777` | UNVERIFIED (the image is pre-unpacked in the test; the unpack mode belongs to the later helper change); it needs the same drop (P12a) |
| `AppArmor` user-namespace restriction | an unconfined unprivileged `crun` may not create a user namespace on newer kernels | **applies**: under B-as-built `crun` runs as the non-root job user, so the restriction hits it. See below | CI control with the restriction ON; the farm's policy is UNVERIFIED |

**AppArmor: the restriction applies to B.** On kernels with `kernel.apparmor_restrict_unprivileged_userns=1`
(Ubuntu 24.04's default, and the hosted runner's) a process that is not root and holds no
`CAP_SYS_ADMIN` gets a user namespace without capabilities unless the AppArmor profile it is confined by grants
`userns`. This contradicts the argument of the egress spike (P5/P10) that a root `crun` is exempt:
under B-as-built `crun` runs as the non-root job user, and the restriction does apply on the farm.

- An earlier revision of this change switched the restriction off in the CI job. A green
  `root_daemon` run in that state is **not** evidence for a host with the restriction on, and the job
  no longer does it: the restriction stays on for the whole job (the runner's default), and the two
  sides are asserted in `root_daemon.rs`.
- Control (a), `a_root_daemon_runs_a_container_mapped_onto_the_job_users_subordinate_ids`: the job
  user runs the distribution's `/usr/bin/crun` and the container starts. Control (b),
  `a_copy_of_crun_is_refused_the_user_namespace_while_the_distribution_path_is_not`: the same binary
  copied to another path fails with the container never run. What CI showed (kernel and profile of
  the Ubuntu 24.04 runner): the copy is unconfined, so the restriction lets it create the namespace
  but strips its capabilities in it, and `crun` stops at its first privileged step with
  `setresuid to \`0\`: Operation not permitted` (exit 1, empty container output); the test asserts that
  message. The distribution's profile on that runner is `profile crun /usr/bin/crun flags=(unconfined)`
  with `userns,`, so it covers our case and no minimal profile of ours is needed there; it first asserts the sysctl reads
  `1`, so it cannot pass on a host where the restriction is off. AppArmor attaches a profile by the
  path of the executable, not by user, which is why (a) passes where (b) fails.
- What js-contain execs: `OciConfig::runtime`, default `/usr/bin/crun`, an absolute path used as given
  (`Oci::new` only checks it is a file). It must stay the distribution's path, never a copy: a copy is
  unconfined by that profile. The test asserts `JS_B_CRUN` is that path.
- The farm still needs, before P12a, either the distribution's `crun` profile (with `userns`) loaded for
  the binary it execs, or a minimal profile for the pinned `crun` path, which P14's held host set-up
  installs, or, as a last resort and a decision for the owner, relaxing the sysctl, which weakens every
  process on the node.

**Residual risk: one identity for all jobs.** Every job runs as the same host user with the same
subordinate id range. `hand_over` and the `0500` mode protect a run's files from other host users, but
not from another concurrent job that escapes its mount namespace: that job has the same host uid. What
bounds the damage then is the path a job would have to guess: run ids are not random. The lease server
mints them as `run-<term>-<count>` (`mint_run_id`, `crates/js-lease-server/src/agents.rs`), a boot
counter and a count that only grows, so they are guessable and are no secret; the root's `0711` mode
(no listing) is the only thing that hides another run's name. A per-job user or a per-job subordinate
range would close it and is not part of B.

Besides these: in B the pulls (`js-oci`) would run as root in the daemon; run them as the job user
(the `image-pull` verb started through the same hook) rather than in the root process.

## 5. Which later pull request takes which option

The owner chose B: take the right-hand column. The left one is the hardening path.

| PR | Under A (agent with no capability, root helper) | Under B (root agent, job-user `crun`) |
|---|---|---|
| P4 spec parameters and hardening | Same for both: the resolved bundle, caps, `noNewPrivileges`, `/tmp` options. `RLIMIT_NPROC` above the daemon's own hard limit is settable only through the unit's `LimitNPROC=` (a non-root process cannot raise its own hard limit) | Same; `RLIMIT_NPROC` set by the root parent before the uid drop, so the per-cgroup `pids.max` is the only bound |
| P7 quota ordering | Helper does `apply`/`usage`/`clear` (needs `CAP_SYS_ADMIN`, and `CAP_FOWNER` unless applied before the give); the agent passes `(slot, limits)` over a socket | In process, as today; apply before the give |
| P10 egress | Helper creates the network and checks the firewall for the agent | In process as root, once P5 picks the mechanism |
| P12a CrunRuntime core | Runs `Oci` in the agent; hand-over and removal through the helper container; no `chown` in the agent | **Taken (B).** `OciConfig::job_user` and the drop in `Crun` (done in this change); the keeper and the unpack helper still need the same drop; the agent chowns the lease directories, calls `hand_over` and `FilesRoot::remove` directly; chown of the cgroup subtree |
| P14 packaging | Two units (agent, helper), `StateDirectory=`, `Delegate=yes`, the helper's socket and policy | One unit; a job-user account with a subordinate id block; cgroup `chown`; state directory kept root-owned |
| Directory layout | The same `Searchable` layout | The same `Searchable` layout |

## 6. Decision and recommendation

**Option B**, decided. The evidence for it, and for keeping A as the later hardening:

1. **The capability route to A is not a privilege reduction.** `CAP_SYS_PTRACE`,
   `CAP_SYS_ADMIN`, `CAP_DAC_READ_SEARCH`, `CAP_NET_ADMIN` and `CAP_FOWNER` are each root in
   practice (section 3). Only the no-capability agent plus helper is a real separation.
2. **That form costs a new privileged program before the first no-preload job can run**: five
   duties behind request interfaces an unprivileged agent can send arbitrary arguments to, each
   needing its own argument policy.
3. **B is a reduction from today and is shown to work.** The agent is root today with a root
   container engine; under B nothing runs as root for a job, and `crun` and everything it runs
   (helper containers, the unpacker) run as the job user. js-contain's `Oci` runs under a root
   daemon with one config field that makes the backend drop privilege (section 4, duty 7); the quota, watch,
   init-namespace and state-directory duties need nothing new.
4. **B makes the give and the clean simpler**: root `chown`s and `unlinkat`s directly.

Open items for B (section 4, duty 7): the same drop for the keeper and the unpack helper,
`delegate()` chowning the delegated subtree, the keeper and the unpack helper as the job user, the
pull as the job user, and the farm's AppArmor policy (a `crun` profile or a sysctl decision before P12a).

**Hardening path (A).** What A would still give is that an agent compromise (it handles the lease
server's commands) does not become root. Buy it later by moving the five duties behind helper
interfaces one at a time. To keep that open the PRs that follow put quota, egress, the
init-namespace check, watch reads and the files hand-over behind traits that the root and helper
forms both implement (`ScratchQuota` already is one). The A tests in this change fix what each
call needs; the cost to plan for is the helper, the unit pair, and a watch list that gets `rx`
ACLs or goes through the helper.

## 7. What is proven and what is not

Proven in CI (the `agent-privilege` job and the `test` job):

- Duty 1: A needs exactly `CAP_SYS_PTRACE`; nothing else stands in; B passes; every other host input
  needs no capability.
- Duty 2: unreadable watch directory is an error, a refused start, and an unchecked finding after a
  job; `CAP_DAC_READ_SEARCH` suffices; the list-without-search gap is real.
- Duty 3: which quota call needs which capability, with the limits read back as root.
- Duty 4: the before/after kernel behaviour of the layout, the unit tests of both rules, and the
  fail-closed run files and hand-over.
- Duty 5: the state directory ownership in both directions.
- Duty 6 (read side): `CAP_NET_ADMIN` is the cost of reading the firewall.

- Option B end to end with a real `crun`, with the AppArmor restriction on: the namespace maps onto the
  job user's subordinate range, the job is unprivileged, `Oci::new`'s search check against a `0711`
  root, and the control that a copy of `crun` is refused (duty 7). The privilege drop of every `crun`
  command, and the explicit refusal of a root job user.

**The tests, by what they do.** The `privilege` target has 28 tests plus the `child` harness entry
(not a test of its own); `root_daemon` has 3. Those that **discriminate the feature** (they fail if the
change is wrong): `layout::below_a_searchable_root_the_job_ids_reach_their_directory_and_nothing_else`,
`layout::run_files_are_unreadable_until_handed_over_and_then_readable_by_the_job_alone`,
`layout::links_planted_by_the_job_in_its_run_files_are_removed_and_never_followed`, all three of
`root_daemon`, and the unit tests named above (`DirLayout::allows`, the refusal of root, the drop's
environment, the lifecycle layout tests). Those that **characterize the kernel or existing behaviour**
(they show what each shape needs and would pass whatever this change does): `layout::below_a_private_root_...`,
every test of `init_ns`, `watch`, `quota`, `egress` and `state_dir`.

UNVERIFIED (left to the later PRs, P5/P10 and the canary host): the keeper and the unpack helper
as the job user; `delegate()` doing the cgroup `chown` itself; the farm's AppArmor policy (CI shows only that the distribution's profile admits `/usr/bin/crun` on the runner); the
unit's `Delegate=` and `AmbientCapabilities=` (A); a `hidepid` process
table; the egress set-up; the `--scratch-quota` flag on the farm's unit (whether the farm sets it is
not known from this repository); the farm's kernel and AppArmor policy against any of the above
(hosted runners differ).
