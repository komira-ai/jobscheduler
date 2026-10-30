# Egress limited to a few endpoints (Linux, Podman)

A contained job gets no network by default (`--container-network` unset means `none`).
A job that must reach something (the object store for a presigned PUT and its reads, the
frontend) is given a Podman network whose egress the host's firewall limits to exactly
those endpoints. The agent owns the check: it does not trust that the firewall is there, it
reads it back before every job and refuses the job when it is not.

- `deploy/js-agent/egress-network.sh`: an operator step. Creates the network and installs
  the firewall table. No package carries it yet; when the agent's deb lands, the script,
  the unit it writes and this document belong beside the agent's unit (the script writes
  `/etc/systemd/system/js-egress.service` and `/etc/js-agent/egress.nft` itself, so a
  package may instead ship them and run the script's `--apply` from its post-install step).
  Folding the script into the agent is a known follow-up; it stays a dry-run-by-default
  shell helper for now.
- `--container-network NAME`: the agent joins every job to that network.
- `--container-egress-allow IP:PORT` (repeatable, at most 16; `CIDR:PORT` too): the
  endpoints the network may reach, the same list as the script's `--allow`. **Required**
  with a named network: the agent does not start without it, so a named network never runs
  unchecked. Podman only (a Docker host with these flags does not start).
- `--container-nft PATH`: the `nft` program of the check; by default `/usr/sbin/nft`. It
  must exist and be executable when the agent starts, like `--container-bin`.
- `--container-add-host NAME:IP` (repeatable, at most 16): a static host entry for every
  container, passed to `create` as `--add-host`.

Supported: rootful Podman 4.x with netavark, the agent running as root under systemd (the
agent does not support rootless engines). Rootless Podman keeps its networks and its
forwarding in a user namespace (pasta or slirp4netns), where the host's `forward` hook
never sees the packets; this setup does nothing for it.

## The check, before every job is created

At start, and again before every contained `create` (a refused start creates nothing and
ends `failed_to_start` with the detail `egress-unchecked`; the reason is logged on the host
and never sent), the agent runs `podman network inspect NAME` and `nft -j list table inet
js_egress`, parses both as JSON (no text scraping), reads
`/proc/sys/net/bridge/bridge-nf-call-iptables`, and requires:

- the network is a `bridge`, `dns_enabled` is false, `ipv6_enabled` is false, it has exactly
  one subnet and it is IPv4, and it names an interface `B` (a missing field is a refusal);
- no `--container-egress-allow` endpoint lies inside that subnet (a neighbour or the
  gateway);
- `br_netfilter` is active (the file reads `1`);
- the table exists, its `forward` and `input` chains are filter chains on their hooks at
  priority -10, and they hold exactly the rules the script writes, in this order, for `B`,
  the subnet `S` and the endpoints:
  - `forward`: drop all IPv6 from `B`; drop `169.254.0.0/16`; accept `established,related`;
    one accept of a new TCP connection from `S` to each endpoint (any order); drop the rest
    from `B`;
  - `input`: the same without the metadata drop;
  - a rule missing, an extra rule (a wider allow, another port), a missing final drop, a
    rule in another chain, or an expression the check does not know, is a refusal.

An nft that fails (a table that is not loaded makes `nft list` exit 1) is a refusal too. No
error carries what podman or nft printed.

The calls add to each start; the worker's stall bound counts a start as three calls
(`CALLS_PER_START`), so a slow `nft` does not trip the watchdog.

## Set up a host

```sh
# Dry run: prints the commands, the nft ruleset and the unit. Changes nothing.
deploy/js-agent/egress-network.sh --network jobs --subnet 192.0.2.0/24 \
    --allow 198.51.100.10:443 --allow 198.51.100.20:8443
# The same, for real (as root):
deploy/js-agent/egress-network.sh --network jobs --subnet 192.0.2.0/24 \
    --allow 198.51.100.10:443 --allow 198.51.100.20:8443 --apply
# br_netfilter, once (persist it in /etc/modules-load.d and /etc/sysctl.d):
modprobe br_netfilter && sysctl net.bridge.bridge-nf-call-iptables=1
```

Then start the agent with `--container-network jobs`, the same endpoints as
`--container-egress-allow`, and one `--container-add-host` for each endpoint's name.
The script creates the network with `--disable-dns --interface-name jsbr0` (`--interface`
changes the name), checks the ruleset with `nft -c` on a temporary file before it creates
the network or replaces the copy the next boot loads, and fails without changing anything
when a network of that name exists with another driver, interface, subnet, DNS on or IPv6.
One egress network per host; running it with other arguments replaces the table.
`--allow` takes `IP:PORT` or `CIDR:PORT` (IPv4, a CIDR of `/24` or narrower); an address
inside the network's subnet, the gateway, and `169.254.0.0/16` are refused. An address of
this host is allowed (the store may be local): such an endpoint is reachable on exactly
that port through the `input` accept.

### Names: DNS is off, so every endpoint is a static entry

The network has no DNS server. A presigned URL names the store by host name and its SigV4
signature covers the `Host` header, so the job cannot be given an IP instead: each name a
job uses must be given to the agent as `--container-add-host <name>:<ip>`, and the IP must
be one of the `--allow` endpoints (or inside an `--allow` CIDR), or the connection is
dropped. The same goes for the frontend. `host-gateway` is refused.

