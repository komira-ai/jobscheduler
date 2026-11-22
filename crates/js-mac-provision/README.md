# js-mac-provision

Limits the egress of a Mac pool's users with a pf (packet filter) anchor keyed on their
uid range. Under `Net::On` a Seatbelt profile cannot name the hosts a process may reach,
so the control sits outside the sandbox: a pool user may connect over TCP to the endpoints
the policy names, to IPv4 loopback, and to nothing else.

```text
js-mac-provision render  --uid-start 600 --uid-count 100 --endpoint 192.0.2.10:443 --endpoint 198.51.100.7:8443
js-mac-provision install --uid-start 600 --uid-count 100 --endpoint ...   # root
js-mac-provision install --flags-file /etc/js-agent/flags                 # root; the agent's own policy
js-mac-provision check   --uid-start 600 --uid-count 100 --endpoint ...   # exits 1 on drift
js-mac-provision remove                                                   # root
js-mac-provision pf-pool-plist                                            # the js-agent package's daemon
js-mac-provision launchd-plist --program /usr/local/bin/js-mac-provision --uid-start ... > /Library/LaunchDaemons/js-agent.pf-pool.plist
sudo -u <pool user> js-mac-provision probe 169.254.169.254:80             # "refused"
```

Flags shared by the verbs: `--anchor` (default `com.apple/com.komira.js.pool`),
`--state-dir` (default `/var/db/com.komira.js-pf`) and `--pfctl` (default `/sbin/pfctl`).

## The rules

`render` is a pure function of the uid range, the endpoints and the anchor name: the same
input gives the same bytes, endpoints sorted by address then port, repeats once. For the
pool `600-602` and one endpoint it is:

```text
block return out quick inet proto { tcp, udp } from any to 169.254.0.0/16 user { 600, 601, 602 }
pass out quick on lo0 inet proto { tcp, udp } from 127.0.0.0/8 to 127.0.0.0/8 user { 600, 601, 602 } keep state
pass out quick inet proto tcp from any to 192.0.2.10 port 443 user { 600, 601, 602 } keep state
block return out quick inet6 proto { tcp, udp } from any to any user { 600, 601, 602 }
block return out quick proto { tcp, udp } from any to any user { 600, 601, 602 }
```

- Every rule is `quick` (the first match ends the evaluation, so nothing later in the main
  ruleset reopens what the anchor closed) and carries `user`, so it touches no process
  outside the range. `user { 600, 601, ... }` lists every uid of the range (macOS pf refuses the `600:699` colon form), so a range holds at most 128 uids; a single uid renders as `user 600`.
- `169.254.0.0/16` is refused first: the cloud metadata address, `169.254.169.254`, is in
  it. An endpoint inside it is refused when the policy is read, so no later edit opens it.
- `keep state` lets the answers of an allowed connection come back.
- IPv6 is refused for the range, `::1` included.
- The blocking rules are `block return`, but on macOS a blocked TCP connect is **not
  refused**: it is dropped silently, so a job's `connect` to an unlisted address waits for
  its own timeout (a probe prints "timed out"). pf builds the reset only after
  `pf_check_proto_cksum` accepts the packet (xnu `bsd/net/pf.c`, `pf_test_rule`), and an
  outbound SYN on an interface set up with TCP checksum assist has not had its checksum
  completed when pf sees it (`pf_inet_hook`, `bsd/net/pf_ioctl.c`). A first run on a Mac
  showed this off the host as well as on the node's own address. Because a timeout looks
  like a host that is not there, the root test proves a block by the blocking rule's packet
  counter (`pfctl -a <anchor> -s rules -vv`), not by the error. The source suggests UDP gets
  an ICMP unreachable (`pf_send_icmp`); no run has shown it.
- The policy is refused (nothing is rendered) for an empty range, a range that starts
  below uid 500 (system accounts) or ends past the last uid, an endpoint that is not an
  IPv4 literal with a port (a name would need DNS; IPv6 is denied), port 0, and an endpoint
  that is unspecified, loopback, link-local, multicast or broadcast.

## install, check, remove (root)

- **install** writes the rules to `<state-dir>/pool.pf` (0600, the directory 0700; every
  state file goes through a temporary file and a rename), loads them with
  `pfctl -a <anchor> -f <file>`, takes a pf enable reference if it holds none, stores what
  `pfctl -a <anchor> -sr` prints right after the load in `anchor.sr`, then runs **check**.
  It deletes `anchor.sr` before it writes `pool.pf` and writes it again only after a
  successful load and readback, so a failed load leaves `check` with no readback to pass on
  (a refusal); it refuses a readback with fewer rule lines than the policy renders (an empty
  `-sr`), and `check` refuses such a stored one.
  It replaces the anchor's rules whole, so it is safe to run again. A reload does not touch
  pf's state table: a connection to an endpoint the new rules refuse stays open until it ends.
