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
place three files on the node yourself: the join token, the flags file and the instance file.

Examples use `js.example.com`, `node-5` and `farm`. Replace them.

## 1. What you need

- A client token with permission to mint join tokens, in a file only you can read. It is the token
  of one of the lease server's clients, the one `js` takes as `--token-file`.
- A node with hostkeeper installed and enrolled. hostkeeper's own documents cover that: read
  "Enrolling a node" in its operations documents, and its `ctl` reference. This runbook does not
  repeat them.
- The release version, plain `X.Y.Z` (the release's tag without the `v`), the same one
  `js-agent --version` prints after the program name.
- Root on the node.
- What the agent's start check needs and the package does not carry: `crun` and `uidmap` from
  the distribution, a subuid and subgid range for the job user `jsjob`, the scratch mount and,
  where user namespaces are restricted, crun's AppArmor profile ("What the host provides" in
  `packaging/js-agent/deb/README.md`). The package makes `jsjob` itself.
- The node's repository entry of the stable suite, `komira-stable` (section 9), which
  hostkeeper's coordinator pushes to every Linux host from its registry of repositories.

## 2. Mint a one-time join token

Mint the token for the instance the node is, named `PROVIDER:INSTANCE_ID`. The provider must be
one the pool admits.

```
js join-token mint --server https://js.example.com:8443 --token-file ./client-token \
    --provider PROVIDER --instance-id INSTANCE_ID --out ./node-5.token
```

- `--out` creates the file new, with mode 0600, and writes the token and a newline. If the file
  exists, `js` refuses before it asks the server for anything. If the mint fails, no file is left.
- `js` prints where the file went and when the token expires. It never prints the token.
- The token is good for 30 minutes unless you pass `--ttl` (at most 24h). Mint close to the
  install, or give a longer `--ttl`.
- Minting again for the same instance replaces its unused tokens.
- Never put the token on a command line, in a shell variable or in a package.

Carry the file to the node by a channel you trust.

## 3. Place the files on the node

Do this as root, before you assign the service. Create `/etc/js-agent` first if it does not exist
(the package's tmpfiles file makes it mode 0700 when the agent's unit starts).

The token, root:root with mode 0600:

```
install -m 600 -o root -g root ./node-5.token /etc/js-agent/token
```

Or feed it on standard input, so that it is never in an argument:

```
( umask 077; cat > /etc/js-agent/token ) < ./node-5.token
chown root:root /etc/js-agent/token
```

Do not echo the token. Delete the minted file from the place you carried it to.

The instance, one line, `PROVIDER:INSTANCE_ID`, the pair you minted for. It is not secret:

```
printf '%s\n' 'PROVIDER:INSTANCE_ID' > /etc/js-agent/instance
```

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
--instance-file=/etc/js-agent/instance
--host-id=node-5
--ca-cert=/etc/js-agent/ca.pem
```

`--server` and `--pool` are required. `--host-id` and `--ca-cert` are optional. Set the mode
and owner:

```
chown root:root /etc/js-agent/flags
chmod 644 /etc/js-agent/flags
```

## 4. A node that ran the archive package or the setup script's unit

A node that ran the agent from hostkeeper's archive package (`hostkeeper-archive`, deleted from
hostkeeper) moves to the `.deb` on the first order of the native entry: hostkeeper's `activate`
takes away the units and files the archive version placed, then installs the `.deb`.

A node set up by the held setup script (`crun_host_setup.sh`) has its own copy of the unit in
`/etc/systemd/system`, which would override the package's in `/usr/lib/systemd/system`. Remove
`/etc/systemd/system/js-agent-crun.service`, `/etc/systemd/system/js-agent-crun-gpu.service`,
`/etc/tmpfiles.d/js-agent-crun.conf` and any `/etc/systemd/system/js-agent-crun.service.d`
override that sets `ExecStart=`, and move `--server` and `--pool` into `/etc/js-agent/flags`
(section 3), before you assign the service.

## 5. Assign the service and set the version

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
`.github/workflows/hostkeeper-desired.yml` after each release that reached the stable suite, once
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

## 6. What happens next

1. At its next heartbeat, hostkeeper orders the node to install `js-agent` at the desired
   version. It downloads the `.deb` from the stable suite, checks it against the pin, extracts it
   into the version's directory under `/var/lib/hostkeeper/packages/js-agent/` and runs the hook's
   `prepare` from that copy (`tree/usr/bin/js-agent`).
2. On a first install, `prepare` checks for the join token. It defers with the reason
   `join_token_missing` unless the token file is a regular file, owned by the user running the
   hook (root), with mode 0600 or 0400. hostkeeper retries the install. Fix the file and the next
   attempt goes on. A node whose state root already holds a usable `host-credential` is not
   deferred.
3. When `prepare` passes, hostkeeper installs the `.deb` with `apt-get` (the package's
   `/usr/bin/js-agent` and its units), holds it, switches `current`, runs `cutover`, which starts
   the agent, and then `verify`.
4. The agent says hello to the server with the token and the instance. The server registers the
   host and answers with a host credential. The agent stores it in its state root as the file
   `host-credential`, and sends it from then on. The one-time token is retired by the server and is
   spent.
   - State root: `/var/lib/js-agent/state`.

Keep `/etc/js-agent/token`. Do not delete it. The unit still passes
`--token-file /etc/js-agent/token`, the agent reads that file again at every start, and a file
that is given and missing stops it.

## 7. Check it worked

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

## 8. Troubleshooting

### The server refuses the token, or says it expired

The server answers `join-token-expired` for a token past its time, `join-token-used` for a token
that already registered another host, and `join-token-instance` for a hello that names another
instance or none. Mint a new token (section 2, with a new `--out` path or after removing the old
file), replace `/etc/js-agent/token` with `install` as in section 3, and check that
`/etc/js-agent/instance` is the pair you minted for and that `--instance-file` is in the flags
file. The agent unit restarts on its own and reads the new file.

A host silent for longer than 24 hours after its first hello also needs a new token.

### Revoke a token

To drop every token bound to an instance, used or not, for example for a node you will not bring
up:

```
js join-token revoke --server https://js.example.com:8443 --token-file ./client-token \
    --provider PROVIDER --instance-id INSTANCE_ID
