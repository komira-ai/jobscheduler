# Running a CI step on the farm

`actions/run` is a GitHub Action that runs one CI step on a machine of the job service's farm
(macOS on Apple silicon, or Linux in a container) from a GitHub-hosted Linux runner. It is for
the steps that are not Buck2 targets, which the build farm runs through its own remote
execution: `cargo test` on macOS, installer and package checks, scripts. The step's log
appears in the workflow's log as it is written, the action's step fails with the step's own
exit code, and the files the step names come back as a workflow artifact.

The action is a thin wrapper around one command of `js`, the job service's client:
`js ci run` (crate docs of `js-cli`, "CI steps"; `js ci run --help`).

## Using it

```yaml
jobs:
  macos-tests:
    runs-on: ubuntu-latest
    permissions:
      contents: read
      id-token: write   # the tailnet join's federated identity
    steps:
      - uses: komira-ai/jobscheduler/actions/run@<commit> # v<release>
        with:
          os: macos
          image: <the macOS bundle, NAME@sha256:HEX>
          run: cargo test --locked -p my-crate
          outputs: |
            target/report.json
          js-version: <release>
          js-sha256: <SHA-256 of js-<release>-x86_64-unknown-linux-musl.tar.gz>
          server: ${{ vars.JS_CI_SERVER }}
          ca-cert: ${{ vars.JS_CI_CA_CERT }}
          token: ${{ secrets.JS_CI_TOKEN }}
          tailnet-client-id: ${{ vars.JS_CI_TAILNET_CLIENT_ID }}
          tailnet-audience: ${{ vars.JS_CI_TAILNET_AUDIENCE }}
          tailnet-ping: ${{ vars.JS_CI_TAILNET_PING }}
```