## What the firewall does

One nftables table, `inet js_egress`, two base chains at priority -10, policy accept; every
rule matches the bridge (`iifname`), so only the egress network's packets are touched, and
a network of another kind or subnet is outside the rules (and refused by the check).

Why -10: netavark's default backend on this Podman is iptables (`iptables-nft`), whose
`FORWARD` chain sits at the filter priority 0 and ends in an `ACCEPT` for the network's
traffic; netavark's nftables backend uses priority 0 too. A base chain's `accept` only
ends that chain, and a `drop` in any chain on the hook is final, so netavark's accept
cannot override our drop at any priority. -10 makes ours run first and drops early.
netavark never edits or removes a table that is not its own.

Persistence: `js-egress.service` (oneshot, enabled) loads `/etc/js-agent/egress.nft` at
boot, before the network and the agent, and `js-agent.service` requires it
(`RequiredBy=`): if the rules fail to load, the agent does not start. It is `PartOf` and
`ReloadPropagatedFrom` `nftables.service`, because Ubuntu's `/etc/nftables.conf` begins with
`flush ruleset`. Stopping it runs `nft delete table inet js_egress` (only that table). If
the table disappears while the agent runs (`nft flush ruleset`, a firewalld restart), the
next job is refused by the check.

## Threat model: what is and is not blocked

Blocked, for a job on the network: the internet, other hosts, the host's own services
(input is dropped except new TCP to an allowed `IP:PORT`), `169.254.0.0/16` (cloud
metadata), DNS (no resolver, UDP dropped), ICMP, every other port of an allowed IP, and all
IPv6: the container is created with `--sysctl net.ipv6.conf.all.disable_ipv6=1` (no
link-local address), the network has no IPv6 subnet (refused otherwise), and the table drops
`meta nfproto ipv6` from the bridge in `forward` and `input`.

Between jobs: with `br_netfilter` active, traffic from one container to another on the
same bridge runs the `forward` hook; its destination is not an allowed endpoint (an
endpoint in the subnet is refused), so the final drop takes it, over IPv4 and IPv6. Without
`br_netfilter` that traffic is switched on the bridge and never meets the hooks, which is
why the agent refuses jobs while it is off.

Not blocked:

- An allowed endpoint is reachable by its whole IP:PORT: a job can send anything it can
  read to it. The store and frontend authorise each request; the firewall does not.
- A connection already established when the ruleset changes stays accepted until it ends.
- ARP and other L2 traffic on the bridge (the bridge-family hooks are not used).
- A job with `CAP_NET_ADMIN` could change its own network; contained jobs drop all
  capabilities.

## Verify on a host

```sh
nft list table inet js_egress          # the rules above; counters show drops
podman network inspect jobs            # bridge, "dns_enabled": false, "ipv6_enabled": false
cat /proc/sys/net/bridge/bridge-nf-call-iptables    # 1
# From a throwaway container on the network (an image already on the host):
podman run --rm --network jobs --pull never IMAGE \
    sh -c 'wget -T 3 -qO- http://169.254.169.254/ ; echo metadata=$?'    # fails (drop counter rises)
podman run --rm --network jobs --pull never --add-host store.example:198.51.100.10 IMAGE \
    sh -c 'wget -T 3 -qO- https://store.example/ ; echo allowed=$?'      # connects
```

With the table removed (`nft delete table inet js_egress`), the agent refuses the next job
with `egress-unchecked`; with `--container-network absent` it does not start.

## Egress on a Mac pool (pf)

A Mac has no network namespace, and a Seatbelt profile cannot allow-list remote hosts (its
`remote ip` filter takes only `*` and `localhost`). The control on a Mac is a pf anchor keyed
on the uid range of the pool's users, loaded by the host step `js-mac-provision`
(`crates/js-mac-provision`, its README is the full text). A pool user may connect over TCP
to the endpoints the policy names (IPv4 `host:port` literals, the store and the frontend),
and to IPv4 loopback; everything else over TCP and UDP is refused at once, the whole of
`169.254.0.0/16` (the cloud metadata address) first, and IPv6 is denied.

- The rules are a pure function of the uid range, the endpoints and the anchor name;
  a range that reaches below uid 500, an empty range and a non-IPv4 endpoint are refused.
- `install` loads the anchor under its own anchor point, enables pf with a counted token,
  checks that the main ruleset evaluates the anchor and reads the rules back; `check`
  compares the loaded rules with the rendered ones; `remove` flushes only this anchor.
  A LaunchDaemon runs `install` at boot and on an interval.
- Unlike the Linux check above, nothing reads the rules back before each job yet: `check`
  is the verb a caller can run (the agent's use of it is a follow-up).
- Limits: pf matches `user` on TCP and UDP only; loopback between pool users cannot be
  narrowed by the sender's uid and stays open; the node's own address is refused unless it
  is a listed endpoint; DNS through mDNSResponder (a unix socket) is outside pf; and the
  behaviour on a Mac is shown only by the root test, `tests/pf_as_root.rs`, which a farm
  Mac runs through a held script.