```

It is idempotent. A revoked token is unknown to the server. A server that admits no agents
answers `join-tokens-unsupported`.

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

hostkeeper shows the install as deferred, and the hook's reason is `join_token_missing`. Check the
token file:

```
ls -l /etc/js-agent/token
```

It must be a regular file, owned by root, mode 0600 or 0400. Place the file with `install` as in
section 3. The hook reads the path from the package's params, which is `/etc/js-agent/token`. hostkeeper retries on its own; there is nothing to restart. `hostkeeper admin
status` and `releases status --service js-agent` (section 7) show where the install stands.

### The server refuses the hello with shape-not-accepted

The agent exits with status 2 and prints, for example,
`the server refused the hello with status 403 (shape-not-accepted, reason quota-without-pool-disk); not retrying`.
The server logs the same refusal with the host id, the code and the reason. The reason says which
condition failed; fix the pool file (`--agent-pool-config` on the server) or the host's flags, then
restart the one that changed.

| reason | meaning | fix |
|---|---|---|
| `below-floor` | the shape, labels or capabilities the host reports do not meet the pool's `[shape]` and `[capabilities]`: less CPU, memory, disk or GPUs than the pool books, another GPU class, or a capability the pool names that the host lacks or reports with another value (including `scratch-quota`) | make the pool file match what the host reports, or give the node the resources and flags the pool names |
| `quota-default-unfit` | the pool names `scratch-quota` and the host's `scratch-default-gib` is absent, zero, above the wire maximum, or more than the pool's `disk_bytes` | set the host's `--scratch-default-gib` (default 2) to at most the pool's `disk_bytes`, or raise `disk_bytes` |
| `quota-without-pool-disk` | the host runs with `--scratch-quota`, and the pool books no disk (`disk_bytes` zero or absent) and does not name `scratch-quota` | see below |

For `quota-without-pool-disk` on a farm node, remove `--scratch-quota` from the node's flags file
and restart the agent: the farm allocates no disk per job, so its pools book no disk and its nodes
enforce no per-job quota. Only a pool that does book disk per job takes quota hosts: its file
names `scratch-quota = "project"` under `[capabilities]` and books `disk_bytes` under `[shape]` of
at least the host's `scratch-default-gib` (2 GiB when the flag is not given) and at most the disk
the host reports. A pool that names `scratch-quota` without `disk_bytes` is refused when the
server loads the file.

## 9. Trust: the repository's key and the pin

hostkeeper installs the `.deb` only from a suite whose `InRelease` is signed by a key in the host's
keyring for the repository entry, and only a file whose SHA-256 is the one the order pins (the
digest the release's `SHA256SUMS` lists, which artifacts.yml attested before the release published
it). The repository entry is hostkeeper's configuration: its coordinator's registry entry `komira`
(`https://packages.komira.ai/apt`, suites `testing` and `stable`, component `main`) gives each Linux
host `komira-testing` and `komira-stable`. The release's targets name `komira-stable`. No attestation
is checked on the host: it is checked once, where the release publishes the package
(docs/release.md).
