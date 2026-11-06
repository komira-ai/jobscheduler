# Machine images for elastic providers

An elastic provider launches each machine from an image the operator names in the provider's
`images` table, one image per architecture:

```toml
[[machine_provider]]
name   = "aws"
kind   = "ec2"
images = { arm64 = "<image id>", x86_64 = "<image id>" }
```

This runbook says what such an image holds, how its build description is made, how to build and
boot-check it, and the held step that publishes it to a cloud account. Nothing in this repository
publishes an image: the build and the check run on a GitHub-hosted runner, with no cloud account,
and the publish is done by hand, by the owner of the account.

## What the image holds

The image is the Ubuntu 24.04 server cloud image with:

| what | from | where |
|---|---|---|
| js-agent | the release archive of a jobscheduler release | `/usr/bin/js-agent`, run through `/opt/js-agent/link/current` (`docs/runbooks/crun-host-setup.md`) |
| crun | Ubuntu 24.04's own `crun` package, with `uidmap` and `xfsprogs` from the archive | `/usr/bin/crun` |
| hostkeeper | the deb of a hostkeeper release, which enables `hostkeeper.service` | `/usr/bin/hostkeeper` |
| the crun host setup | `crun_host_setup.sh --apply` of this repository, run once at build time | the job user, its subids, the directories, the AppArmor profile, `js-agent-crun.service` |
| the scratch file system | an XFS file system in a sparse 16 GiB file, mounted with project quota | `/var/lib/js-agent-scratch.xfs` on `/var/lib/js-agent/scratch` |
| the user-data reader | `packaging/machine-image/js-agent-user-data.service` | `/etc/systemd/system/js-agent-user-data.service` |
| its recipe | the recipe it was built from | `/usr/share/js-machine-image/recipe.json` |

The archives are pinned by HTTPS URL and SHA-256 in `packaging/machine-image/pins.toml`, and the
build refuses a download whose SHA-256 differs. The units enabled at boot are
`js-agent-crun.service`, `hostkeeper.service`, `js-agent-user-data.service` and the scratch mount
`var-lib-js\x2dagent-scratch.mount`.

The image holds no credential, token, server address or worker property. The crun unit is written
at build time with the server `https://set-by-user-data.invalid` and the pool `set-by-user-data`,
which never resolve and which the user data replaces. `cargo xtask image-recipe` refuses a pins
key, or a key in a baked configuration file (the units, the drop-in, the mount unit and the
tmpfiles file; a `NAME=` or a `--name` on any line), that looks like secret material (a token, a
password, a key), naming the file, the line or field, and the key. The baked
`crun_host_setup.sh` is not scanned: it is a program, whose shell variables are not settings, and
the recipe pins it by its SHA-256.

### The crun host setup

