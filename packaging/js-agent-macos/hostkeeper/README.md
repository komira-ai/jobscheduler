# js-agent as a hostkeeper package (macOS)

`cargo xtask release hostkeeper-package --target aarch64-apple-darwin` builds
`js-agent-<version>-macos-arm64.tar.zst` from the files in this directory and the release's
signed `js-agent` binary, which the package carries byte for byte. The Linux packages and the
release index are described in `packaging/js-agent/hostkeeper/README.md`.

## What the package holds

| file | placed by hostkeeper at | |
|---|---|---|
| `bin/js-agent` | `/var/lib/hostkeeper/packages/js-agent/current/bin/js-agent` | the release binary |
| `units/js-agent.plist` | `/Library/LaunchDaemons/js-agent.plist` | the agent's launchd job |
| `manifest.toml` | (read by hostkeeper) | the hook, its bounds, the run label and the hook's params |

## The job

The label is `js-agent`, not the installer package's `com.komira.js-agent`: hostkeeper requires a
launchd job's label to be the service's name (or to start with `js-agent.`), and takes the job
from `units/<label>.plist`. Every other key is the one `js-mac-provision agent-plist` renders for
the installer package's job: `UserName` root, no `RunAtLoad`, `KeepAlive` while
`/etc/js-agent/token` exists, a 30 second `ThrottleInterval`, a 120 second `ExitTimeOut` and the
logs under `/Library/Logs/js-agent`. The arguments differ: the program is run through
hostkeeper's `current` link, and `--flags-file /etc/js-agent/flags` takes the place of
`--server` and `--pool`. The root helper (`js-mac-session`) stays with the installer package.

## What the operator places

As on Linux (see the Linux README), root's: `/etc/js-agent/token` (the join token, mode 0600) and
`/etc/js-agent/flags` (one flag per line, with `--server` and `--pool`, not writable by group or
others), and `/etc/js-agent/instance` when a one-time token is used. The job starts only while
the token file exists.

## Moving a Mac from the installer package

The installer package's job is `com.komira.js-agent`, run from `/usr/local/js-agent`. Remove it
before the first install, or two agents would run on the host:

```
sudo launchctl bootout system/com.komira.js-agent
sudo rm /Library/LaunchDaemons/com.komira.js-agent.plist
```

Move the `--server` and `--pool` of that plist into `/etc/js-agent/flags`, keep the token file
where it is, and order the install of `js-agent`. The installer package's helper job and its
`/usr/local/js-agent` files are not touched.
