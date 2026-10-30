# What js-agent runs as when jobs go through the rootless crun runner

Status: decision wanted. This document shows both options with what the kernel does for each
duty, proven by tests that run in CI, and ends with a recommendation. It changes no behaviour
of the podman path.

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
group or others and the search bit for others (`mode & 0o066 == 0 && mode & 0o001 != 0`, in
practice `0711`), and whose every ancestor is searchable by others; the agent checks that itself at
start, naming the closed directory, instead of failing at `Oci::new`. Each run's directory
(`ScratchRoot::create`) is `0711`, the agent's, and holds the subdirectories the agent hands to the job's
ids; the job cannot list the root, so it does not learn the name of another lease, and the other
leases' contents are not visible in its mount namespace anyway. The state root keeps the private
rule (it is not bound into a job).

**Run files (the secrets).** A `Searchable` `FilesRoot` is `0711` and writes each run's directory
`0700` and its files `0400`, the agent's, so a job that has not been handed its files, and every
other user, reads nothing: it fails **closed**. `FilesRoot::hand_over(run, uid, gid)` gives the
directory and files to the job's ids and closes the directory to `0500`. Proven by
`layout::run_files_are_unreadable_until_handed_over_and_then_readable_by_the_job_alone` (the job's
ids get `EACCES` before the hand-over, read the contents after, a different user gets `EACCES`
after, and nobody lists the root). A 0700 secrets directory therefore stays 0700, and is bound from
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

### Duty 7. Starting `crun`, the id maps, and the cgroup subtree (UNVERIFIED here)

These cannot be shown on a hosted runner without the real runtime, and are the first things the
real-crun job (P8) and the canary host must prove:

- B: the agent starts `crun` as the job user: set the groups, gid and uid, and give the job user
  `RLIMIT_NPROC` before dropping (a root parent can raise it; a non-root daemon may not). The
  delegated cgroup subtree (`actions/`) must belong to the job user, because `crun` moves its
  container from the daemon's cgroup into `<lease>/ctr`, and the kernel requires write permission
  on the common ancestor's `cgroup.procs`. A root agent does that with a `chown` of the subtree at
  start; the harness here shows that a root process can start a process as a given user with an
  exact capability set (it is how every shape-A test runs), not the cgroup part.
- A: a unit with `User=`, `Delegate=yes`, `AmbientCapabilities=` empty, and `newuidmap` and
  `newgidmap` on the path (`js-contain` checks both). `apparmor_restrict_unprivileged_userns` on
  the distribution's newer kernels applies to both shapes equally.
- Both: pulls (`js-oci`) run in the agent's process. In B that code would run as root, so the pull
  and the verification of what it fetched should run as the job user (the `image-pull` verb of P9
  started as the job user), not in the root process.

## 5. Which later pull request takes which option

| PR | Under A (agent with no capability, root helper) | Under B (root agent, job-user `crun`) |
|---|---|---|
| P4 spec parameters and hardening | Same for both: the resolved bundle, caps, `noNewPrivileges`, `/tmp` options. `RLIMIT_NPROC` above the daemon's own hard limit is settable only through the unit's `LimitNPROC=` (a non-root process cannot raise its own hard limit) | Same; `RLIMIT_NPROC` set by the root parent before the uid drop, so the per-cgroup `pids.max` is the only bound |
| P7 quota ordering | Helper does `apply`/`usage`/`clear` (needs `CAP_SYS_ADMIN`, and `CAP_FOWNER` unless applied before the give); the agent passes `(slot, limits)` over a socket | In process, as today; apply before the give |
| P10 egress | Helper creates the network and checks the firewall for the agent | In process as root, once P5 picks the mechanism |
| P12a CrunRuntime core | Runs `Oci` in the agent; hand-over and removal through the helper container; no `chown` in the agent | `Oci`'s `crun` is started through a wrapper that drops to the job user; the agent calls `hand_over` and `FilesRoot::remove` directly; chown of the cgroup subtree |
| P14 packaging | Two units (agent, helper), `StateDirectory=`, `Delegate=yes`, the helper's socket and policy | One unit; a job-user account with a subordinate id block; cgroup `chown`; state directory kept root-owned |
| Directory layout | The same `Searchable` layout | The same `Searchable` layout |

## 6. Recommendation

**Take B for the first crun release, and keep the privileged duties behind seams so that A with a
helper can replace them later.**

Reasons, from the evidence above:

1. **The capability route to A is not a privilege reduction.** `CAP_SYS_PTRACE`,
   `CAP_SYS_ADMIN`, `CAP_DAC_READ_SEARCH`, `CAP_NET_ADMIN` and `CAP_FOWNER` are each root in
   practice (section 3). An agent holding them has the same blast radius as a root agent plus a
   capability set to get wrong. Only the no-capability agent plus helper is a real separation.
2. **That form costs a new privileged program before the first no-preload job can run.** The helper
   has to do five things (init-namespace attestation, watched directories, quota apply/usage/clear,
   firewall read and set-up, and the hand-over of run files). Each is a request interface the
   unprivileged agent can send arbitrary arguments to, so each needs its own argument policy, and
   the first no-preload build is already gated on P10 and P12a.
3. **B is a reduction from today.** The agent is root today and talks to a root container engine
   and the jobs run as the engine's mapped ids; under B the agent is root, nothing on the host
   runs as root for a job, and the code that handles job-controlled bytes (`crun`, the unpacker's
   helper containers) runs as the job user. It adds no privileged code; the layout and tests in this
   change are the same under both.
4. **B's open items are small and checkable.** The job-user spawn (groups, ids, `RLIMIT_NPROC`) and
   the cgroup `chown` are UNVERIFIED here but are a few system calls, and P8 proves them on a real
   runtime. The root pull path is avoidable by running the pull as the job user.

What A would still give that B does not: an agent compromise (it handles the lease server's
commands) does not become root. That is worth buying later, after the first release, by moving the
five duties behind the helper interfaces one at a time. To keep that open, the PRs that follow
should put quota, egress, the init-namespace check, watch reads and the files hand-over behind
traits that the root and helper forms both implement; `ScratchQuota` already is one.

If the CEO prefers A now, the cost to plan for is the helper (its protocol and its argument
checks), the unit pair, and a watch list that either gets `rx` ACLs or goes through the helper;
the tests here already fix what each call needs.

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

UNVERIFIED (left to P8, P5/P10 and the canary host): `crun` as the job user from a root parent and
the cgroup `chown` (B); the unit's `Delegate=` and `AmbientCapabilities=` (A); a `hidepid` process
table; the egress set-up; the `--scratch-quota` flag on the farm's unit (whether the farm sets it is
not known from this repository); the farm's kernel and AppArmor policy against any of the above
(hosted runners differ).
