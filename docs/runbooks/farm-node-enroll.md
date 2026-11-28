# Putting js-agent on a farm node with hostkeeper

js-agent ships as a Debian package, `js-agent`, for `amd64` (`linux-x86_64`) and `arm64`
(`linux-arm64`), in the apt repository at `https://packages.komira.ai/apt` (suites `testing` and
`stable`). hostkeeper's `apt` installer installs it from the `stable` suite, pinned by the
package's SHA-256, and runs the agent's update hook (`/usr/bin/js-agent update-hook`) around the
install. This runbook takes one Linux node from nothing to a registered host. The package's files
are described in `packaging/js-agent/deb/README.md`. There is no native package for macOS yet
(docs/release.md, "macOS"): a Mac runs the agent from the installer package
(`packaging/js-agent-macos/README.md`) and is not updated by hostkeeper.

A package is public and the same on every host, so it carries no secret and no host setting. You
place the files on the node yourself: the flags file, and the host certificate and key, which
hostkeeper keeps on a node it manages (section 2). A node is admitted by its host certificate
alone: a request without a verified host certificate is refused.

Examples use `js.example.com`, `node-5` and `farm`. Replace them.

## 1. What you need

- A node with hostkeeper installed and enrolled. hostkeeper's own documents cover that: read
  "Enrolling a node" in its operations documents, and its `ctl` reference. This runbook does not
  repeat them.