Pin the action to a commit, and `js-version` and `js-sha256` to one release: the downloaded
`js` holds the client token, so a new release cannot change what runs with it until the pin
changes. Every input is described in `actions/run/action.yml`. The step's outputs are
`job-id`, `outcome` (`succeeded`, `failed`, `cancelled`, `timed-out`, `lost`,
`failed-to-start`, or the kind of `js`'s own failure, such as `rejected`) and `exit-code`
(0 for a job that succeeded, the step's code for one that failed with one, and empty
otherwise).

## What happens

1. **Reach.** The runner joins the tailnet as an ephemeral node tagged `tailnet-tag`
   (`tag:ci` unless named), with `tailscale/github-action` pinned to a commit and its tarball
   to a version and SHA-256. It authenticates with a GitHub OIDC token that the tailnet's
   federated identity exchanges, so no tailnet secret is stored; the join pings
   `tailnet-ping` until it answers.
2. **The client.** A directory of the step's own is made (`mktemp -d`), so two farm steps of
   one job share no file. `js` is downloaded there from the release `js-version` and checked
   against `js-sha256` before anything of it runs. The client token, the token the job
   fetches the commit with, and the service's CA are written there to files only the runner's
   user can read; none is ever an argument.
3. **The job.** `js ci run` submits one job: `/bin/sh` running a script that fetches the one
   commit (`git fetch --depth 1` of `commit`, by default the workflow's), checks it out, runs
   `run` with `bash --noprofile --norc -e -o pipefail -c` in the checkout (as GitHub runs a
   `run:` step), and then writes the `outputs` that exist to its stdout as a tar archive
   framed by a marker that holds the job's id (so the same command run again with the same
   id submits the same job). The job asks for `os=macos`, `arch=arm64`,
   `exec=sandbox-exec`, or `os=linux`, `exec=container`; its network level is `network`
   (`open` by default: the fetch needs GitHub). It names no CPU or memory.
4. **The log.** `js` writes the job's stdout and stderr to the step's log as the job writes
   them (`js job run`'s loop: a lost submit is sent again with the same job id, so it makes
   one job; the first SIGINT or SIGTERM, which GitHub sends when the run is cancelled,
   cancels the job), takes the outputs frame out of stdout, and unpacks the archive: files
   and directories only, so an archive holding a link (which the artifact upload would
   follow to the runner's own files) is refused.
5. **The end.** The action's step exits with the step's own exit code when the job failed
   with one, 0 when it succeeded, and `js`'s status otherwise (`js ci run --help`): 11
   cancelled, 12 timed out at `wall-limit`, 13 lost, 14 failed to start, 15 undetermined, 16
   succeeded but its outputs could not be read back. A step's own code can equal one of these;
   the `outcome` output tells them apart.
6. **The artifact.** Unless the run was cancelled, the outputs are uploaded as
   `artifact-name`, whether or not the step succeeded (a failed step's report is often the
   one that matters).

## Decisions and why

**How GitHub reaches the job service.** The job service listens on the tailnet only and has no
public listener. The hosted runner joins the tailnet for the length of the job, as hostkeeper's
`set-desired` workflow does to reach its coordinator. The other two ways were a relay host
inside the tailnet that GitHub calls (a public listener in all but name, and one more service to
run), and a self-hosted runner inside the tailnet (workflows here run on hosted runners only, and
a self-hosted runner holds every job's secrets on a machine of the farm). The join holds no
secret: the federated identity issues a node only to this repository's workflows, tagged
`tailnet-tag`, and the tailnet's ACL lets that tag reach the job service's port and nothing
else.

**The client token.** CI is one client of the job service: an entry `github-ci` of the server's
`--client-token` (role `client`, the default), whose token is the organisation secret
`JS_CI_TOKEN`. A client may submit jobs and read, follow and cancel its own jobs; it cannot
list hosts, cordon them, read pools or usage, or see another client's jobs. What a holder of
the token gains is that: it can run jobs on the farm as CI does (in the Mac's sandbox or a
Linux container, with the network level it asks for) and read CI's jobs and logs. It cannot
reach the farm without the tailnet as well. The server keeps only the token's digest and
re-reads its file on every tick, so a rotation is a new file on the server and a new secret,
with no restart.

**How the source reaches the job.** The job fetches its commit from GitHub itself. The runner
cannot write to the farm's object store (that would be a second credential and a second
route in), a job file holds at most 16 KiB, and a Mac cannot mount a tree yet
(komira-ai/jobscheduler#888, item 3). A shallow fetch of one commit takes seconds. The token
is the workflow's own `GITHUB_TOKEN` by default, which expires with the workflow job; it
reaches the job as the job file `source-token` (no read of the job returns a job file's
contents) and git reads it through a credential helper configured for the repository's URL
alone, so it is in no argument, no environment variable and no file of the checkout, and no
other host is given it. The helper is removed once the commit is checked out. The file stays
readable to the job, as the token of a step on a GitHub runner is readable to that step: the
step runs the repository's own code at that commit. A public repository could leave it empty,
but `js` refuses an empty token file, so the default stays.

**How the outputs come back.** In the job's stdout. The job service has no call that returns a
job's files: a Linux agent refuses declared outputs, and a Mac agent's stay on the Mac. The log
store already keeps every byte of stdout exactly as the job wrote it, and `js` already reads it
to its end, so the job writes a tar archive after a line holding a marker (the job's id) and
its length, and `js` takes it out of the stream. On macOS the archive is made with
`COPYFILE_DISABLE=1`, so extended attributes add no `._` files. An archive that cannot be made
is said on the job's stderr and leaves the step's status as it is. Its limit is the agent's log cap
(`--log-file-cap`, 64 MiB per stream by default; bytes past it are a gap, and the frame is then
reported broken) and `js`'s own 1 GiB. A step whose outputs are larger needs a call of the job
service that returns a job's files, which this does not add.

**Sizing.** The job's `size-class` label is the repository, the workflow, the job and the
action's `name`, written as a class (`SizeClass::of_target`), so every run of one step reads and
teaches one history. Its `size-fallback` is the step's kind, `ci-macos` or `ci-linux`, whose
history sizes a step with fewer than five runs of its own; `size-fallback` names another.

**The script, not a program on the host.** The job runs `/bin/sh` and the tools of the host
(macOS) or the image (Linux): `mktemp`, `git`, `bash`, `tar`, `wc`. A program of ours on the job's
side would have to be installed on every host or carried in every image. The script takes every
value as a positional parameter, never in its text.

## Before a macOS step can run

- The job service starts native jobs from `SubmitJob` (komira-ai/jobscheduler#888, items 1 and
  2): today a Mac agent refuses a job start named by an image with no native run. `js ci run`
  sends `command` `/bin/sh` and the script in `args`, so the native start must run a
  `command` that is an absolute path on the host with the job's `args`, and take the image as
  the bundle the job is named by.
- The Mac pool's agents allow the network level CI asks for (`--native-network open`, or
  `on` with GitHub on the allow-list and `network: allow-list`).
- The Macs have `git` (the Command Line Tools) and, for Rust steps, a toolchain the pool user
  can run.
- The server has the client entry `github-ci`; the organisation has the secret `JS_CI_TOKEN`
  and the variables the example names; the tailnet has the federated identity for the
  repositories that call the action, issuing `tag:ci`, and an ACL rule from `tag:ci` to the
  job service's port.

## When it fails

| what fails | what it means | what to do |
|---|---|---|
| Join the tailnet | The federated identity refused the runner, or `tailnet-ping` never answered. | Check the calling job grants `id-token: write`, the identity names this repository, and the ACL. |
| Check js against its pinned SHA-256 | The downloaded archive is not the one pinned. | Check `js-version` and `js-sha256` belong together; do not change the hash to make it pass. |
| Run the step, status 1 | The service could not be reached, or refused the token. | Check `server`, `ca-cert` and the token. |
| Run the step, status 3 | The service refused the job, for example `no-capable-host`: no registered host is a Mac (or a Linux host) and no provider may launch one. | Check the pool. |
| Run the step, status 4 | The job itself was refused, for example a wall limit above the service's ceiling. | Fix the input the message names. |
| Run the step, status 9 | The submit's answer was lost; the job may exist. | Re-run the workflow job (a new job id) or run `js ci run` again with the printed `--id`. |
| Run the step, status 14 | The job failed to start: no capacity within the pool's queue wait, or the agent refused it. | `js job get --job-id <job-id>` says why. |
| Run the step, status 16 | The step succeeded, but its outputs did not come back: the script did not reach them, or the frame was cut (the log cap). | Read the job's stderr; make the outputs smaller. |
