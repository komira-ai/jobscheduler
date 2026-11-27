# Agent hand-over check

What it shows: a js-agent release that rolls onto running hosts hands each host's running jobs
to the new agent instead of draining the host ([crun-host-setup.md](crun-host-setup.md), "Agent
updates while jobs run"). An update that drains cordons the host first and waits for its jobs;
one that hands over places no cordon, and the jobs that ran before the update run after it on
the same host in the same attempt. The check reads that from outside, through the job service:
no host is seen cordoned or tainted while it moves, every host ends on a new agent version, and
no job of those hosts ends lost, failed or failed to start, or is started again.

The check is read-only: it runs only `js host list`, `js job list` and `js job get`. It starts
no roll and changes no host; the roll is started the usual way, apart from it. It is a held step
only because it needs a client token of the job service, so the owner runs it.

It is one check (`crates/js-handover-check`) with two ways to run it, with the same flags,
verdicts and exit statuses:

- the program `js-handover-check`, which needs no Rust toolchain where it runs. The build farm
  builds it (`buck2 build //crates/js-handover-check:release --show-full-output` prints where
  buck2 put it; [farm.md](../farm.md) says how buck2 reaches the farm); copy it to the machine
  that runs the check;
- the verb `cargo xtask agent-handover-check`, from a checkout with cargo.

## Before

- The hosts run a release that can hand over: `js-agent --version` prints `stops: drain
  handover` on each (the first update from a release without that line still drains, and the
  check then fails it by name, as it should).
- The job service lists an agent version for every host to watch. The lease server keeps the
  version a hello gave in the host's row, so a server that has restarted lists it again once a
  heartbeat of that session reaches it, with no new hello. A host lists none when its row was
  written by a lease server that did not keep the version (until the host says hello again), or
  when a restarted server has had no heartbeat from it yet; the check refuses a host with no
  version, since it could not tell whether that host moved. Wait a heartbeat and start it again.
- No host to watch is cordoned: the check refuses one, since a drain could not be told from it.
- Jobs run on every host to watch (start a build large enough to run for the length of the
  roll). A host with no job before shows nothing about a hand-over, and the check ends
  inconclusive for it.
- `js` is on `PATH` (or given with `--js`), and a client token file of the job service is at
  hand.

## Steps

1. With the build running and before the roll starts, start the check, naming each host:

   ```sh
   js-handover-check --server https://<job service> \
     --token-file <client token file> --out-dir <workspace scratch directory> \
     --host <host> --host <host> ...
   ```

   (or `cargo xtask agent-handover-check` with the same flags).

   It prints each host's agent version and the ids of the jobs running on it, then reads the
   hosts every `--interval-secs` seconds (20), at most `--samples` times (360, two hours), until
   every host lists a version other than the one it listed first.
2. Let the roll start, the usual way. The check needs nothing from it.
3. When every host has moved (or the readings are spent), the check reads the running jobs
   again, asks the service about each job that ran before and does not run now on the same host
   in the same attempt, and prints its verdict.

## Reading the verdict

Each host gets lines of its own: the version it moved from and to, the marks it showed and
the readings in which it was not heard as live (neither fails the check), the number of jobs
that ran on it before, how many it kept and how many ended succeeded or cancelled, and the ids
of the jobs running on it after.

- `PASS` (status 0, on stdout): every host moved, none was seen cordoned or tainted, and every job that
  ran before runs still in the same attempt or ended succeeded or cancelled.
- `<host>: FAIL: did not move from <version>`: the host was still on its version, or was not
  listed, when the readings were spent.
- `<host>: FAIL: cordoned at reading <n>: it drained instead of handing over`: the update
  cordoned the host, as an update that drains does. Its update's detail in hostkeeper says why
  it did not hand over (see the conditions in crun-host-setup.md).
- `<host>: FAIL: tainted at reading <n> (<reason>)`: the new agent found the host unclean.
- `<host>: FAIL: job <id> <outcome>`: a job that ran before did not survive: `lost`, `failed`,
  `failed-to-start`, `started-again` (it runs in a later attempt or on another host),
  `not-running:<state>` (it waits to run again in the same attempt), or `unreadable` (the
  service did not answer for it). A job can fail by itself; read its result before calling it
  the update's.
- `FAIL: jobs of these hosts that ended lost, failed or failed to start since the check began`:
  each is named, whether it ran before or started during the roll. "Since the check began" is
  read against the clock of the machine running the check, so run it where the clock is synced
  with the job service's.
- `FAIL: <n> finding(s), each named above` (status 1, on stderr): the verdict when any line
  above says `FAIL`.
- `INCONCLUSIVE` (status 1, on stderr): every host moved and lost nothing, but one ran no job
  before.
- Any other line on stderr (status 1): the check refused to start, or a reading after the roll
  failed; the line says which. Status 2 is a usage error (a host id that is not 1 to 253 letters,
  digits, `_`, `.`, `:` or `-`, a host id given twice, an out-dir that is not a directory). A refused check watched nothing.

The out-dir keeps the evidence: `hosts-before.json` (the first reading of the hosts),
`timeline.tsv` (each reading of each host: version, cordon, mark, state, taint), `outcomes.tsv`
(each job that ran before, with its outcome) and `jobs-ended-badly.jsonl`.
