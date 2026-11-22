# Proving a Mac host with the smoke bundle

The smoke bundle is a darwin bundle of one program that prints a single line: the host name, the user and uid it runs as, the macOS version and its arguments. The release publishes it, public, to GitHub's container registry (`packaging/js-smoke-bundle/README.md` says how), so a Mac host is proven with one job and no file placed by hand: the agent pulls the bundle by digest, checks every byte, places it in its store and runs the program as a pool user under Seatbelt.

The bundle, by digest (the tag is `1`):

```
ghcr.io/komira-ai/js/smoke:1@sha256:9e02a9ff48c04e969131abf064e01af65a167038ecc85764a8236a75cb1dc9a7
```

## What the agent needs

The host runs jobs from bundles already: `js-agent` has a bundle store (`--bundle-store`, with `--scratch-root` and `--bundle-budget-gib`), trusts a list of manifest digests (`--bundle-trust list`, the default, with `--bundle-allow-file`) and fetches a bundle it does not hold (`--bundle-pull missing`). Two lines are added:

1. To `/etc/js-agent/flags`, the registry path the bundle may be pulled from:

   ```
   --image-allow=ghcr.io/komira-ai/js/smoke
   ```

   The entry admits that repository and nothing else on the registry. The package is public, so no `--registry-credentials` entry is needed. Restart the agent service after the change: the flags are read at start.

2. To the file `--bundle-allow-file` names, the bundle's manifest digest, on a line of its own:

   ```
   sha256:9e02a9ff48c04e969131abf064e01af65a167038ecc85764a8236a75cb1dc9a7
   ```

   The agent reads the file again at each job start, so this line needs no restart.

## The job

```
js job run --server https://js.example.com:<job port> --token-file <client token> --ca-cert <ca.pem> \
  --spec.image ghcr.io/komira-ai/js/smoke:1@sha256:9e02a9ff48c04e969131abf064e01af65a167038ecc85764a8236a75cb1dc9a7 \
  --spec.constraints.os macos --spec.constraints.arch arm64 --spec.constraints.exec sandbox-exec \
  --spec.args proof --spec.wall-limit-seconds 5m
```

No `--spec.command`: the program is the bundle's entrypoint named as the bundle, `smoke` (the last part of the image's name), which is `bin/smoke` in its tree. On a host that does not hold the bundle yet, the job's start fetches it in the background and is held (`bundle-pending`) until the bundle is in the store; the job then starts as any other.

What `js` prints: the job's stdout, one line, on stdout, for instance

```
js-smoke host=mac-1.example.com user=kbf-lease-pool-3 uid=603 macos=15.6 args=proof
```

and on stderr the job id it drew (`js: job id <id>`) and the end, `job <id> succeeded`; `js` exits 0. The `host=` field names the host that ran the job; the user is one of the host's pool users and the uid one of the helper's `--uid-range`.

What a failure says, and what to look at:

- `image-not-allowed`: the `--image-allow` line is missing or the agent was not restarted.
- `bundle-not-allowed`: the digest is not in the allow-list file.
- `bundle-not-held`: the agent runs with `--bundle-pull never`; put the digest in its prefetch file, or run it with `--bundle-pull missing`.
- `bundle-fetch-failed`: the host could not reach `ghcr.io`, or what it served did not match the digest; the agent's journal says which.
- The job ran and exited non-zero: the program stops at the first of `hostname`, `id` and `sw_vers` that fails, and its stderr says which.
