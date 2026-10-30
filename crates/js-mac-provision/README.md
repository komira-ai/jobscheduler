# js-mac-provision

Limits the egress of a Mac pool's users with a pf (packet filter) anchor keyed on their
uid range. Under `Net::On` a Seatbelt profile cannot name the hosts a process may reach,
so the control sits outside the sandbox: a pool user may connect over TCP to the endpoints
the policy names, to IPv4 loopback, and to nothing else.

```text
js-mac-provision render  --uid-start 600 --uid-count 100 --endpoint 192.0.2.10:443 --endpoint 198.51.100.7:8443
js-mac-provision install --uid-start 600 --uid-count 100 --endpoint ...   # root
js-mac-provision check   --uid-start 600 --uid-count 100 --endpoint ...   # exits 1 on drift
js-mac-provision remove                                                   # root
js-mac-provision launchd-plist --program /usr/local/bin/js-mac-provision --uid-start ... > /Library/LaunchDaemons/com.komira.js.pf-pool.plist
sudo -u <pool user> js-mac-provision probe 169.254.169.254:80             # "refused"
```

Flags shared by the verbs: `--anchor` (default `com.apple/com.komira.js.pool`),
`--state-dir` (default `/var/db/com.komira.js-pf`) and `--pfctl` (default `/sbin/pfctl`).

## The rules

`render` is a pure function of the uid range, the endpoints and the anchor name: the same
input gives the same bytes, endpoints sorted by address then port, repeats once. For the
pool `600-699` and one endpoint it is:

```text
block return out quick inet proto { tcp, udp } from any to 169.254.0.0/16 user 600:699
pass out quick on lo0 inet proto { tcp, udp } from 127.0.0.0/8 to 127.0.0.0/8 user 600:699 keep state
pass out quick inet proto tcp from any to 192.0.2.10 port 443 user 600:699 keep state
block return out quick inet6 proto { tcp, udp } from any to any user 600:699
block return out quick proto { tcp, udp } from any to any user 600:699
```

- Every rule is `quick` (the first match ends the evaluation, so nothing later in the main
  ruleset reopens what the anchor closed) and carries `user`, so it touches no process
  outside the range. `user 600:699` is pf's inclusive range; a single uid renders as `user 600`.
- `169.254.0.0/16` is refused first: the cloud metadata address, `169.254.169.254`, is in
  it. An endpoint inside it is refused when the policy is read, so no later edit opens it.
- `keep state` lets the answers of an allowed connection come back.
- IPv6 is refused for the range, `::1` included.
- A refused connection is `block return`: the pool user's `connect` fails at once with
  "connection refused" instead of hanging to its timeout, and a test can tell pf's refusal
  from a host that is simply not there.
- The policy is refused (nothing is rendered) for an empty range, a range that starts
  below uid 500 (system accounts) or ends past the last uid, an endpoint that is not an
  IPv4 literal with a port (a name would need DNS; IPv6 is denied), port 0, and an endpoint
  that is unspecified, loopback, link-local, multicast or broadcast.

## install, check, remove (root)

- **install** writes the rules to `<state-dir>/pool.pf` (0600, the directory 0700), loads
  them with `pfctl -a <anchor> -f <file>`, takes a pf enable reference if it holds none,
  then runs **check**. It replaces the anchor's rules whole, so it is safe to run again.
  A reload does not touch pf's state table: a connection to an endpoint the new rules
  refuse stays open until it ends.
- **check** is the enforcement question (below, `enforced`): pf is enabled (`pfctl -s info`
  says `Status: Enabled`), the main ruleset (`pfctl -sr`) evaluates the anchor, and the
  anchor holds the rendered rules. pf prints a rule in its own spelling (`port = 443`,
  `flags S/SA keep state`), so the rendered file is loaded into a scratch child of the
  anchor (`<anchor>/check`, which the parent's wildcard does not reach), both are read
  back with `pfctl -a ... -sr`, and the scratch child is flushed. The spelling is pf's on
  both sides, so a difference is a real one. Blank lines and indentation are ignored.
- **remove** runs `pfctl -a <anchor> -F rules`, which flushes the rules of this anchor and
  of nothing else (never the main ruleset, never another anchor), gives back the enable
  token and deletes the state files. Connections already in the state table stay until
  they end, so an established connection to a removed endpoint stays open.

Every `pfctl` call goes through the `Pfctl` trait, so the logic is tested with a fake on
every platform; only `SystemPfctl` starts a process.

### The gate: `enforced`

This crate renders, installs and checks the anchor. It does **not** gate jobs: nothing
stops a `Net::On` job when the anchor is missing. The gate belongs to the Mac native
runtime (the agent side), which does not exist yet. The API it calls is
`js_mac_provision::enforced(&Config) -> Result<(), ProvisionError>`: **call it at agent
start and before every `Net::On` job, and refuse on any error.** `PfDisabled`,
`NotReferenced` and `Drift` say it is not enforced; `Run`, `Failed` and `State` say it
could not be told, which is a refusal too (the Linux agent's `egress-unchecked`).

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
`interval` seconds (default 30). `launchd-plist` prints it, with the whole policy as
arguments, so the daemon reads no file of its own. Running `install` repeatedly is the
design: it puts the anchor back whatever removed it, and an install that ran before the
system's own pf start-up fails (`NotReferenced`) and succeeds on the next run. The host
step puts the plist in `/Library/LaunchDaemons` (owner root:wheel, mode 0644) and
`launchctl bootstrap system <plist>`; to take it down, `launchctl bootout system/com.komira.js.pf-pool`, then `remove`.

### The pf enable token

pf is enabled with `pfctl -E`, which takes a reference on it and prints a token; `pfctl -X
<token>` gives the reference back, and pf turns off only when the last reference is gone,
so this program never turns off a pf something else enabled. `install` takes a reference
only when it holds none (pf off, or no token stored), so the 30-second daemon takes
nothing. It writes the marker `<state-dir>/pf-token.pending` before `-E` and removes it
once the token is in `<state-dir>/pf-token`; a marker left behind means a run died in
between and a reference may be held that no file names. If the token cannot be stored, it
is released at once. A stored token from before a reboot is replaced (pf is off then) and
releasing the old one is ignored. If pf was already on and `-E` prints no `Token :` line,
`install` accepts that when `pfctl -s info` says Enabled and keeps no token (**unverified
on a Mac**). `remove` releases the stored token.

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
  address leaves through `lo0` and meets the rules as any other destination: refused
  unless that address is a listed endpoint.
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
  What pf does with them (the `user` range, the reset a `block return` sends to the local
  socket, the scratch child read back with `-sr`, the output of `pfctl -E` and `-s info`) is shown
  by `tests/pf_as_root.rs`, which a farm Mac runs through a held script.

## The root test

`tests/pf_as_root.rs`, `#[ignore]`d (root, loads pf rules under its own anchor
`com.apple/js-mac-provision-test`). As pool uids it connects, through the program's `probe`
verb, to a listed endpoint (this Mac's own address and a port of a listener the test
opens): connected; to another port of the same address, to `169.254.169.254:80` and to a
documentation address: refused; to loopback: connected. The uids on each side of the
range and a control uid are not refused by anything. After `remove` the pool uid connects again.