- **install --flags-file PATH** takes the policy from the macOS agent's flags file
  (`/etc/js-agent/flags`) instead of the flags above, so that the anchor loaded is the one the
  agent checks: its `--native-network`, `--native-pf-uid-start`, `--native-pf-uid-count`,
  `--native-pf-endpoint`, `--native-pf-anchor`, `--native-pf-state-dir` and `--native-pfctl`
  lines, read as the agent reads them (`src/agent_flags.rs`: a regular file of root's or of the
  user running it, not writable by group or others, at most 64 KiB of UTF-8, one `--flag=value`
  per line; every other line is the agent's and is skipped). It runs `check` first and loads
  nothing when the anchor is in force already, so a run every 30 seconds does not reload the rules
  (and delete the readback) under the agent's own checks; a changed policy fails the check and is
  loaded. A file that is not there, or that sets a network ceiling other than `on`, loads nothing
  and exits 0, as the daemon runs before the host's configuration package arrives. A pf flag
  beside a ceiling other than `on`, a ceiling of `on` without the pool's uids, a value the agent
  would refuse, a flag given twice (`--native-pf-endpoint` aside) and an untrusted file are
  refused. The anchor, the state directory and the `pfctl` are the file's (the agent's defaults
  when it names none), not this run's `--anchor`, `--state-dir` and `--pfctl`: they are the ones
  the agent checks with.
- **check** is the enforcement question (below, `enforced`) and **changes nothing**: pf is
  enabled (`pfctl -s info` says `Status: Enabled`), the main ruleset (`pfctl -sr`) evaluates
  the anchor, the rules `install` stored equal what the policy renders now (so a changed
  policy is a `Drift` until `install` runs), and the live anchor (`pfctl -a <anchor> -sr`)
  equals the readback `install` stored. pf prints a rule in its own spelling (`port = 443`,
  `flags S/SA keep state`), so the live anchor is compared with pf's own earlier readback,
  not with the rendered text. Blank lines and indentation are ignored. Being read-only, any
  number of checks may run at once and beside `install`. `pfctl -sr` opens `/dev/pf`, so it
  needs root: the macOS agent runs as root and calls it; anything else gets a
  failed `pfctl` and a refusal. A missing state file is "could not tell", also a refusal.
- **remove** runs `pfctl -a <anchor> -F rules`, which flushes the rules of this anchor and
  of nothing else (never the main ruleset, never another anchor; a leftover `<anchor>/check`
  child of an earlier version is flushed too, and its absence is no error), gives back the
  enable token and deletes the state files. Connections already in the state table stay
  until they end, so an established connection to a removed endpoint stays open.

Every `pfctl` call goes through the `Pfctl` trait, so the logic is tested with a fake on
every platform; only `SystemPfctl` starts a process.

### The gate: `enforced`

This crate renders, installs and checks the anchor; it does **not** gate jobs itself. The
gate is the macOS agent's (`js-agent`, `--native-network on` with the `--native-pf-*` flags,
which carry the anchor, the uid range, the endpoints and the `pfctl` this crate was
given). It calls `js_mac_provision::enforced(&Config) -> Result<(), ProvisionError>` **at
agent start and before every job that asks for the filtered network (`allow-list`; an
`open` job under `--native-network open` has no filter and is not checked), and refuses on
any error**: the
agent does not start, and the job is refused as `egress-unchecked` and creates nothing.
`PfDisabled`, `NotReferenced` and `Drift` say it is not enforced; `Run`, `Failed` and `State`
say it could not be told, which is a refusal too. The answer is never kept.

### The anchor point and the main ruleset

pf evaluates an anchor only if the main ruleset names it. The stock `/etc/pf.conf` has
`anchor "com.apple/*"`, which reaches every child of `com.apple`, so the default anchor
`com.apple/com.komira.js.pool` needs no edit of a file the OS owns and replaces on an
update. With another anchor name, the operator adds `anchor "<name>"` or
`anchor "<parent>/*"` to the main ruleset and loads it.

That placement has two weaknesses, and the gate, not the interval, is what closes them:

- A `pfctl -f /etc/pf.conf` by anything (MDM, an OS update, a system service) can drop the
  reference or the rules. Egress is then open until the next LaunchDaemon tick. The
  daemon runs `install` every 30 seconds by default, which only shortens the window.
  `enforced` before each job closes it for the jobs.
- A wildcard anchor evaluates its children in name order, so an Apple anchor with a
  numbered name (such as `250.ApplicationFirewall`) is evaluated before
  `com.komira.js.pool`. A `quick` pass there for a pool user's traffic would win over our
  block. The rules here are `quick` but cannot reorder what runs first.

**Open decision for engineering:** our own top-level anchor (`com.komira.js`), referenced
from a main ruleset we load, which holds Apple's rules and then ours. That fixes the
ordering and puts the reference in our hands, and trades it against "do not edit OS
files": we would own a copy of `/etc/pf.conf`'s content, a later OS change to it would not
reach us, and loading a whole main ruleset flushes what others loaded into it. Until that
is decided the default stays under `com.apple/`.

### Persistence: a LaunchDaemon

The rules live in the kernel and are gone at reboot, and an OS update may reload
`/etc/pf.conf`. Rather than a file under `/etc/pf.anchors` plus an include in the main
ruleset (an edit of an OS file), a LaunchDaemon runs `install` at boot and every
`interval` seconds (default 30), with the label `js-agent.pf-pool` (a label of the js-agent
service, as hostkeeper requires of every label a package runs). Running `install` repeatedly is
the design: it puts the anchor back whatever removed it, and an install that ran before the
system's own pf start-up fails (`NotReferenced`) and succeeds on the next run.

