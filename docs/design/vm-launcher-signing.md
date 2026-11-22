# Signing and installing the VM launcher on a Mac

The program that starts a virtual machine on macOS, the launcher, needs Apple's virtualization entitlement, and an
entitlement is part of a code signature. This note says how our launcher, `js-vmm`, is built, signed and installed by
hostkeeper, what a third-party launcher (vfkit, for the first Linux guest experiments) needs, and what was measured to
decide it. It covers the macOS VM leases `js-vmm` serves today and the long-lived Linux guest it is meant to start
later; the guest itself is not designed here.

## Decision

- `js-vmm` ships in the js-agent macOS package, at `/Library/Application Support/js-agent/bin/js-vmm`, mode 0755, the
  path js-agent's `--vm-driver` names.
- The action that builds the package's programs signs it **ad hoc, with the hardened runtime and the entitlement
  `com.apple.security.virtualization`** (`cargo xtask pkg entitle-macos`), right after building it. That needs no key,
  so the job still names no secret, and the `js-vmm` of an unsigned package (the first-install test's) starts virtual
  machines.
- The release's `sign-code` job then signs it like the other programs, with the team's **Developer ID Application**
  identity, the identifier `com.komira.js-agent.vmm`, the hardened runtime and a secure timestamp, and with
  `--preserve-metadata=entitlements`, which keeps the one entitlement `build` gave it (and gives the other programs,
  which have none, none). `sign-code` runs nothing built from the repository, so it reads no entitlements file of
  ours: the entitlement travels in the signature `build` made, and the checks below hold it to what it must be.
- The package is signed by the `sign` job with the **Developer ID Installer** identity, as before. That signature is
  what hostkeeper checks before it installs anything; nothing on the Macs changes.
- `cargo xtask release check-code-signature` holds the released `js-vmm` to the table of
  `crates/xtask/src/release/mac_programs.rs` like the other programs, and `cargo xtask pkg check-macos` reads every
  program's signature back from the package and refuses a program that `codesign --verify --strict` refuses, an
  entitlement set other than the program's own (`js-vmm`: the virtualization entitlement and nothing else; every other
  program: none), or an entitled program without the hardened runtime.
- Experiments sign ad hoc: `cargo xtask pkg entitle-macos --bin-dir <dir>`, or `codesign --force --sign - --options
  runtime --entitlements <plist>` with the one key. vfkit is used as its maintainers release it, or re-signed that way;
  hostkeeper never installs it.

## Why the entitlement is kept rather than given in `sign-code`

`sign-code` checks nothing out and runs nothing built from the repository, so an entitlements plist would have to be
written inline in the workflow, a second copy of what `entitlements_of` says. `--preserve-metadata=entitlements` keeps
the entitlements of the signature being replaced instead. Measured on a Mac with an ad hoc identity standing in for the
Developer ID: the release build of `js-vmm`, signed by `pkg entitle-macos` and re-signed under another identifier with
`--preserve-metadata=entitlements`, kept exactly the virtualization entitlement and verified under
`codesign --verify --strict`; `js-mac-provision`, as the linker signed it, re-signed the same way, carried no
entitlement. A `build` that gave a program a wrong entitlement is refused by `check-macos` in the `package` job, before
anything is signed for a release.

## What an ad hoc signature can and cannot do

hostkeeper trusts a `macos-pkg` package when `pkgutil --check-signature` passes and the leaf certificate's SHA-256 is in
the Mac's signers file (`/etc/hostkeeper/macos-pkg.toml`); a second package, or a changed package, from the same
identity needs no change there. For the programs inside, it asks only that `codesign --verify --strict` passes and that
no file carries `com.apple.quarantine`. An ad hoc signature passes both, so an unsigned package's `js-vmm` works on a
Mac; the release signs it with the Developer ID because the release signs every program of the package so.

The difference shows on a quarantined copy:

- Gatekeeper assesses a file that carries `com.apple.quarantine`. Measured: a quarantined ad hoc copy of the launcher
  does not start (the `exec` does not return), a quarantined copy signed with a Developer ID and notarized starts, and
  copies without the attribute start either way. hostkeeper refuses a quarantined file, and the system installer lays
  the payload down without the attribute, even from a quarantined `.pkg` (measured), so a hostkeeper Mac never meets
  the first case. A launcher copied onto a Mac by a browser or a chat client would; experiments fetch with `curl` or
  `scp`, which set no attribute.
- The launcher needs no privacy (TCC) grant.

## What the entitlement needs (measured)

On a Mac Studio with macOS 27.0.1, SIP on and Gatekeeper assessments on, as the login user over ssh (not root, not from
launchd), with vfkit 0.6.4 starting a Linux-type VM (`VZEFIBootLoader`, one virtio block disk, no network device):

