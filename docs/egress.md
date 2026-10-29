# Egress limited to a few endpoints (Linux, Podman)

A contained job gets no network by default (`--container-network` unset means `none`).
A job that must reach something (the object store for a presigned PUT and its reads, the
frontend) is given a Podman network whose egress the host's firewall limits to exactly
those endpoints. The pieces:

- `deploy/js-agent/egress-network.sh`: creates the network and installs the firewall
  table. No package carries it yet; when the agent's deb lands, the script, and the unit
  it writes, belong beside the agent's unit (the script writes
  `/etc/systemd/system/js-egress.service` and `/etc/js-agent/egress.nft` itself, so a
  package may instead ship them and run the script's `--apply` from its post-install step).
- `--container-network NAME`: the agent joins every job to that network. At start the
  agent runs `network inspect NAME` through the runtime and refuses to start when it fails,
  so a missing network is one startup error and not a failed `create` for every job.
- `--container-add-host NAME:IP` (repeatable, at most 16): a static host entry for every
  container, passed to `create` as `--add-host`.

Supported: rootful Podman 4.x with netavark, the agent running as root under systemd (the
agent does not support rootless engines). Rootless Podman keeps its networks and its
forwarding in a user namespace (pasta or slirp4netns), where the host's `forward` hook
never sees the packets; this setup does nothing for it.

## Set up a host

```sh
# Dry run: prints the commands, the nft ruleset and the unit. Changes nothing.
deploy/js-agent/egress-network.sh --network jobs --subnet 192.0.2.0/24 \
    --allow 198.51.100.10:443 --allow 198.51.100.20:8443
# The same, for real (as root):
deploy/js-agent/egress-network.sh --network jobs --subnet 192.0.2.0/24 \
    --allow 198.51.100.10:443 --allow 198.51.100.20:8443 --apply
```

Then start the agent with `--container-network jobs` and one `--container-add-host` for
each allowed endpoint's name. Rerunning with the same arguments changes nothing. A network
that exists with another subnet, with DNS on, or as `--internal` is never changed: the
script fails. One egress network per host; running it with other arguments replaces the
firewall table. `--allow` takes `IP:PORT` or `CIDR:PORT` (IPv4, CIDR of `/16` or narrower);
`169.254.0.0/16` is refused.

### Names: DNS is off, so every endpoint is a static entry

The network has no DNS server (`--disable-dns`). A presigned URL names the store by host
name and its SigV4 signature covers the `Host` header, so the job cannot be given an IP
instead: each name a job uses must be given to the agent as
`--container-add-host <name>:<ip>`, and the IP must be one of the script's `--allow` IPs
(or inside an `--allow` CIDR), or the connection is dropped. The same goes for the
frontend. `--add-host` is therefore required for every named endpoint; `host-gateway` is
refused. IPv6 entries are accepted by the flag but the network has no IPv6 route, so they
are useless here.

## What the firewall does

The script loads one nftables table, `inet js_egress`, of its own, with two base chains
at priority -10 (policy accept: only packets from the network's subnet are touched):

- `forward` (a container going anywhere else): drop `169.254.0.0/16` first; accept
  `established,related`; accept a new TCP connection to each allowed `IP:PORT`; drop the
  rest (UDP, ICMP, other ports, other hosts, the internet).
- `input` (a container going to the host itself, by any of the host's addresses, the
  bridge gateway included): accept `established,related`; accept a new TCP connection to
  an allowed `IP:PORT` (this matters only when an allowed IP is the host's own); drop the
  rest. The host's services (SSH, the agent, a metrics port) are unreachable.

Why -10: netavark's default backend on this Podman is iptables (`iptables-nft`), whose
`FORWARD` chain sits at the filter priority 0 and ends in an `ACCEPT` for the network's
traffic; netavark's nftables backend uses priority 0 too. A base chain's `accept` only
ends that chain, and a `drop` in any chain on the hook is final, so netavark's accept
cannot override our drop at any priority. -10 makes ours run first (before the order of
two priority-0 chains, which is not defined, could matter) and drops early. netavark never
edits or removes a table that is not its own.

Persistence: `js-egress.service` (oneshot, enabled) loads `/etc/js-agent/egress.nft` at
boot, before the network and the agent. It is `PartOf` and `ReloadPropagatedFrom`
`nftables.service`, because Ubuntu's `/etc/nftables.conf` begins with `flush ruleset`, and
restarting or reloading that service would otherwise remove the table. Stopping
`js-egress.service` does not remove the table.

### What is not blocked

- An allowed endpoint is reachable by its whole IP:PORT: a job can send anything it can
  read to it. The store and frontend authorise each request; the firewall does not.
- Containers of the network can reach each other only if `br_netfilter` is not loaded
  (the bridge then does not run the `forward` hook for same-subnet traffic). Load it
  (`modprobe br_netfilter`, `net.bridge.bridge-nf-call-iptables=1`) to have the table drop
  container-to-container traffic.
- IPv6: the network is IPv4-only, so a container has no IPv6 route; nothing is added for it.
- A connection already established when the ruleset changes stays accepted until it ends.

## Verify on a host

```sh
nft list table inet js_egress          # the rules above; counters show drops
podman network inspect jobs            # "dns_enabled": false, the subnet, not "internal"
# From a throwaway container on the network (an image already on the host):
podman run --rm --network jobs --pull never IMAGE \
    sh -c 'wget -T 3 -qO- http://169.254.169.254/ ; echo metadata=$?'    # fails (drop counter rises)
podman run --rm --network jobs --pull never --add-host store.example:198.51.100.10 IMAGE \
    sh -c 'wget -T 3 -qO- https://store.example/ ; echo allowed=$?'      # connects
```

The agent's start also checks the network: with `--container-network absent` it exits
naming the flag and the network.
