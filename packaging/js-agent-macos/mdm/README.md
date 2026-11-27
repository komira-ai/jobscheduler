# The privacy profile

`js-agent-full-disk-access.mobileconfig` grants Full Disk Access (the privacy service
`SystemPolicyAllFiles`) to `js-agent` itself, and to nothing else: not `js-mac-provision`, which
holds no grant. A native job and its supervisor are to be started without
the disclaim, so that they answer for `js-agent` and hold what it holds; every other program
`js-agent` starts is started disclaimed (see `../README.md`, "Privacy grants"). The profile's
`Comment` says why the agent holds the grant: it deletes the accounts native jobs run as, and a
job it starts through a supervisor, as its own slot user under a sandbox profile, is to read the
build trees tierstore mounts. The `Comment` is text only: a Mac that holds the profile from
before keeps the grant unchanged, shows the earlier text until the profile is uploaded again,
and needs no `PayloadVersion` change (it is the profile format's version, 1).
It is one `com.apple.TCC.configuration-profile-policy` payload with one entry:

| key | value |
|---|---|
| `Identifier` | `/Library/Application Support/js-agent/bin/js-agent`, where the package lays the program and updates replace it in place |
| `IdentifierType` | `path` |
| `CodeRequirement` | the agent's designated requirement: the identifier `com.komira.js-agent`, Apple's Developer ID chain and the team `U7RTX4XD3X`, and no code hash, so every release meets it |
| `Authorization` | `Allow` |
| `StaticCode` | `false` |

The file is rendered from the signing table (`crates/xtask/src/release/mac_programs.rs`) and
`cargo test -p xtask` fails when it differs from the table by one byte, so the entry cannot name
another team, identifier, path or service, carry a hash, deny, or hold a second entry without the
table changing in the same pull request. Change the table and the file together.

## Owner's steps

Full Disk Access can be granted by a profile from device management only: a profile installed by
hand cannot grant it.

1. Create the secrets `MACOS_APPLICATION_P12` and `MACOS_APPLICATION_P12_PASSWORD` in the
   environment `pkg-signing` (the Developer ID Application certificate and key of the team, see
   `../README.md`, "Signing"), so the release can sign the programs.
2. Upload `js-agent-full-disk-access.mobileconfig` to the device management service as a custom
   profile, scope System, and assign it to the group of Macs that run js-agent. It needs no
   change per release.
3. After a release that signs the programs has been installed, the agent reports the capability
   `full-disk-access` of its host: `granted` when the agent holds the grant. A host that reports
   anything else refuses native jobs, naming the profile.

To check a Mac by hand, read the agent's log in `/Library/Application Support/js-agent/log/`: the
agent writes `full disk access: <state>` once at each start.