| signature | result |
|---|---|
| as released (Developer ID Application of its maintainers, hardened runtime, timestamped, the entitlement) | VM running |
| `vfkit-unsigned` re-signed ad hoc, hardened runtime, the entitlement | VM running |
| the same, without the entitlement | refused at configuration: `VZErrorDomain Code=2 ... The process doesn't have the "com.apple.security.virtualization" entitlement` |
| the ad hoc one with `com.apple.quarantine` set | does not start |

So the entitlement needs no provisioning profile and no Developer ID, and the hardened runtime does not get in the way.
A bridged network attachment needs a different entitlement, `com.apple.vm.networking`, which Apple grants only on
request; NAT and a file-handle attachment do not, and nothing here uses a bridge.

Then the workflow's own steps on the same Mac, from this repository's code: `pkg entitle-macos` signed the release
build of `js-vmm` (flags `adhoc,runtime`, the one entitlement) and left the cdhash of `js-agent` as it was; `stage-macos`,
`pkgbuild`, `productbuild` and `pkg check-macos` built and accepted the package; `installer` put the same component into
the user's home from a quarantined product archive. The installed `js-vmm` had the staged bytes, no quarantine, passed
`codesign --verify --strict`, carried the entitlement, and ran (`--version`, and `probe p6`, which stops at the missing
base image before it starts a VM). vfkit's unsigned build, named `js-vmm` and signed by `pkg entitle-macos`, ran a VM:
the verb's signature is one Virtualization.framework admits.

## Install and update

hostkeeper installs the js-agent package as it does today: it checks the package's signature and leaf, expands it
with `pkgutil --expand-full`, checks the payload (owners, modes, no quarantine, `codesign --verify --strict` of each
Mach-O), runs the update hook's `prepare` (cordon, drain), installs with `installer -allowUntrusted -pkg <pkg> -target /`,
checks each installed file against the expanded tree, and runs `cutover`, `verify` and `cleanup`.

- The launcher of a VM lease is a child of js-agent, one per VM. `prepare` drains the agent, so when `installer`
  replaces the files no `js-vmm` of a lease runs.
- `installer` replaces a file by a new one (a new inode), not in place. Measured with a package installed into the
  user's home, the same tool and the same replace: a signed program started from the first version kept running across
  the second version's install, and the file on disk was the new one. A long-lived launcher, such as the supervisor of a
  Linux guest, therefore survives a package update and runs the new binary at its next start; it never runs a file
  half written, and the system never kills it for a signature that no longer matches its pages.
- A rollback reinstalls the previous version's package, whose `js-vmm` carries the same shape of signature.

Nothing new is needed on the Macs: no signers-file change, no new hostkeeper service, and no operator step for the
signature. The launcher is on a Mac once the js-agent package is (`mac-hostkeeper-package.md`); js-agent uses it when
the host's flags give `--vm-driver "/Library/Application Support/js-agent/bin/js-vmm"` and `--vm-store`.

## vfkit

The vfkit release (Apache-2.0) is a universal binary signed with its maintainers' Developer ID Application identity,
with the hardened runtime, a timestamp and the virtualization entitlement, and it starts even when quarantined. A
`vfkit-unsigned` asset is published beside it. For an experiment either is fetched by version and SHA-256 and used as
is, or the unsigned one is re-signed ad hoc as above, which replaces the maintainers' signature; both were measured to
run a VM. It is not shipped in any package of ours. If it ever were, it would be built from a pinned source and signed
like `js-vmm`, with its own line in `entitlements_of` (`crates/xtask/src/macpkg.rs`) and in the signing table
(`crates/xtask/src/release/mac_programs.rs`).

## Not measured

- A launcher started by launchd as a System-domain daemon, or as a dedicated user's agent with nobody logged in, and a
  run of 24 hours: the Linux guest experiment covers these.
- An install by hostkeeper on a farm Mac (root, `-target /`): it needs the first js-agent install there, which waits on
  the host setup of `mac-hostkeeper-package.md`. The user-domain install used here is the same `installer` with another
  target, and the release's first-install test installs the unsigned package, `js-vmm` included, through hostkeeper's
  installer as root on a hosted runner; neither starts a virtual machine from the installed `js-vmm`.
- Our own Developer ID signature on `js-vmm` starting a virtual machine: the vfkit maintainers' Developer ID signature
  with the entitlement did, and the release's `package` job reads ours back (`check-code-signature`, `check-macos`),
  but no job starts a VM from it.
- Macs on macOS 26: the measurements are from macOS 27.0.1.
