# Putting js-agent on a farm node with hostkeeper

js-agent ships as a hostkeeper package named `js-agent`, for `linux-x86_64`, `linux-arm64` and
`macos-arm64`. hostkeeper installs it under `/var/lib/hostkeeper/packages/js-agent/<version>/`,
points `/var/lib/hostkeeper/packages/js-agent/current` at it, links the unit into the system and
runs the agent's update hook. This runbook takes one node from nothing to a registered host. The
package files are described in `packaging/js-agent/hostkeeper/README.md` (Linux) and
`packaging/js-agent-macos/hostkeeper/README.md` (macOS).

A package is public and the same on every host, so it carries no secret and no host setting. You
place three files on the node yourself: the join token, the flags file and the instance file.

Examples use `js.example.com`, `node-5` and `farm`. Replace them.

## 1. What you need

- A client token with permission to mint join tokens, in a file only you can read. It is the token
  of one of the lease server's clients, the one `js` takes as `--token-file`.
- A node with hostkeeper installed and enrolled. hostkeeper's own documents cover that: read
  "Enrolling a node" in its operations documents, and its `ctl` reference. This runbook does not
  repeat them.
- The release version. Take it from the release's `js-agent-index.json`. The version is plain
  `X.Y.Z`, the same one `js-agent --version` prints after the program name.
- Root on the node.
- The release trust entry in section 9, already on the hostkeeper side.

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
(on Linux the package's tmpfiles file makes it mode 0700 when the service is installed).

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

## 4. macOS only: remove the installer package's job

The installer package ran the agent as the job `com.komira.js-agent`. The hostkeeper package's job
is `js-agent`. Remove the old job before hostkeeper links the new one, or two agents would run on
the node.

```
sudo launchctl bootout system/com.komira.js-agent
sudo rm /Library/LaunchDaemons/com.komira.js-agent.plist
```

Move the `--server` and `--pool` of the old plist into `/etc/js-agent/flags`. Keep the token file
where section 3 puts it. The installer package's helper job and its `/usr/local/js-agent` files are
not touched.

The new job keeps running only while `/etc/js-agent/token` exists, so place the token first.

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

Then set the desired version of `js-agent` from the release index:

```
hostkeeper ctl --endpoint https://hostkeeper.example.com:9443 --ca-file ./ca.pem \
    --token-file ./operator-token releases set-desired --service js-agent \
    --release-index "$(cat js-agent-index.json)" --release-index-digest SHA256_OF_INDEX
```

- `--release-index` is the text of `js-agent-index.json`, byte for byte. `--release-index-digest`
  is its SHA-256, 64 lower-case hex characters.
- The version itself goes in the `desired` message, which `ctl` takes only through
  `--request <file|->` (proto3 canonical JSON). See hostkeeper's `ctl` reference for
  `releases set-desired`.
- hostkeeper orders each node the package for its own platform from the index.

hostkeeper's own documents on assignments say when an install is ordered and what holds one back.

## 6. What happens next

1. At its next heartbeat, hostkeeper orders the node to install `js-agent` at the desired
   version. It unpacks the package, points `current` at it and runs the hook's `prepare`.
2. On a first install, `prepare` checks for the join token. It defers with the reason
   `join_token_missing` unless the token file is a regular file, owned by the user running the
   hook (root), with mode 0600 or 0400. hostkeeper retries the install. Fix the file and the next
   attempt goes on. A node whose state root already holds a usable `host-credential` is not
   deferred.
3. When `prepare` passes, hostkeeper switches `current`, runs `cutover`, which starts the agent,
   and then `verify`.
4. The agent says hello to the server with the token and the instance. The server registers the
   host and answers with a host credential. The agent stores it in its state root as the file
   `host-credential`, and sends it from then on. The one-time token is retired by the server and is
   spent.
   - State root: `/var/lib/js-agent/state` on Linux, `/var/db/js-agent/state` on macOS.

Keep `/etc/js-agent/token`. Do not delete it. The unit and the job still pass
`--token-file /etc/js-agent/token`, the agent reads that file again at every start, and a file
that is given and missing stops it. On macOS the job runs only while the file exists.

## 7. Check it worked

The host is registered. `js` has no verb that lists hosts. Read the lease server's `GET /v1/hosts`
(see `docs/http-gateway.md`) with a client token. The host is listed with its host id (`node-5` if
you set `--host-id`), `source: registered` and `state: live` with a recent heartbeat.
`docs/runbooks/first-live-launch.md` shows what to look for.

The unit is active.

On Linux:

```
systemctl status js-agent-crun.service
```

On macOS:

```
sudo launchctl print system/js-agent
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
(`journalctl -u js-agent-crun` on Linux, `/Library/Logs/js-agent` on macOS). The messages:

| message | fix |
|---|---|
| `--flags-file: given more than once` | the unit or the job names `--flags-file` twice |
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

## 9. The release trust entry

hostkeeper installs only what a trusted workflow attested. The channel-sync that stages
js-agent releases needs this entry for the service `js-agent`:

| key | value |
|---|---|
| service | `js-agent` |
| repository | `komira-ai/jobscheduler` |
| workflow | `.github/workflows/artifacts.yml` |
| refs | `refs/heads/main` |

This is the same entry as in `packaging/js-agent/hostkeeper/README.md`, and the same workflow path
hostkeeper's and tierstore's packages are trusted by. It lives on the hostkeeper side, not in this
repository.