- The release version, plain `X.Y.Z` (the release's tag without the `v`), the same one
  `js-agent --version` prints after the program name.
- Root on the node.
- What the agent's start check needs and the package does not carry: `crun` and `uidmap` from
  the distribution, the scratch mount (XFS with `prjquota`: see "No job starts" in section 7)
  and, where user namespaces are restricted, crun's AppArmor
  profile ("What the host provides" in `packaging/js-agent/deb/README.md`). The package makes
  the job user `jsjob` and its subuid and subgid ranges itself, and keeps ranges a host has.
- The node's repository entry of the stable suite, `komira-stable` (section 8), which
  hostkeeper's coordinator pushes to every Linux host from its registry of repositories.

## 2. Place the files on the node

Do this as root, before you assign the service. Create `/etc/js-agent` first if it does not exist
(the package's tmpfiles file makes it mode 0700 at boot; the agent's unit does not make it).

The flags file, `/etc/js-agent/flags`. The unit passes `--flags-file /etc/js-agent/flags` to the
agent, and the update hook reads the same file. The rules:

- One flag per line, each starting with `--`.
- Lines that are empty or start with `#` are skipped. Spaces around a line are trimmed.
- No line may be `--flags-file`, alone or with `=`. A file does not include a file.
- At most 64 KiB, UTF-8 text.
- A regular file (a link is followed), owned by root or by the user running the agent, and not
  writable by group or others.

```
# /etc/js-agent/flags
--server=https://js.example.com:8443
--pool=farm
--host-cert=/var/lib/hostkeeper/identity/host.crt
--host-key=/var/lib/hostkeeper/identity/host.key
--host-id=node-5
--ca-cert=/etc/js-agent/ca.pem
```

`--server`, `--pool`, `--host-cert` and `--host-key` are required. `--host-id` and `--ca-cert` are optional (a host of a host group may give `--host-id-from-facts=/var/lib/hostkeeper/facts.json` instead of `--host-id`, never both). Set the mode
and owner:

```
chown root:root /etc/js-agent/flags
chmod 644 /etc/js-agent/flags
```

The server's name and its address. Once a connection to the server has completed its TLS
handshake, the agent sends its heartbeats, dispatch polls and job reports to the address that
handshake completed on. It sends there without looking the name up, so a node whose resolver
cannot answer for a while (the name is in a public zone and the site's uplink is down) stays
registered. The agent looks the name up again in the background, about once a minute, and moves
to a new address only after a handshake with it completes. The certificate is always checked
against the name in `--server`. The agent's journal shows one line when lookups start failing
(`cannot look up NAME: ...; requests go to ADDRESS ...`), one when they answer again
(`NAME is looked up again: [...]`), and one when it moves (`NAME moved from OLD to NEW ...`).

What the agent remembers is lost when it restarts. To let a node start, or restart, while the name
cannot be looked up, give the address beside the name:

```
--server=https://js.example.com:8443
--server-address=192.0.2.10
```

`--server-address` takes an IP address, uses the port of `--server`, and only goes with an
`https://` server.

When the server moves to a new address:

1. Change the record of the name first. Running agents follow it on their own once the new
   address completes a handshake.
2. Then change `--server-address` in the flags file on each node. The agent reads the file only
   when it starts, so the new value takes effect at the node's next restart.

A node that starts with a stale `--server-address` tries the old address and then looks the name
up. If the old address refuses connections, the first request goes on to the new address. If it
drops what is sent to it, the first request uses up its limit and fails; the next one looks the
name up first. A stale value stops a node only if the name cannot be looked up at that moment
either.

### The host certificate

An enrolled, approved node holds a host certificate. hostkeeper keeps the host's own certificate and private key
at fixed paths it documents and renews both in place before the certificate expires. The flags file
(`/etc/js-agent/flags`), which the host group's config package installs like every other setting,
names the two paths, as in the example above:

hostkeeper does not edit the flags file.

The key is mode 0640, owned by root, in a group made for the purpose (for example
`hostkeeper-identity`) that the agent's account belongs to; hostkeeper may also repoint a symbolic
link for either path when it renews. The agent follows links. The rule for the key is the same for
everyone who reads it: after following links a regular file, owned by root or by the user running
the agent, with no access for others and not writable by its group (0600, 0400, 0640 and 0440
pass; 0644, 0660 and 0604 do not). The certificate file only has to be readable.

With both flags the agent presents the certificate to the lease server in the TLS handshake and
sends no token. It reads the two files again before every request, so a renewal needs no restart;
it switches to a new pair only when the pair is consistent (the certificate parses, the key passes
the rule and goes with it) and otherwise keeps the pair it has. The agent still accepts
`--token-file` and `--instance-file` (installed units pass them), reads neither and warns once for
each at start. The lease server must be `https://` and started with the CA that signs
these certificates (`--agent-client-ca`); a server without it refuses the handshake.

The flags are both or neither. One alone stops the agent with status 2 and names the other. At
start a pair that cannot be used (a missing file, a key that fails the rule, a key that is not the
certificate's) stops it with status 2, naming the file and never its content. `js-agent cordon`
takes `--host-cert` and `--host-key` the same way, and the user running it must be able to read the
key; given the agent's flags file (`--flags-file /etc/js-agent/flags`) it reads them, the server and
the host id from there (`docs/runbooks/cordon-a-node.md`).

A node that hostkeeper does not manage needs a certificate of its own that the server's
`--agent-client-ca` accepts, at paths you name with the two flags. There is no other way to join.

## 3. A node that ran the archive package or the setup script's unit

A node that ran the agent from hostkeeper's archive package (`hostkeeper-archive`, deleted from
hostkeeper) moves to the `.deb` on the first order of the native entry: hostkeeper's `activate`
takes away the units and files the archive version placed, then installs the `.deb`.

A node set up by the held setup script (`crun_host_setup.sh`) has its own copy of the unit in
`/etc/systemd/system`, which would override the package's in `/usr/lib/systemd/system`. Remove
`/etc/systemd/system/js-agent-crun.service`, `/etc/systemd/system/js-agent-crun-gpu.service`,
`/etc/tmpfiles.d/js-agent-crun.conf` and any `/etc/systemd/system/js-agent-crun.service.d`
override that sets `ExecStart=`, and move `--server` and `--pool` into `/etc/js-agent/flags`
(section 2), before you assign the service.

## 4. Assign the service and set the version

hostkeeper's coordinator decides which services a node runs. All the `hostkeeper ctl` commands
below take the coordinator's address, its CA and an operator token file:

```
hostkeeper ctl --endpoint https://hostkeeper.example.com:9443 --ca-file ./ca.pem \
    --token-file ./operator-token services assign --node-id node-5 --service js-agent
```

You can instead name the service when you approve the node's enrollment:

```
hostkeeper ctl --endpoint https://hostkeeper.example.com:9443 --ca-file ./ca.pem \
    --token-file ./operator-token enrollment approve --request-id REQUEST_ID \
    --key-sha256 KEY_SHA256 --host-id node-5 --role farm --services js-agent
```

The desired version of `js-agent` is set by this repository's
`.github/workflows/hostkeeper-desired.yml`, which each release run starts once the release reached the stable suite, once
the repository variable `JS_AGENT_SETS_DESIRED` is `true` (docs/release.md, "Deploying through
hostkeeper"). To set a version by hand, rollback included, run that workflow from main with the
version ("Run workflow"): it checks that the stable suite lists the release's two `.deb`s with the
digests of its `SHA256SUMS` and sends the release's targets.

By hand with `ctl`, the request is the version and the targets that
`cargo xtask release desired --repo komira-ai/jobscheduler --version X.Y.Z --after-run "" --dir DIR`
prints (`targets=...`): one `apt` entry per Linux platform, naming the repository entry
`komira-stable`, the Debian version `X.Y.Z`, the `.deb`'s SHA-256 and the manifest (the hook
`/usr/bin/js-agent update-hook`, its bounds, the run unit `js-agent-crun.service` and the hook's
params):

```
hostkeeper ctl --endpoint https://hostkeeper.example.com:9443 --ca-file ./ca.pem \
    --token-file ./operator-token releases set-desired --service js-agent --request ./request.json
```

where `request.json` is `{"desired": {"version": "X.Y.Z"}, "targets": TARGETS}`. hostkeeper orders
each node the entry of its own platform.

hostkeeper's own documents on assignments say when an install is ordered and what holds one back.

## 5. What happens next

1. At its next heartbeat, hostkeeper orders the node to install `js-agent` at the desired
   version. It downloads the `.deb` from the stable suite, checks it against the pin, extracts it
   into the version's directory under `/var/lib/hostkeeper/packages/js-agent/` and runs the hook's
   `prepare` from that copy (`tree/usr/bin/js-agent`).
2. On a first install, `prepare` checks that the node has a way to authenticate: the flags file
   (`/etc/js-agent/flags`) must name `--host-cert` and `--host-key`, the certificate file must be
   there and the key must pass the rule of section 2. Otherwise it defers with the reason
   `join_token_missing` (the name is a shipped wire value, kept), naming the file to fix.
   hostkeeper retries the install. Fix the file and the next attempt goes on.
3. When `prepare` passes, hostkeeper installs the `.deb` with `apt-get` (the package's
   `/usr/bin/js-agent` and its units), holds it, switches `current`, runs `cutover`, which starts
   the agent, and then `verify`.
4. The agent says hello with its certificate, and the server registers the host under the host
   id the certificate names.
   - State root: `/var/lib/js-agent/state`.

## 6. Check it worked

The host is registered. `js` has no verb that lists hosts. Read the lease server's `GET /v1/hosts`
(see `docs/http-gateway.md`) with a client token. The host is listed with its host id (`node-5` if
you set `--host-id`), `source: registered` and `state: live` with a recent heartbeat.
`docs/runbooks/first-live-launch.md` shows what to look for.

The unit is active, and the package is the release's:

```
systemctl status js-agent-crun.service
dpkg-query --show js-agent
```

hostkeeper's view of the node and of the service:

```
hostkeeper admin status
hostkeeper ctl --endpoint https://hostkeeper.example.com:9443 --ca-file ./ca.pem \
    --token-file ./operator-token releases status --service js-agent
```

`hostkeeper admin status` runs on the node and shows the installed services and their versions.

## 7. Troubleshooting

### The flags file is refused

The agent exits with a usage failure and names the file. Read the unit's journal
(`journalctl -u js-agent-crun`). The messages:

| message | fix |
|---|---|
| `--flags-file: given more than once` | the unit names `--flags-file` twice |
| `--flags-file: needs a path` | `--flags-file` is the last word with no path |
| `--flags-file PATH: cannot be read: ...` | the file is missing or unreadable |
| `--flags-file PATH: not a regular file` | the path is a directory, a pipe or the like |
| `--flags-file PATH: writable by group or others (mode NNNN)` | `chmod go-w` the file |
| `--flags-file PATH: owned by user N, not by the user running the agent or root` | `chown root:root` the file |
| `--flags-file PATH: larger than 65536 bytes` | cut the file down |
| `--flags-file PATH: not UTF-8 text` | rewrite the file as UTF-8 |
| `--flags-file PATH: line N: --flags-file may not appear in a flags file` | remove that line |
| `--flags-file PATH: line N: not a flag (every line must start with --)` | start the line with `--`, or comment it with `#` |

A line is also not a flag when it is only `--`.

### The first install keeps deferring

hostkeeper shows the install as deferred, and the hook's reason is `join_token_missing`. The detail names the file to fix: a
certificate or key that is not there, a key that others can read or its group can write, or a key
owned by a user that is neither root nor the user running the hook (section 2). A flags file
that cannot be read (missing, writable by group or others, one of the two flags alone) is a
failure too; the agent itself stops on such a file when it starts, naming it. hostkeeper retries
on its own; there is nothing to restart. `hostkeeper admin status` and
`releases status --service js-agent` (section 6) show where the install stands.

### The server answers `host-certificate-required`

The server refused an agent request that carried no verified host certificate (a 401 with the
body `{"error":"host-certificate-required"}`). Check that the server runs with `--agent-client-ca` and a TLS listener, that the agent's `--server`
is `https://`, and that the certificate and key named by `--host-cert` and `--host-key` are the
host's, signed by that CA.

### The server refuses the hello with shape-not-accepted

The agent exits with status 2 and prints, for example,
`the server refused the hello with status 403 (shape-not-accepted, reason quota-without-pool-disk); not retrying`.
The server logs the same refusal with the host id, the code and the reason. The reason says which
condition failed; fix the pool file (`--agent-pool-config` on the server) or the host's flags, then
restart the one that changed.

| reason | meaning | fix |
|---|---|---|
| `below-floor` | the labels or capabilities the host reports do not meet the pool's `[capabilities]`: a capability the pool names that the host lacks or reports with another value (including `os`, `arch`, `isa_level`, `os_version`, `fuse-jobs` and `scratch-quota`), or GPUs of another class than the pool's `gpu_class`. It is never about amounts: a host that reports less (or more) CPU, memory, disk or GPUs than the pool's `[shape]` joins and is booked at what it reports | make the pool file's `[capabilities]` match what the host reports, or give the node the flags the pool names |
| `quota-default-unfit` | the pool names `scratch-quota` and the host's `scratch-default-gib` is absent, zero, above the wire maximum, or more than the scratch disk the host reports, at which it is booked | set the host's `--scratch-default-gib` (default 2) to at most that disk, or give the node more scratch disk |
| `quota-without-pool-disk` | the host runs with `--scratch-quota`, the pool does not name `scratch-quota`, and either the pool books no disk (`disk_bytes` zero or absent) or the host reports no scratch disk | see below |

For `quota-without-pool-disk` on a farm node, remove `--scratch-quota` from the node's flags file
and restart the agent: the farm allocates no disk per job, so its pools book no disk and its nodes
enforce no per-run scratch quota (each job's `/tmp` and `/var/tmp` is still bounded: see the next
section). Only a pool that does book disk per job takes quota hosts: its file
names `scratch-quota = "project"` under `[capabilities]` and books `disk_bytes` under `[shape]` of
at least the host's `scratch-default-gib` (2 GiB when the flag is not given) and at most the disk
the host reports. A pool that names `scratch-quota` without `disk_bytes` is refused when the
server loads the file.

### No job starts: `quota-unavailable`, the scratch mount has no `prjquota`

Each contained job's `/tmp` and `/var/tmp` is a directory of its own in the scratch root, bounded by
an XFS project quota (`--job-tmp-gib`, 1 GiB by default), whether or not the agent runs with
`--scratch-quota`. An agent whose scratch filesystem cannot enforce a project quota starts, and then
starts no job: it prints `js-agent: warning: the scratch root cannot enforce a project quota, which
bounds each job's /tmp and /var/tmp (...)` at start, reports the finding `quota-unhealthy` every
minute, and refuses every start `quota-unavailable`. It probes again every minute, and takes jobs
once a probe passes.

The fix is the mount option, set once per node. The scratch filesystem is the host's: no package
and no hostkeeper config carries its mount. The file is the scratch mount's unit,
`/etc/systemd/system/var-lib-js\x2dagent-scratch.mount`, and the change is `prjquota` added to the
`Options=` of its `[Mount]` section, keeping the options it has (for example
`Options=noatime` becomes `Options=noatime,prjquota`). XFS takes a quota option only at mount time,
not at a remount, so the filesystem is unmounted and mounted again. One node at a time:

1. Cordon the node for the window with `js-agent cordon --for 1h`, given the node's host id and
   credential flags as `docs/runbooks/cordon-a-node.md` says, and wait until it runs no job (the
   lease server lists none on the host). Do not stop the agent with `systemctl stop` while it runs
   a job; the cordon drains it.
2. Once the node is idle, add `prjquota` to the unit's `Options=` (an editor, as root), then
   reload systemd and restart the mount unit:

   ```text
   systemctl daemon-reload
   systemctl restart 'var-lib-js\x2dagent-scratch.mount'
   ```

   The agent's unit (`js-agent-crun.service`) requires the mount (`RequiresMountsFor=`), and
   systemd restarts a unit whose required unit is restarted, so the restart takes the idle agent
   down with the mount and starts it again after it. (The setup script's step 4,
   `crun_host_setup.sh`, makes the same edit with more checks, but it refuses while the agent's
   unit is active and needs it stopped first: use it only on a node being set up, not on a
   running farm node.)
3. Check: `findmnt -no OPTIONS /var/lib/js-agent/scratch` lists `prjquota` (XFS shows it as
   `prjquota` or `pquota`), `systemctl is-active js-agent-crun` says `active`, and the agent's
   start printed `js-agent: scratch quota: project (...)` (`journalctl -u js-agent-crun`).
4. Let the cordon end (stop the `js-agent cordon` process, or let `--for` pass), then go on to the
   next node.

## 8. Trust: the repository's key and the pin

hostkeeper installs the `.deb` only from a suite whose `InRelease` is signed by a key in the host's
keyring for the repository entry, and only a file whose SHA-256 is the one the order pins (the
digest the release's `SHA256SUMS` lists, which artifacts.yml attested before the release published
it). The repository entry is hostkeeper's configuration: its coordinator's registry entry `komira`
(`https://packages.komira.ai/apt`, suites `testing` and `stable`, component `main`) gives each Linux
host `komira-testing` and `komira-stable`. The release's targets name `komira-stable`. No attestation
is checked on the host: it is checked once, where the release publishes the package
(docs/release.md).