The js-agent package for macOS ships the daemon: `pf-pool-plist` prints its fixed plist,
`/Library/LaunchDaemons/js-agent.pf-pool.plist`, which runs
`/Library/Application Support/js-agent/bin/js-mac-provision install --flags-file
/etc/js-agent/flags` at load and every 30 seconds as root (the golden file is
`tests/golden/pf-pool.plist`). The policy is read from the agent's flags file at every run, so the
configuration package that writes the flags file changes the anchor too, within 30 seconds, and
the plist holds no policy. hostkeeper lists the label in the package's `run` labels and enables it,
and the update hook bootstraps it at `cutover`, after it has run the same `install --flags-file`
itself from the staged release, before the agent starts (the hook's `pf_unit` param). After a reboot launchd loads it from `/Library/LaunchDaemons`.

`launchd-plist` prints the daemon for a policy given as flags, with the whole policy as
arguments, for a host without the package; put the plist in `/Library/LaunchDaemons` (owner
root:wheel, mode 0644) and `launchctl bootstrap system <plist>`. To take either down,
`launchctl bootout system/js-agent.pf-pool`, then `remove`.

### The pf enable token

pf is enabled with `pfctl -E`, which takes a reference on it and prints a token; `pfctl -X
<token>` gives the reference back, and pf turns off only when the last reference is gone,
so this program never turns off a pf something else enabled. `install` takes a reference
only when pf is off, or on with neither a stored token (`<state-dir>/pf-token`) nor the
record `pf-token.none` ("`-E` printed no token while pf was on"), so the 30-second daemon
takes nothing. It writes the marker `pf-token.pending` before `-E` and removes it once the
token (or the record) is stored. If the next `install` finds the marker, a run died in
between: a reference may be held that no file names, only a reboot clears it, and `install`
says so on standard error and removes the marker. If the token cannot be stored, it is
released at once. A stored token from before a reboot is replaced (pf is off then) and
releasing the old one is ignored. A `-E` that prints no `Token :` line while pf is
Enabled is accepted and recorded (**unverified on a Mac**, as is whether such an `-E` takes
a reference at all). `remove` releases the stored token.

## What it does not stop

pf by uid is a control on the packets a pool user's sockets send, and it has limits:

- **TCP and UDP only.** pf matches `user` on those two. ICMP and other protocols from a
  pool user are not matched by these rules.
- **Loopback is open to the pool.** pf cannot tell which user owns the far end of a
  loopback connection, so "pool user to another pool user's listener" cannot be refused
  by a rule keyed on the sender. Builds and tests that talk to themselves over 127.0.0.1
  need loopback, so it is passed (IPv4 only) and not narrowed. A pool user can therefore
  reach any listener on 127.0.0.1, including the agent's and the host's own loopback
  services; those must not rely on the network for access control (use a unix socket
  with file modes, or check the peer uid).
- **The node's own address.** A connection from a pool user to the node's own non-loopback
  address leaves through `lo0` and meets the rules as any other destination: blocked
  unless that address is a listed endpoint (and dropped, not refused, like every blocked
  TCP connect).
- **DNS and mDNSResponder.** The anchor lists IPv4 literals, so a pool user needs no
  resolver, but the unix socket to mDNSResponder is not a packet and is outside pf:
  a pool user can ask the system resolver, which sends the query as its own uid
  (not in the range). Names and data can leave through DNS queries that way.
- **Other unix sockets** are outside pf entirely; Seatbelt (see `js-mac-pool`) is the
  control for them.
- **Established connections** keep their state when the anchor loads; they are limited
  from the next connection on.
- **Order in the main ruleset.** A `quick` rule before the `anchor` line that passes the
  pool's traffic is evaluated first.
- **Not verified on a Mac yet.** The rules' text and the logic are tested against a fake.
  What pf does with them (the `user` range, whether a `block return` answers a TCP connect
  at all, the readback and its stability, the output of `pfctl -E` and `-s info`) is shown
  by `tests/pf_as_root.rs`, which `cargo xtask mac-root-tests` runs on a Mac.

## The root test

`tests/pf_as_root.rs`, `#[ignore]`d (root, loads pf rules under its own anchor
`com.apple/js-mac-provision-test`). As pool uids it connects, through the program's `probe`
verb, to a listed endpoint (this Mac's own address and a port of a listener the test
opens): connected; to loopback: connected. To another port of this Mac's own address, to
`169.254.169.254:80` and to a documentation address it must be blocked: the packet counter
of the blocking rule (`pfctl -a <anchor> -s rules -vv`, read before and after) rises and the
connect ends "refused" or "timed out", never "connected" and never another error; a probe
whose counters could not be read is reported `UNPROVEN` and fails. Each row of the printed
table also shows the control uid's outcome to the same target. The uids on each side of the
range and a control uid are not refused by anything. After `remove` the pool uid connects
again.