The image's crun host setup is the one of `docs/runbooks/crun-host-setup.md`, made by the same
script: the build runs `crun_host_setup.sh --apply` once, with the unit and the tmpfiles file of
this repository by path and SHA-256. Its steps are the list in
`packaging/js-agent/crun_host_steps.toml`, which the runbook's table and the recipe both follow; a
test fails when the table, the script's order or the recipe differs from it. The image does not
repeat any of the steps itself. Two things differ from a host set up by hand, both before the
script runs: crun comes from the pinned deb rather than the newest one of the archive (the
script's `packages` step then finds it installed), and the scratch mount unit is the image's own,
already mounted with `prjquota` (the `scratch quota` step then leaves it).

### The user data

The agent's configuration comes from the provider's user data at boot. The user data is a
`#cloud-config` whose `write_files` writes these paths; cloud-init writes them in
`cloud-init.service`, before the reader runs:

| path | mode | required | what |
|---|---|---|---|
| `/etc/systemd/system/js-agent-crun.service.d/50-user-data.conf` | 0644 | yes | `[Service]`, then `ExecStart=` (cleared) and `ExecStart=` set again: the baked unit's line with this host's `--server` and `--pool` |
| `/etc/js-agent/token` | 0600 | yes | the join token the agent reads with `--token-file` |
| `/etc/hostkeeper/hostkeeper.toml` | 0600 | no | hostkeeper's config, filled from `/usr/share/hostkeeper/hostkeeper.toml.template`; without it hostkeeper does not start |

The baked unit's line is:

```text
/opt/js-agent/link/current/js-agent --server <server> --token-file /etc/js-agent/token --pool <pool> --drain-wait 3600 --stop-grace 120 --runtime crun --crun-bin /usr/bin/crun --job-user jsjob --state-root /var/lib/js-agent/state --scratch-root /var/lib/js-agent/scratch/runs
```

`js-agent-user-data.service` runs after `cloud-init.service` and before the agent and hostkeeper.
It asserts that the two required paths exist and runs `systemctl daemon-reload`, so the agent's
unit starts with the drop-in. The agent's unit requires the reader (the drop-in `10-image.conf`),
so a machine booted with no user data, or with user data that lacks a required path, does not
start the agent; `systemctl status js-agent-user-data` names the missing path.

The token is in the user data, so it is also in cloud-init's copy of it under `/var/lib/cloud` and
can be read from the provider's metadata service by any process of the machine. The image removes
neither: whatever renders the user data is responsible for that, as the lease server's boot payload
is today. The lease server does not render this user data yet; until it does, an image of this
runbook is launched with user data written by hand.

## The recipe

`cargo xtask image-recipe --out <dir>` writes, for `linux-x86_64` and `linux-arm64`, the
directory `<dir>/<platform>/` with:

| file | what |
|---|---|
| `recipe.json` | the build description: the base image and the archives by URL and SHA-256, every file the build installs with its mode and text, the crun host setup (its steps and the script's command), the units enabled, the user-data paths, and the build's commands in order |
| `user-data`, `meta-data` | the NoCloud seed that builds the image from the base image: the recipe's files as cloud-init `write_files`, its commands as `runcmd` (the first that fails ends the build), then a power-off |
| `base.sha256` | the base image's SHA-256, for `sha256sum --check` of `base.img` |

It makes no network call: the same pins and files give byte-identical output, and the line it
prints names each recipe's SHA-256.

The recipe's platform maps to the provider's `images` key: the image built from
`linux-x86_64/recipe.json` goes under `images.x86_64` (its `images_key`), and the one from
`linux-arm64/recipe.json` under `images.arm64`.

To move a pin (a new jobscheduler or hostkeeper release, a new base image), edit
`packaging/machine-image/pins.toml` with the URL and the SHA-256 from the release's own
`SHA256SUMS` (for crun, from the archive's `Packages` index), never from a file you downloaded.

## Building and checking an image

The workflow `machine-image` (`.github/workflows/machine-image.yml`) runs only by hand: Actions,
`machine-image`, Run workflow, with the architecture (`x86_64` or `arm64`), or

```text
gh workflow run machine-image.yml -f arch=x86_64
```

It runs on a GitHub-hosted runner of that architecture, with read access to the repository and
nothing else, and:

1. runs `cargo xtask image-recipe` and downloads the base image, checked against `base.sha256`;
2. boots the base image in QEMU with the build seed: cloud-init downloads and checks the archives,
   installs them, runs the crun host setup and the rest of the build, and powers off. The build
   passes when the console shows `js-image-build-done`;
3. boots the built image with the fake user data of `packaging/machine-image/check/user-data`
   (a loopback server nothing answers, the pool `image-check`, a made-up token). The check
   passes when the console shows `js-image-check-passed`, which the fake user data prints only
   after: the reader, the agent's unit and hostkeeper's unit are enabled; the agent's unit
   started; the agent printed its first retry line (`js-agent: <why>; next in <n>ms`), which it
   does only after its start checks of the host passed; both the reader and the agent are active;
   and the agent's command line carries the user data's pool;
4. uploads the built image (qcow2), its recipe and both console logs as the run's artifact,
   kept 7 days.

It publishes nothing and touches no cloud account.

## Publishing an image (held)

Publishing creates an image in a cloud account and costs money for its storage. It is not
automated. The owner of the account does it by hand, from a checked image, one architecture at a
time. Run every command first as written (each that creates something has its dry run), read
what it would do, and only then run it again without the dry-run flag.

1. Download the artifact of a `machine-image` run that passed, for the architecture:
   `gh run download <run id> --name machine-image-<arch>`. Check that `recipe.json` in it equals
   the recipe `cargo xtask image-recipe` writes at the run's commit, and note the recipe's
   SHA-256 the run printed.
2. Make the disk the cloud imports from the artifact's image, `jobscheduler-linux-<arch>.qcow2`
   (`<arch>` the architecture of the run): `qemu-img convert -O raw jobscheduler-linux-<arch>.qcow2 disk.raw`.
3. EC2:
   1. `aws s3 cp --dryrun disk.raw s3://<bucket>/<name>.raw`, then without `--dryrun`.
   2. `aws ec2 import-snapshot --dry-run --disk-container Format=RAW,UserBucket="{S3Bucket=<bucket>,S3Key=<name>.raw}"`,
      then without `--dry-run`; wait for `aws ec2 describe-import-snapshot-tasks` to report
      `completed` and note the snapshot id.
   3. `aws ec2 register-image --dry-run --name <name> --architecture <x86_64|arm64> --virtualization-type hvm --ena-support --boot-mode uefi-preferred --root-device-name /dev/sda1 --block-device-mappings "DeviceName=/dev/sda1,Ebs={SnapshotId=<snapshot id>}"`,
      then without `--dry-run`; note the image id (`ami-…`).
4. Compute Engine (no dry run exists for these calls: read the command twice before running it):
   1. `tar --format=oldgnu -Sczf <name>.tar.gz disk.raw`, then `gcloud storage cp <name>.tar.gz gs://<bucket>/`.
   2. `gcloud compute images create <name> --source-uri gs://<bucket>/<name>.tar.gz --architecture <X86_64|ARM64> --guest-os-features UEFI_COMPATIBLE,GVNIC`;
      note the image name.
5. Remove the uploaded disk from the bucket.

`<name>` is `jobscheduler-<js-agent version>-<platform>-<first 12 digits of the recipe SHA-256>`,
so an image names what it was built from.

Afterwards the operator records, in the launch record: the image id and the region (or project),
the platform, the recipe's SHA-256, the URL of the `machine-image` run, the commit it ran at, the
versions of js-agent and hostkeeper in the pins, the date, and who published it. Then they put the
image id in the provider's `images` table under the recipe's `images_key`.

## What is not checked

The workflow's boot check runs in QEMU on a hosted runner, not on a cloud machine: it does not
prove the image boots on EC2 or Compute Engine, that cloud-init there reads the provider's user
data, or that the agent reaches a real server. The first launch from a published image
(`docs/runbooks/first-live-launch.md`) is where those are seen. The publish commands above are not
run by any test.
