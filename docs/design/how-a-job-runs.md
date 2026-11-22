# How a job is run: from its operating system

Companion to `jobscheduler-design.md`, section 3.8 (the capability vocabulary), and
`pools-providers-network.md` (a pool's `exec`).

A client names the operating system and the architecture of a job, not how the host runs it.
The job service decides that when it builds the job's start (`start_of`,
`crates/js-lease-server/src/job_start.rs`):

| `exec` the job names | `os` | image | start |
|---|---|---|---|
| none | `macos` | yes | native: the image runs as a bundle |
| none | `linux`, none, or another | yes | container |
| `sandbox-exec` | any | yes | native |
| `container` | any | yes | container |
| any | any | no (layers alone) | container |

- **A mode the job names wins over its `os`.** `exec` is what an older client sends, and it is
  held to as before. The job is placed only on a host that reports the mode, and the mode
  decides how its image runs. A job that the submit gives `container` (`validate.rs`, for
  layers or a remapped host port) is a container job.
- **A job that names no mode is placed by its `os` and `arch` alone.** A `macos` job that names
  no mode matches a Mac whose pool books `exec = "sandbox-exec"`.
- **Every start carries the job's `os` and `arch`, native or container, when the job names
  them.** A start names them only as `linux` or `macos`, and `x86_64` or `arm64`. An agent that
  runs more than one platform can choose its runtime from them. An agent that runs one platform
  decodes them and does not read them: on Linux the start's `os` and `arch` are read nowhere,
  and on a Mac only the bundle admission compares them with the bundle's platform. A value an
  agent does not know would cost it the start, which is why the service leaves out an `os` or
  `arch` it does not know rather than copying the constraint's text.
- **A Linux job is never placed on a Mac.** A Macs' pool names `os = "macos"`, and `os` is
  matched exactly. A Mac that runs Linux jobs is a later design.
