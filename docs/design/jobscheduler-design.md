# jobscheduler: the design

jobscheduler is a generic, open-source scheduler that runs OCI images on Mac or Linux hosts. On Linux it launches each job with `crun` directly, with no container daemon in the path, so a job starts fast. On Linux this is built; on macOS the single image job definition is step 17 of section 12.1, and no launch time has been measured yet (step 18). A **pool** is a named set of capacity and the platforms that supply it (section 4.2); it names **container platforms** in priority order. The first kind is `Default`, which is ours: `js-agent` with `crun` on Linux and native runs on macOS, with our own placement. It takes an ordered list of **machine providers**: static hosts we install `js-agent` on, which reach out and register (your own bare-metal Linux and Macs), then cloud VM providers that start a machine that boots `js-agent` (Lambda Labs built; AWS EC2 and GCP Compute Engine planned). Kubernetes, Cloud Run and ECS are planned platform kinds with their own parameters (section 3.7). One capability vocabulary, owned by the scheduler, is how a request such as a GB300 accelerator finds the machine that has it (sections 3.8, 5.6). v1 builds the Default platform only.

This is the design document. It describes the target design, and for every piece it says **Built**, **Partly built** or **Planned** and names the crate that holds it. A new engineer should be able to read it top to bottom and know what the system is, what the one picture shows, how each operation works, and what is left to do. Where today's code differs from the design, the difference is a numbered step in section 12.1, not part of the narrative.

Every statement about behaviour cites a file or crate. Paths are relative to the repository root; crate paths are under `crates/`. A statement marked Built describes what the code does. Anything else is a design and is marked Partly built or Planned. Where an older design document and the code disagree, this document follows the code and says so.

Eleven SVG diagrams in `docs/design/diagrams/` carry the pictures. Contents: 0 status and the other documents; 1 problem; 2 goals and non-goals; 3 the design in one picture (pieces and their status, leases and runs, layers, one placement engine, container platforms and machine providers and their interfaces, client surface, other platforms (later), one capability vocabulary); 4 configurations; 5 how each operation works (including placement with an accelerator request worked end to end, the job and how it runs, and job networking); 6 background work; 7 edge cases handled; 8 races the current code loses; 9 assumptions; 10 state; 11 limits; 12 future work and the gap list; 13 open questions; related work; 14 glossary.

## What changed in this revision

The maintainers decided the following. The text below follows these decisions; today's code is described only where a section says "today" or "as built", and the steps that close the gap are in section 12.1.

- **Clients use a generated surface.** A client calls the service through a CLI or a client library that are both generated from one protobuf service definition. kbf is one client of that service among others and is not part of the picture.
- **kbf is a client only.** kbf no longer embeds the server crates (`js-server-core`, `js-sched`, `js-state` and the others). This reverses an earlier shape in which kbf linked them.
- **Postgres is the store** for leases, runs, hosts and usage history. SQLite stays only for tests and single-node development.
- **Container platforms, with Default first.** The maintainers decided to model where a job runs as a *container platform*, one of `Default`, `Kubernetes`, `Cloud Run` and `ECS` (others may follow), each with its own typed parameters, and a pool names platforms in priority order. This supersedes the earlier split in which machine providers and container platforms were siblings. `Default` is ours and is the only kind v1 builds (sections 3.5, 3.7).
- **Machine providers belong to the Default platform.** Default takes an ordered list of machine providers: `static` (hosts we install `js-agent` on, which reach out and register, Linux and macOS; today's self-registered hosts become this kind; today's agentless operator-listed static pool is removed as a production kind and kept only as a test fake, question 27), and dynamic VM providers that call a cloud's VM API to start a machine that boots `js-agent` (Lambda Labs built; AWS EC2 and GCP Compute Engine planned). Placement walks them in priority order: first existing free capacity, then the first provider that can acquire new capacity. Every machine runs `js-agent`, and the scheduler never branches on provider kind. Multi-cloud means a cloud's VM API only, never its job or container services, and running `js-agent` on Kubernetes nodes as a machine source is not the plan.
- **One capability vocabulary.** The scheduler owns the terms (os, arch, ISA level, cpu, memory, disk, and a structured accelerator with vendor, model, count, memory per device and, where it matters, interconnect). Each provider's catalog returns offers already translated into those terms, so a request for `nvidia-gb300` is matched against EC2 instance types by code in the EC2 provider crate, not by the scheduler (section 3.8). A VM's agent-detected capabilities are checked against the offer when it says hello.
- **Interfaces and mappings are in code; configuration selects and limits.** `ContainerPlatform` and `CapacityProvider` are Rust traits, the capability vocabulary and its accelerator aliases are in code, and each provider crate translates its cloud's catalog into offers in code. Configuration chooses which platforms and providers a pool uses and in what order, and sets credentials, regions, allowed instance types, price and machine caps and purchase options. It never says what an instance type provides and never defines an alias, so a new accelerator needs a release (settled, section 13).
- **One placement engine**, `js-sched`: load placement, tail placement for whole-node requests, a CPU slack setting, a memory percent setting, and estimates that come from usage history.
- **The job is an OCI image on both platforms.** Linux runs it with `crun` directly. macOS runs it natively from the OCI bundle (a `js-bundle` is an OCI image manifest) under the sandbox. Docker and Podman are at most a fallback (section 5.7).
- **The picture shows the target only; leases and runs are explained; 5.7 is rewritten.** Section 3's picture has no registry, no separate scaler and no "today" notes (built and planned status is in the table of 3.1; the drift is in 0.1 and 12.1). Section 3.2 says what a lease and a run are and how they relate. Section 5.7 now walks one run from submit to result and says why each step exists; constants and today-versus-target detail moved into an "As built today" box at its end.
- **Per-job network policy.** A job asks for `network = none | allow-list | open` and declares a list of host ports; each declared port becomes one DNAT host-port forward, and there is no other ingress. The agent's flags are the ceiling a job cannot exceed. The Podman firewall path is retired, so there is one concept (section 5.8).

---

## 0. Status and the other documents

### 0.1 Where the code is today

The design below is the target. The code that exists today is one `js-lease-server` process that keeps leases in a file, a registry of self-registered hosts (`js-registry`), a scaler with one cloud provider (`js-scaler`), hand-written HTTP/JSON clients, and kbf linking the placement libraries. The target has one scheduler (`js-sched`) and neither a separate registry nor a separate scaler; section 3 says where their duties go. Section 12.1 lists each difference and the steps that close it. There are no Kubernetes manifests, Dockerfile or helm chart in the repository. Whatever supervises the server (a service unit, a coordinator-managed service) is outside this repository.

### 0.2 Built, partly built, in review, planned

- **Built** means the code is in the main branch and a test exercises it.
- **Partly built** means some of the piece is Built and the rest is not; the text says which part.
- **In review** means an open pull request. At the time of writing one matters here: a change that lets `POST /v1/leases` carry a `request_id` (section 5.3). It is described as "in review", never as built.
- **Planned** means only a design document or an issue says it. Every planned item is a step in section 12.1.

### 0.3 How the older documents relate to this one

This document supersedes nothing by deleting it. Nothing is removed. The table says which document is now the authority for what.

| Document | Status relative to this one |
|---|---|
| `docs/design.md` | **Superseded as the entry point.** Still the record of the decision list (D-numbers) and the long-form rationale. Parts are stale: its crate map lists `js-config` and `js-provider-aws` as new crates (neither exists in `crates/`) and says the SQLite and Postgres state backends stay in kbf (they exist here, under `js-state`; this document makes Postgres the store); its protocol section describes a protobuf stream where the code is JSON polling; its Kubernetes target has no manifests; its claim that the agent holds no registry credential is false since private-registry pulls were added. Read it for why, not for what. |
| `docs/design/providers.md` | **Authority for the machine-provider hierarchy and admission rules** (D35, D59 to D61), which section 3.5 and 5.6 summarise; it predates the platform layer and the capability vocabulary. Planned, not built. |
| `docs/design/kbf-on-js-agent.md` | **Partly superseded.** It describes kbf's side of the integration. Where it has kbf embedding server crates, this document replaces that with kbf as a client of the service (section 3.6). Nothing in it about upload grants, a task tool or an `--executor` flag is in this repository. |
| `docs/design/pools-providers-network.md` | **Reference; partly built.** The config file, scaler, elastic contract and the host-to-scheduler network layer it proposes now exist. Its first section ("there is no scaler") is stale. It is about how a host reaches the scheduler, not about job networking (section 4.4 against section 5.8). |
| `docs/design/whole-node.md` | **Reference; current in its contracts.** Authority for whole-node rules. This document summarises it (sections 5.4, 6.4). |
| `docs/design/placement.md`, `placement-by-load.md` | **Reference.** `placement.md` is current as a contract. `placement-by-load.md` names flags (`--placement sched`, `--placement lease-table`, `--placement-load`) that the lease server does not have; the design here makes load placement the only mode. |
| `docs/design/hosts.md`, `registration.md` | **Reference.** `registration.md` is stale on a deny-list of hosts (not built) and on agent self-fencing (not built, section 8). |
| `docs/design/agent-egress.md`, `docs/egress.md` | **`agent-egress.md` is the authority for the crun network mechanism** (section 5.8); it is stale in saying the crun runtime cannot be selected (the `js-agent` rustdoc has the same sentence). **`docs/egress.md` describes the Podman firewall path, which is retired** by this design. |
| `docs/design/agent-privilege.md`, `mac-lease-teardown.md`, `invariants.md`, `absence.md`, `lease-limits.md`, `agent-suite.md`, `configuration.md`, `bench-analysis.md` | **Reference; current.** |
| `docs/design/deployment.md`, `rollout.md` | **Design for a deployment that is not in the repository.** `deployment.md` chose a single-writer lease file; this document replaces that with Postgres (section 10). The flags it prescribes (`--term-floor`, `--verify-state`, a readiness endpoint, a generic `--boot-grace`) do not exist. |
| `docs/design/vm-backend.md`, `capabilities.md`, `sizing.md`, `retries-and-runs.md`, `ucdk.md`, `generalising.md`, `extraction-steps*.md`, `merged-code.md`, `risks-f.md`, `decisions.md`, `amendments.md`, `aws-iam.md`, `kbf-today.md` | **Design or history.** Mostly planned. `merged-code.md` carries the planned trait renames of section 3.5. `kbf-today.md` is stale on kbf's scheduler core flag. |
| `docs/release.md`, `docs/layering.md`, `docs/js-mdm.md`, `docs/runbooks/*` | **Current references** for release, layering, the MDM host layer, and operations. |
| Crate-level rustdoc of `js-lease-server`, `js-agent`, `js-sched`, `js-registry`, `js-lease`, `js-scaler`, `js-server-core`, `js-store` | **The most accurate source of detail**, with one known exception: the `js-agent` crate rustdoc (`js-agent/src/lib.rs`) still says the crun runtime cannot be selected, though `--runtime crun` exists. This document links to rustdoc rather than restating flags. |

---

## 1. Problem

A build farm, a benchmark rig and a CI system all need the same thing: run a piece of work on a machine that has enough free capacity, on its own, without leaving anything behind that changes the next piece of work. The hard parts are not the first run. They are:

1. **Who owns a host right now.** A client asks for a host, then stops answering. The host must come back, but not while the client is still using it, and not so late that the farm starves.
2. **Whole-host requests starve behind small ones.** A benchmark that needs a quiet machine never gets one if small work keeps arriving. Draining a host costs the remaining time of the work on it; that cost must be chosen, not accidental.
3. **A host is an unreliable thing.** It can go silent, reboot, be reimaged, or be compromised by the job it ran. The scheduler must say "unknown" when it does not know, and never free a host it cannot see.
4. **Jobs must start fast and be contained.** A job is an image; launching it must not wait on a container daemon. It must be able to write exactly one thing, to exactly one place, once, and reach only the network it declares.
5. **Macs are different.** A Mac host has no container runtime that gives the isolation Linux has. Isolation is a throwaway user, a sandbox profile and a firewall rule, and tearing it down must be proven, not assumed.
6. **Capacity comes from several places.** Machines you own, machines you rent by the hour, and machines a cloud's VM API starts for you are all capacity. A request must be placed on the first source that can serve it, and a machine that costs money needs hard caps and a brake, because the cost of a bug is measured in currency.

jobscheduler is the service that answers these. It is one scheduler service (`js-sched`) that records who holds which host (leases) and what runs on it (runs), decides which host (placement), and gets machines from machine providers; with an agent on each machine that runs the job, contains it and reports; and the release machinery that keeps all of it updated. Section 3.2 defines lease and run.

## 2. Goals and non-goals

### 2.1 Goals

- **G1. A lease is the only way to own a host.** A lease (defined in section 3.2) has a time-to-live that the holder renews and a wall-time ceiling that renewal cannot extend. It is recorded before anything is done for it (section 5.3).
- **G2. Never free what cannot be seen.** The provider answers present, absent or unknown. Only absent frees a booking. Silence reads as unknown.
- **G3. Whole-host requests finish.** A whole-node request reserves the host with the least estimated remaining time, drains it, and commits it when quiet. Smaller work may use the host meanwhile only if it will end inside the estimated tail (section 5.5).
- **G4. Contained jobs.** Digest-pinned images, empty capability sets, a dedicated unprivileged identity, scratch quotas, no network unless the job declares it and the agent allows it (sections 5.7, 5.8).
- **G5. Taint instead of trust.** A host whose post-job checks fail is marked tainted, permanently, and takes no further work.
- **G6. Macs run natively under a proven teardown.** A throwaway user per lease, a generated sandbox profile, a network rule, and a kill-and-delete that judges outcomes rather than exit codes (section 5.9).
- **G7. Bounded spend.** Every elastic pool has instance, hourly, daily and total caps, and a brake that trips to plan-only after repeated launch failures (section 6.5).
- **G8. Updatable without ceremony.** Every push to shipped code is released, attested and byte-identical on rebuild; agents update themselves through a hook that drains first (section 6.6).
- **G9. One job, two platforms.** The job is an OCI image on Linux and on macOS; the host's operating system decides how it runs (section 5.7).
- **G10. One way to ask.** Every client uses a CLI or library generated from one service definition (section 3.6).
- **G11. Machines we control, in any cloud, found by capability.** Multi-cloud means a cloud's VM API starts a machine that boots `js-agent`; jobscheduler places and launches every job itself, and a request names what it needs in one vocabulary (sections 3.5, 3.8, 5.6).
- **G12. Other container platforms later, on the same vocabulary.** Kubernetes, Cloud Run and ECS map the same request onto their own knobs and refuse impossible requests up front (section 3.7).

### 2.2 Non-goals

- Multi-tenant isolation. There is one client token and one trust domain (section 11).
- High availability of the service in the first target. It is one active writer over Postgres; whether to add a standby is open (section 13, question 1).
- Hosting a public listener. The service is meant to sit on a private network.
- Being a workflow engine. It places and contains work; retry policy and result interpretation belong to the client.
- Delegating execution to a cloud's batch or container service in v1. The Default platform's machine providers supply machines and never run a job. The other platform kinds (Kubernetes, Cloud Run, ECS) are planned (section 3.7) and not built in v1.
- A VM backend. The helper crates exist; the runtime does not (section 12).

## 3. The design in one picture

![Components, target design: a generated CLI and client library call one protobuf service API; one scheduler, js-sched, holds the queue and placement and uses estimates from usage history; leases and runs are its state machine; Postgres holds everything including the hosts; on the right a pool names container platforms in priority order: Default, whose ordered machine providers are Static, Lambda Labs, AWS EC2 and GCP Compute Engine, then dashed Kubernetes, Cloud Run and ECS (later). The object store is reached only by the job.](diagrams/components.svg)

Read the picture from the left. Clients call one service API through a CLI or a library, both generated from the service definition. Behind it is **one scheduler, `js-sched`**: it keeps the queue of waiting requests, decides where each request goes, and uses estimates drawn from usage history. What it promises is recorded as leases and runs (section 3.2). Everything it knows, the hosts included (what exists, which are live, what each has), is state in Postgres. In the target there is **no `js-registry`**, no separate scaler and no second placement path. Arrows: the scheduler decides and records its state in Postgres, asks the Default platform for capacity, and agents report to the scheduler.

On the right is a pool's list of container platforms, in priority order. `Default` is the only one v1 builds. It runs every job on a machine that runs `js-agent` (`crun` on Linux, native on macOS) and takes an ordered list of machine providers that supply those machines: `Static` (hosts we install `js-agent` on, which reach out and register) and the cloud VM providers that start a machine when asked (Lambda Labs, AWS EC2, GCP Compute Engine). Acquiring and releasing machines is the Default platform's and its providers' work, not the scheduler's: the scheduler decides that capacity is needed and which provider to ask, and the platform applies the caps and calls the provider. The dashed platforms are later; jobscheduler would hand them the OCI image directly and there is no `js-agent` on them (section 3.7). Every platform and provider describes what it can supply in the vocabulary of section 3.8. The object store is outside: only a job reaches it, through a URL the client minted.

**Where today's registry and scaler duties go.** Host table -> scheduler state in Postgres. Whole-node reservations -> `js-sched`. Run delivery -> the agent protocol. Caps, brake and acquire records -> the Default platform. Today's `js-registry` and `js-scaler` crates are removed by the steps in section 12.1; how the code differs today is in section 0.1, not in the picture.

### 3.1 The pieces

This table is where built and planned status is recorded; the picture above shows the target only. The duties that move out of `js-registry` and `js-scaler` are in the paragraph above and the Registry entry of the glossary. Status says how much of the piece exists today. The crate column says where it lives today, which for `js-registry` and `js-scaler` is structure the design removes.

| Piece | What it is | Status | Crate or path |
|---|---|---|---|
| Service definition | One protobuf service: leases, runs, hosts, pools, usage | Planned | none yet |
| Generated CLI and library | The only clients; generated from the definition | Planned | replace `js-cli`, `js-client` |
| HTTP/JSON API | Hand-written routes today; a generated gateway if kept (section 3.6) | Built (hand-written) | `js-lease-server/src/api.rs`, `run_api.rs`, `js-agent-wire` |
| Service binary | One process with a client listener and an agent listener | Built | `js-lease-server` |
| Leases and runs | The state machine of section 3.2: who holds what, with commit-before-start | Built over a file; runs are in server memory today; a lease whose host is lost expires instead of ending as lost (sections 6.3, 8 row 29); Postgres Planned | `js-lease` (table), `js-state` (store) |
| Submit-a-job call | One call that takes a lease, starts a run and releases the lease on exit; today a client must acquire a lease and then post a run (section 5.7) | Planned (step 27) | none yet |
| Postgres store | The store of leases, runs, hosts, usage history | Partly built: backend and CI job exist, server does not use it | `js-state` (`postgres` feature) |
| Placement engine | One engine for load, tail, slack, memory percent, estimates, and the walk over platforms and providers (section 3.4) | Partly built: load placement, CPU slack and tail placement exist in `js-sched` and run in kbf's embedded engine; the server's `SchedPlacer` (flag `--placement js-sched`) gets neither load readings nor the slack setting, so neither is reachable on a deployed server; memory percent is a pool key in `js-lease-server` (`agent_pool.rs`) applied to the host shape the registry books, and `js-sched` has no such key; the estimate feed is behind a flag and off by default; the walk is Planned; the server's default path is the registry picker (steps 8, 11) | `js-sched`, `js-whole`; `js-registry` picker; `js-lease-server/src/sched` |
| Host table | Which hosts exist, which are live, what each has; sessions, taint, cordon, credentials. The design keeps it as scheduler state in Postgres; there is no registry component. | Built in memory in the registry; Postgres Planned (steps 2, 6) | `js-registry` |
| Queue and reservations | Waiting requests and whole-node marks | Partly built: live in the registry, not in `js-sched` | `js-registry/src/whole/` |
| Usage history and estimates | Per-minute usage, cost curves, P90 wall times | Partly built: SQLite file, estimate feed behind a flag, no ended-lease record | `js-usage`, `js-registry/src/lives.rs` |
| Container-platform layer | A pool's `platforms` in priority order, the `ContainerPlatform` trait, and the `Default` implementation that drives the machine providers (section 3.5) | Planned; a fixed static-then-registry order is Built | `js-lease-server/src/layered.rs` |
| Machine-provider interface | One `CapacityProvider` contract, the machine-provider trait (section 3.5) | Planned; `ElasticProvider` is its Built core | `js-elastic`, `js-provider` |
| Static machine provider | Hosts we install `js-agent` on, which reach out and register (bare-metal Linux and macOS); one kind for the registry's self-registered hosts, with the host book they keep; the agentless operator-listed pool is not part of it (removed as a production kind, section 3.5, question 27) (hosts, sessions, taint, cordon, credentials, bookings) | Partly built: the registry (`RegisteredProvider`) and `StaticPool` are two separate lease-level providers; the merged `CapacityProvider` form is step 6 | `js-registry`, `js-pool-static` |
| Lambda Labs provider | A dynamic VM provider (elastic) that launches machines through the cloud's VM API | Partly built: built as `ElasticProvider`; ported to `CapacityProvider` in step 5 | `js-provider-lambdalabs`, fake `js-elastic-fake` |
| AWS EC2, GCP Compute Engine providers | Dynamic VM providers (elastic) | Planned | none yet |
| Capability vocabulary | One set of terms for what a request needs and what a host or offer has (section 3.8) | Partly built: `Requirements` has os, arch, exec, disk, cpu, memory, a GPU count and a GPU class string; no structured accelerator, no alias table in code | `js-types`, `js-caps`, `js-elastic/src/offer.rs` |
| Catalog translation, verify-on-hello | A provider returns offers in our vocabulary; a new VM's detected capabilities are checked against its offer | Planned: Lambda Labs takes GPU facts from operator-written `[[option]]` rows today and moves them into its crate's code; hello checks only the pool's shape floor | `js-elastic`, provider crates, `js-registry` |
| Other container platforms | Kubernetes, Cloud Run, ECS, each with typed parameters (section 3.7) | Planned, after v1 | none yet |
| Acquiring machines | Caps, brake and records before any acquire, for any elastic provider. A part of the Default platform in the design; its demand logic folds into placement (step 9). It is not a fleet manager. | Built as the scaler | `js-scaler`, `js-lease-server/src/scaler/` |
| Agent | Registers, heartbeats, runs jobs, contains them, reports | Built | `js-agent` |
| Linux runtime | `crun` run directly by the agent as a non-root job user | Built | `js-agent/src/crun/`, `js-contain`, `js-oci` |
| Container-engine runtime | Docker or Podman; a fallback | Built | `js-agent/src/cli_runtime/` |
| macOS runtime | Native run from an OCI bundle under a throwaway user and sandbox | Partly built | `js-agent/src/native/`, `js-mac-session`, `js-mac-pool`, `js-mac-provision`, `js-bundle` |
| Job network policy | Per-job none, allow-list, open, and declared ports | Partly built: crun mechanism built, not wired; no job field | `js-agent/src/egress/`, `js-agent-wire` |
| Object store | Not part of jobscheduler | outside | |
| Release machinery | Build, attest, publish, host-driven agent update | Built | `xtask`, `.github/workflows/release.yml`, `packaging/` |

### 3.2 Leases and runs

![State diagrams: a lease goes requested, placed, active, then ended as released, expired or lost; a run goes queued, then started or failed to start, then exited, stopped or lost; a run lives only inside an active lease.](diagrams/lease-and-run.svg)

A client sees two things.

A **lease** is a client's hold on capacity of a given shape (cpu, memory, disk, an accelerator), or on a whole host, on one host, for a time. A lease has a time to live that the client keeps extending by renewing, and a wall-time ceiling counted from activation that renewing cannot extend. Whichever runs out first ends the lease. It goes through these states:

- **Requested.** Recorded and queued until placement finds a host.
- **Placed.** A host is booked for it. The booking is recorded before the host is asked (section 5.3). A whole-host lease stays here until the agent reports the host quiet and ready, and its clocks start then (section 5.4).
- **Active.** The host's agent has confirmed it holds the booking. The client can start runs; the time to live and the wall-time clock are running.
- **Ended**, as **released** (the client asked), **expired** (time to live or wall time ran out) or **lost** (the host is gone for good). Ended means the client's hold is over. The booking is freed when the host confirms the lease is gone, or when the provider says the host is absent; a host that is only silent frees nothing.

Requested and placed are both `Pending` in the code (section 6.2).

A **run** is one execution of a job, an OCI image and a command, inside a lease. It goes through these states:

- **Queued.** Accepted and checked against the lease's shape.
- **Started.** The agent is running it.
- **Failed to start.** The agent refused it (for example the host is tainted or the limits exceed the lease); nothing ran.
- Then one of **exited** (with an exit code and a result), **stopped** (the client asked, or the lease ended) or **lost** (the host is gone and there is no result).

**How they relate.** A lease holds zero to many runs, as long as their limits fit inside its shape. By design a run never outlives its lease: when the lease ends, its runs are stopped. A run's result does outlive the lease: the run record, with its exit code and log tail, is kept after the lease ends for a retention window (section 13, question 14), so a client can read it after releasing. Today the agent is not told when a lease ends, so a run is bounded by its wall limit and can outlast the lease (step 21 of section 12.1; the Fencing bullet in the box of section 5.7). The split exists so that a client can keep warm capacity (a booked host, a pulled image) and start many short runs quickly without placing each one.

**The simple path.** A client that only wants to run one job submits the job. The service takes a lease of the job's shape, starts one run and releases the lease when the run exits; the client never handles a lease and reads the result afterwards. A client that wants warm capacity or a whole host takes the lease itself, as kbf does. Both the one-call submit and ending a lease on a lost host are Planned (step 27 of section 12.1).

### 3.3 Layers

`layers.toml`, checked by `cargo test -p xtask --test layering`, assigns each crate to `types`, `pure`, `runtime`, `server` or `app`. A crate may depend on its own layer or lower. Pure crates may not use clock, sleep, hash maps with random order, or sockets (a clippy configuration denies them). This is why placement and the whole-node cost rules are testable as functions. The dependency rule for projects is one-way: a client project such as kbf depends on jobscheduler and never the reverse; a layering test refuses any `kbf-*` crate, except the `update-hook` and `update-rights` dev-dependencies.

### 3.4 One placement engine

`js-sched` is the only place a placement decision is made. It takes a request, the hosts in its state and the offers the providers can make, and the usage history, and answers a host, a wait, or a refusal. It is a pure function over its inputs: the same inputs give the same answer, which is why it is testable without a clock or a socket.

What it decides:

- **Load placement.** A job is placed by the load readings of the hosts, not by a booking count. There is one mode.
- **CPU slack.** A setting, default 1.1x, never deliberately above it. Tuned from data.
- **Memory percent.** `memory_schedulable_percent` is the share of a host's memory the scheduler may book, 1 to 100. An agent's claim never raises it.
- **Tail placement.** A whole-node request reserves the host with the least estimated remaining time; backfill only inside the tail (sections 5.4, 5.5).
- **Estimates from history.** The tail needs an estimated end for each lease. It comes from the P90 wall time of past runs with the same profile key, kept in usage history (section 13, question 3).
- **Walking the platforms and providers.** The engine considers a pool's platforms in priority order and, inside Default, its machine providers in priority order, for ordinary and whole-node requests (section 5.6).

Nothing else places: host facts are scheduler state in Postgres and the whole-node reservation ledger is `js-sched`'s. How far each rule is built, and the steps that close the gap, are in the Placement engine row of 3.1 and steps 8 and 11 of section 12.1.

### 3.5 Container platforms, machine providers and their interfaces

Two layers, two traits. A **container platform** is what runs a job's OCI image; a pool names platforms in priority order, and `ContainerPlatform` is the trait above. A **machine provider** exists only inside the `Default` platform: it supplies machines that run `js-agent`, and `CapacityProvider` is its trait. The word provider alone in this document means a machine provider. A machine provider never runs a job and never sees a request. **Every machine it supplies runs `js-agent`**, so the scheduler talks to every host the same way, and **the scheduler never branches on provider kind**: a provider answers the same calls whatever it is.

**Platform kinds.** `Default` (v1), `Kubernetes`, `Cloud Run`, `ECS`; their parameters are section 3.7.

**Machine providers of Default.** An ordered list. Placement walks it in priority order (section 5.6). These kinds exist or are planned:

| Provider | Machines come from | `acquire` | Status | Crate |
|---|---|---|---|---|
| `static` | Hosts we install `js-agent` on, which reach out and register themselves: bare-metal Linux (`crun`) and macOS (native). A deployment always has one. | Refused: it has nothing to launch | Partly built: two lease-level providers today (the registry and `StaticPool`), one kind in step 6 | `js-registry`, `js-pool-static` |
| `lambdalabs` | A cloud's VM API, starting a machine on request | Boots a VM that runs `js-agent` | Partly built: built as `ElasticProvider` | `js-provider-lambdalabs` |
| `ec2` (AWS), `gce` (GCP Compute Engine) | A cloud's VM API | Boots a VM that runs `js-agent` | Planned | none yet |

Multi-cloud means exactly this: a cloud's VM API starts a machine that boots `js-agent`, and jobscheduler places and launches the job. It never means the cloud's job or container services. Running `js-agent` on Kubernetes nodes as a machine source is not the plan.

**The platform interface (Planned; a sketch, not a final signature).** `Default` is one implementation. It owns the machine providers: it keeps the host table, serves a lease on a host, delivers runs to the host's agent, and calls `acquire` when the engine decides to. The other kinds submit an image to the platform (section 3.7).

```rust
trait ContainerPlatform {
    fn id(&self) -> PlatformId;
    fn capabilities(&self) -> PlatformCaps;                   // flags in the vocabulary of 3.8: os, arch, accelerator models, whole host, host ports, network levels   (Planned)
    fn can_ever_serve(&self, r: &Requirements) -> bool;       // admission: from catalogs and registered hosts (Default), from capabilities (others)   (Planned)
    async fn submit(&self, s: SubmitSpec) -> SubmitAck;       // image by digest, resources, network policy, wall limit, submit id
    async fn observe(&self, r: &RunHandle) -> RunState;       // Pending | Running | Succeeded | Failed | Absent | Unknown   (Absent only after an explicit not-found)
    async fn cancel(&self, r: &RunHandle) -> CancelAck;
    async fn logs(&self, r: &RunHandle) -> LogStream;
    async fn result(&self, r: &RunHandle) -> Option<RunResult>;
}
```

For `Default`, `submit` is not a separate step: a client holds a lease on a host and submits runs on it (section 5.7), and `can_ever_serve` asks the machine providers' catalogs and the host table. Whether hostless platforms share this trait or the client surface gets a hostless lease is question 28.

**The machine-provider interface (Planned; a sketch, not a final signature).** One trait, `CapacityProvider`. Its Built core is `js_elastic::ElasticProvider` (`js-elastic/src/provider.rs`: `catalog`, `checks`, `host_prep`, `payload_limit`, `acquire`, `list`, `release`, `unresolved`, `is_gone`). The built `acquire` answers `Accepted`, `Refused`, `Unknown` or `Deferred { wait }` (`js-elastic/src/answer.rs`); `Deferred` means the call is not settled and a later `list` or retry settles it. `host_prep` and `payload_limit` shape the boot payload and stay in the target; `unresolved` and `is_gone` serve re-adoption and are subsumed by `list` and `observe`.

```rust
trait CapacityProvider {
    fn id(&self) -> ProviderId;                               // stable; the scheduler never branches on kind
    fn can_launch(&self) -> bool;                             // false for static
    fn capabilities(&self) -> ProviderCaps;                   // flags: what it can ever offer, so impossible requests are refused up front   (Planned)
    async fn catalog(&self) -> Result<Vec<Offer>, ProviderError>; // what a launch would give, translated into our vocabulary: accelerator, os, arch, image, zone, price, purchase option, quota. A failure is an error, never an empty list   (Built synchronously in ElasticProvider; the translation, the async refresh and the cache are Planned)
    fn capacity(&self) -> Capacity;                           // hosts supplied now; how many more could be acquired   (Planned)
    async fn acquire(&self, a: AcquireSpec) -> AcquireAck;    // Accepted{instance} | Refused(code) | Unknown | Deferred{wait}   (Built)
    async fn release(&self, i: &InstanceId) -> ReleaseAck;    // terminate, then read back                              (Built)
    async fn list(&self, scope: Scope) -> Vec<Instance>;      // for reaping and re-adoption                            (Built)
    async fn observe(&self, i: &InstanceId) -> Observed;      // Present | Absent | Unknown                             (Planned for elastic)
    fn verify_instance(&self, hello: &Hello) -> Option<InstanceId>; // match a first hello to an acquire            (Planned)
    fn events(&self) -> Option<EventStream>;                  // optional: reclaim notices                              (Planned)
}
```

The semantics, each of which needs a conformance case:

- **Idempotent on the acquire id.** A second `acquire` with the same id answers `Accepted` with the same instance and launches nothing. The instance carries the acquire id as a tag. (Built: the launched host name is derived from the scheduler and the acquire id.)
- **`Accepted` means launched, not ready.** The machine's own `js-agent` registers it. The provider's job ends at the first matching hello, or at failure.
- **`Refused(code)`** is a plain constant (`quota`, `capacity`, `shape`, `denied`; `js-elastic/src/answer.rs`). It is an answer: a cloud's quota error is reported here and is the platform's limit speaking. `capacity` is documented there as the signal for the caller to fall through to its next option or provider (section 5.6).
- **`Unknown`** is no answer (timeout, reset, throttle). It never launches a second instance and never frees one. (Built: `Released::Unknown` means "not known", callers retry and never read it as gone.)
- **`release`** terminates, then reads back, and reports what it read. **`observe`** says `Present`, `Absent` after an explicit not-found, or `Unknown`; only `Absent` frees a booking. **`list`** returns only instances carrying this scheduler's tags, and an error is not an empty list.
- **Commit before acquire.** The scheduler records the acquire intent (id, pool, shape) durably before it calls `acquire`, as a lease is recorded pending before the provider starts it. (Built in the scaler: records are written before any provider call.)
- **The provider owns the translation, in code.** `catalog` returns offers in the vocabulary of section 3.8. The provider crate translates its cloud's instance types and accelerator names into offers in Rust, covered by that crate's tests and the provider conformance suite. The scheduler core does not know any cloud's instance-type names, and no configuration file defines what an instance type provides.

**Flavours.** There are two behaviours, and they are data (`can_launch`), not kinds the scheduler branches on: a *registered* provider (`static`) never launches and answers `acquire` with `Refused(capacity)`; an *elastic* provider launches. A third flavour, *delegated* (a provider that runs the job itself, as a cloud batch service would), was dropped by an earlier amendment: a machine provider never runs a job, and handing an image to a service that runs it is a container-platform kind (section 3.7). The `Flavour` enum of `js-provider`, which has only `Delegated`, is deleted.

**What today's traits become.**

| Today | Becomes |
|---|---|
| `js_elastic::ElasticProvider` (`js-elastic`), implemented by `LambdaProvider` and `js-elastic-fake` | `CapacityProvider`, hosted in `js-elastic` (renamed there and widened with `capabilities`, `capacity`, `observe`, `verify_instance`). Lambda Labs and the fake implement it. This deliberately differs from `merged-code.md` step 23, which puts a new `CapacityProvider` in `js-provider` beside the renamed `Pool`; one crate for the contract and the existing implementations avoids moving them twice. |
| `js_provider::Provider` (`js-provider`), the synchronous lease-level trait with `start`, `stop`, `observe`, `capacity`, implemented by `RegisteredProvider`, `StaticPool`, `LayeredProvider`, `BoxedProvider`, `FakeProvider` | Stops being a provider. It becomes the lease table's internal booking port (`merged-code.md` step 23 names it `Pool`): "book this lease on that host, stop it, observe it". Its `Flavour` is deleted and `LayeredProvider` is removed; the ordered walk of section 5.6 replaces it. |
| `js_registry::RegisteredProvider` | The host table behind the `static` `CapacityProvider`, with `acquire` refused. The host table is scheduler state in Postgres; there is no separate registry component and no placement outside `js-sched`. |
| The `joined` pool kind (hand-started agents) | The `static` provider. There is no separate kind. |
| The kind name `static` (`js-lease-server/src/kinds.rs`, config) | **The name is reused with a different meaning.** Today `static` is the agentless, operator-listed `StaticPool`. The target `static` machine provider is `js-agent` hosts that register themselves. A configuration that uses the old kind must be migrated (question 27); it is not unchanged. |
| `js_pool_static::StaticPool`, pool kind `static` in `js-lease-server/src/kinds.rs` | **Folded into `static`, then removed as a production kind (recommended).** It is a different thing from a registered host: hosts listed in a file with no agent, bookings in memory, whole-host leases only (its crate rustdoc: "a static pool has no agent"). An agentless host cannot be verified, tainted, drained, updated or measured, and the design has one way to run a job. Keep its booking logic only as a test fake for the lease-table, `js-conformance` and `js-cli` tests, which use it today. Whether any deployment relies on the agentless kind is question 27. |
| The `layered.rs` fixed order (static pool, then registered hosts) | The ordered walk of section 5.6; `LayeredProvider` is deleted in step 8, when the walk replaces it. |

**Adding a cloud.** Implement `CapacityProvider` in a new crate, translate that cloud's catalog into offers in code, pass the conformance suite, and register the kind. The core and the configuration schema do not change; configuration only selects the new provider and limits it.

Acquiring and releasing machines belongs to the Default platform, not to the scheduler: it applies the caps and the brake and writes the records, then calls `acquire` and `release`. It does not manage the fleet of registered hosts and does not know a provider's kind. A provider's selection and limits (credentials reference, regions, allowed instance types, caps, purchase options) are declared in the configuration file; what its machines provide is code in its crate, and a new kind is a new crate behind the trait. The service renders the boot payload (join token, pinned agent build, network step) and a provider delivers it as it is; a provider never sees a network mode or a boot plan.

### 3.6 The client surface

**The service definition (Planned).** One protobuf `service` names every call a client may make: acquire, renew, release and list leases; submit, stop, get and list runs; list hosts and cordon one; read pools, providers and usage. Nothing else is a client API. The agent's own protocol (`hello`, heartbeat, `js-agent-wire`) is separate: it is JSON polling today (Built) and whether it moves to the same toolchain is step 20.

**Generated clients (Planned).** The `js` CLI and a Rust client library are generated from the definition, and so are libraries for other languages when a client needs one. A new call in the definition appears in the CLI and the library without hand-written code.

**Transport.** gRPC is the service's native transport. The HTTP/1.1 and JSON routes that exist today (`/v1/leases`, `/v1/leases/{id}/runs`, and the rest) are Built and hand-written. If they survive, they become a gateway generated from annotations on the same definition, so there is one source of truth; whether they survive is open (section 13, question 2).

**What it replaces.** `js-client` is a hand-written HTTP/JSON client (`ureq`, `rustls`) and `js-cli` a hand-written `js` binary with its own `api.rs`, `command.rs`, `leases.rs`, `run.rs`. Both are Built and both are replaced.

**kbf is a client.** kbf calls the service through the generated library: it holds whole-host leases, submits runs, and mints the read grant and the presigned upload URL for each run (section 5.7). It does not link `js-server-core`, `js-sched`, `js-state`, `js-store`, `js-usage` or any other server crate. Today kbf does link them (step 11 removes that). Any other program is a client in the same way; the service has no special client.

**Authentication** is one bearer token today (Built). Per-client tokens and quotas are Planned (section 11).

### 3.7 The other container platforms (later)

`Default` is the only platform v1 builds; nothing else in this subsection exists in code. The others are planned kinds of the same layer, each with its own typed parameters. For these, jobscheduler hands the platform the same OCI image and the platform places and runs it: there is no machine for jobscheduler to place on and no `js-agent`.

| Kind | Typed parameters (sketch) | Status |
|---|---|---|
| `Default` | `machine_providers`: an ordered list (section 3.5) | v1; partly built |
| `Kubernetes` | cluster (a kubeconfig reference or in-cluster), namespace, runtime class, node selectors, service account | Planned, later |
| `Cloud Run` | project, region, service account, VPC egress setting | Planned, later |
| `ECS` | cluster, launch type (Fargate or an EC2 capacity provider), subnets, security groups, task role | Planned, later |

Candidates, not commitments: Azure Container Apps, Nomad, Fly Machines. A kind is added when someone needs it.

**What is lost on these platforms**, stated plainly:

- **`crun` launch speed.** The platform's own start path applies (scheduling, image pull, sandbox start), not a direct `crun` start on a warm host.
- **Our host placement.** There is no host for us to place on, so our load placement, CPU slack setting, memory percent and history estimates do not choose where the job runs. The platform chooses.
- **Whole-host and tail placement.** A whole-node request has no meaning: there is no host the scheduler can reserve, drain or commit.
- **Per-job egress enforcement by our agent.** There is no `js-agent` on the job's machine, so the agent ceiling and the `network` and `ports` rules of section 5.8 do not apply. The platform's network policy applies instead, and the interface can only pass the request through.
- **Taint, post-job checks and the macOS path.** These belong to a host we control and have no counterpart.

What stays: leases (a lease is a run on the platform, with no host identity) and the queue, caps and spend limits where the platform reports cost, usage history from reported runs, and the generated client surface. The rules of section 3.5 carry over where they make sense: `submit` is idempotent on the submit id, the scheduler records the intent before it calls, `Unknown` is no answer and never frees or doubles a run, and only an explicit `Absent` frees a booking. The image is pinned by digest as for any job.

**Where they fit in a pool.** A pool's `platforms` list names kinds in priority order. A request is served by the first platform that can serve it; one that needs what only hosts we control give (a whole node, a declared host port, `network = open` under our ceiling, macOS) is passed over by a platform that lacks the capability. Capability, not kind, decides: each platform publishes capability flags (`PlatformCaps`), and admission refuses at once a request no platform in the pool can ever serve (section 5.6).

**How they map the vocabulary.** Each platform's implementation translates the vocabulary of section 3.8 into its own knobs, in code:

| Request | Kubernetes | Cloud Run | ECS |
|---|---|---|---|
| accelerator model and count | the extended resource (`nvidia.com/gpu`) with the count, plus a node selector on the GPU product label | its GPU type field, which supports few models; others are refused | GPU count in the task definition, plus an instance-type placement constraint |
| cpu, memory | resource requests and limits | service CPU and memory settings | task CPU and memory |
| arch | node selector on the architecture label | refused unless the platform supports it | the task's runtime platform (CPU architecture) |
| network | their network policy (the request is passed through) | VPC egress setting | security groups and subnets |

These rows are the design's intent, not a checked mapping of each platform's current API; each is confirmed when its platform is built.

### 3.8 One capability vocabulary

The scheduler owns one set of terms for what a request needs and what a host, an offer or a platform has. Requests, the agent's report, provider offers and platform flags all use it, so matching is one function and no cloud's names reach the core.

| Term | Type and unit | Matching |
|---|---|---|
| `os` | `linux` or `macos` | exact |
| `arch` | `x86_64` or `arm64`. `aarch64` is accepted as an alias and normalised to `arm64` when a request, an offer or a hello is parsed (in `js-caps`), so everything inside holds one spelling | exact |
| ISA level | `x86-64-v1` to `v4`, or an Armv8 version | at least, within the family |
| `cpu` | cores (thousandths of a core in `Requirements`) | at least |
| `memory`, `disk` | bytes (GiB in request syntax) | at least |
| `exec` | `container` or `sandbox-exec` | exact |
| `accelerator` | `vendor` (open set of lower-case strings, `nvidia` first); `model` (canonical lower-case family name, such as `nvidia-gb300`); `count` (integer); `memory per device` (GiB); `interconnect` (optional, a kind and a size, for example `nvlink-domain>=72`) | `model` exact after alias resolution; `count` and `memory per device` at least; `interconnect` at least when given. A field the request omits matches anything |

**What is built.**

- `js_types::Requirements` (`js-types/src/job.rs`) holds `resources` (CPU in thousandths of a core, memory, a GPU count and a GPU class string), `disk_bytes`, `os`, `arch` and `exec`. It has no ISA level and no accelerator structure.
- `js-caps` (`matching.rs`) matches a capability request against a node: `arch` and `os` exact (`js-caps` spells ARM `arm64`, and the request syntax is `arch=arm64`; `js_elastic::Offer` and the pinned agent builds spell it `aarch64`, so today an `arm64` request and an `aarch64` offer never meet), `isa_level` at least within the family, `cpu.feature` as a subset, `cpus`, `mem_gib`, `disk-gib` and `gpu` as counts, and `gpu.kind` exact. Its GPU detection (`gpu.rs`) counts GPU functions from Linux PCI data and the macOS chip; it does not name a model.
- The agent names the GPU itself: `js-agent/src/facts.rs` reads `nvidia-smi` (index, uuid, model name, memory) and spells the class as the lower-case model name with words joined by `-`, so `NVIDIA A100-SXM4-80GB` is `nvidia-a100-sxm4-80gb`. It reports one class per host and refuses a host whose GPUs are of more than one model. An operator flag `--gpu-class` can override the class, so the class is reported, not strictly detected.
- `js_elastic::Offer` (`js-elastic/src/offer.rs`) carries `gpus` and `gpu_class: Option<GpuClass>` in that same full-model spelling, and `Offer::fit` checks a demand dimension by dimension (`Os`, `Arch`, `Exec`, `CpuMillis`, `MemoryBytes`, `DiskBytes`, `Gpus`, `GpuClass`), returning the first `Dimension` that fails. The class check is string equality. Availability is not part of the fit.
- The Lambda Labs provider does not translate GPU names in code. It joins Lambda's instance-type catalog (vCPUs, memory, storage, price, architecture, regions with capacity) with operator-written `[[option]]` rows in the configuration (`instance_type`, `gpu_class`, `gpus`; `js-provider-lambdalabs/src/options.rs`), and `select.rs` turns each row and region into an `Offer` whose GPU class is the row's. A row whose type is not in the catalog, or whose GPU count disagrees with it, is skipped with a warning. The catalog's free-text GPU description is deliberately not parsed. That is the opposite of the target: the rows supply GPU facts today.

**What changes (Planned, steps 25 and 26).**

- **Lambda Labs moves to code.** The GPU facts of each Lambda instance type become a translation in `js-provider-lambdalabs`, tested by the crate and the conformance suite. The `[[option]]` rows shrink to an allow list of instance types; they no longer carry `gpu_class` or `gpus`.

- **A structured accelerator** replaces the pair `gpus` and `gpu_class` in `Requirements` and `Offer`: vendor, model, count, memory per device, optional interconnect. The agent reports the same structure.
- **Model family against full name.** Today a request must spell the full agent name, and `nvidia-a100` matches nothing because no host reports exactly that. The canonical `model` is the family-level name (`nvidia-a100`, `nvidia-gb300`); the variant facts (memory per device, form factor) are separate fields. A request that names the model matches every variant, and `memory per device >= 80 GiB` narrows it. A request that names a full spelling resolves through the table to the family and that variant's facts and matches only that variant; `Offer::fit` compares the canonical model, then the count, then memory per device, then a form factor if the request fixed one.
- **Aliases, in code.** An alias table maps every spelling to the canonical model and its variant facts: the name `nvidia-smi` prints (today's agent spelling, for example `nvidia-a100-sxm4-80gb`), the text a provider's catalog uses for a GPU, and a cloud's GPU field. A request that names a full spelling resolves through the table and means exactly that variant, so today's requests keep working. A name the table does not know passes through as its own spelling and matches only an identical one, so a new accelerator works for exact-name requests before the table learns it. **Owners.** `js-caps` (pure, shared by the agent and by provider crates) owns the canonical model names and the agent's `nvidia-smi` spellings, and is the only place a canonical name is defined. Each provider crate owns, in its own code, the translation from its cloud's source spellings to a canonical name from `js-caps`. A conformance test checks that every canonical name a provider emits exists in `js-caps`. So the agent and every provider resolve to the same canonical model. No configuration defines or overrides an alias; a new accelerator ships in a release (settled, section 13).

Illustrative rows (the cloud spellings are examples to be confirmed against each cloud's data; the GB300 agent spelling is not known today and is read from a real machine when the row is written):

| Canonical model | Variant facts | Agent spelling (`nvidia-smi`) | Cloud spelling |
|---|---|---|---|
| `nvidia-a100` | 80 GiB, SXM4 | `nvidia-a100-sxm4-80gb` (the agent's spelling today) | EC2 GPU name `A100` with its memory size |
| `nvidia-a100` | 40 GiB, SXM4 | `nvidia-a100-sxm4-40gb` | EC2 GPU name `A100` with its memory size |
| `nvidia-gb300` | to be filled from the first machine | to be captured on a real GB300 | the cloud's GPU name for it |
- **One arch spelling.** `arm64` is canonical and `aarch64` is accepted and normalised at parse, so the request `arch=arm64` fits an `aarch64` offer. A test covers it (step 25).
- **ISA level** moves from `js-caps` matching into `Requirements`, so a request can ask for it and an offer can state it.

## 4. Configurations

### 4.1 Service

Two modes, never both: flags, or one `--config` TOML file (`[server]` plus `[[pool]]` tables). Any flag beside `--config` is refused with exit 2. The file holds paths to secrets, never secrets. Unknown fields are refused. Nothing is checked over the network at start. (All Built.)

Settings that matter for design:

| Setting | Default | Meaning | Status |
|---|---|---|---|
| store | required | The Postgres connection, as a path to a file that holds it. SQLite is accepted only for tests and single-node development. | Planned |
| `boot` | required | `initialise` creates the lease file; `resume` refuses a missing one. Goes away with the file (section 10). | Built |
| listen | loopback | A non-loopback address requires TLS; certificates reload on SIGHUP. | Built |
| agent listen | none | A separate port, required with an elastic pool. | Built |
| tick | 5 s | The one place time acts (section 6.1). | Built |
| default / max ttl | 600 s / 3600 s | Lease time-to-live. | Built |
| `max_lease_wall` | none | Ceiling on a lease's wall time; required for pools with accelerators. Config file only. | Built |
| `placement` | `registry` | `registry` or `js-sched`. Goes away when `js-sched` is the one engine (step 8). | Built |
| `estimate_feed` | off | Use learned P90 wall times as lease end estimates (section 5.5). | Built |
| `require_host_credential` | off | Refuse a first hello that does not ask for a host credential (`CredentialRequired`). It does not decide whether a credential is issued: an agent that asks, presenting a fresh one-time token, is issued one regardless (section 5.1). | Built |
| usage | on | `--usage-off` opts out. | Built |

### 4.2 Platforms, machine providers and pools

A pool is a named set of hosts with a shape, a class and caps. It names container platforms in priority order; the Default platform names its machine providers in priority order, and its hosts come from them. `shape`, `max_machines`, `join_token_file`, `memory_schedulable_percent` and the queue settings below describe hosts, so they apply to the Default platform's machines only; they are rejected for a pool that names only hostless platforms.

**Target (Planned).** Machine providers and platforms are declared once and a pool names platforms. The single `provider` key stays as shorthand: a pool that names one elastic provider gets a Default platform whose machine providers are `static` followed by it, so registered hosts are preferred and the elastic provider tops up. Operator limits per pool are `allowed_instance_types`, `max_price_cents_per_hour` and `max_machines`, per machine provider; they bound what pass 2 may acquire (section 5.6).

```toml
# Configuration selects and limits. It never says what an instance type provides
# and never defines an accelerator alias; both are code in the provider crate.
[[machine_provider]]
name = "rack"
kind = "static"                       # hosts we install js-agent on; they register themselves

[[machine_provider]]
name = "lambda"
kind = "lambdalabs"                   # an elastic provider (built)
api_key_file = "<path-to-key-file>"

[[machine_provider]]
name = "aws"
kind = "ec2"                          # planned
region = "<region>"
purchase = ["on-demand", "spot"]      # ordered; a reservation or capacity-block option is added if the newest GPU types are sold only that way (confirm before step 19)
images = { arm64 = "<image-id>", x86_64 = "<image-id>" }   # selection: which machine image to boot per architecture (it carries the GPU driver stack)

[[platform]]
name = "ours"
kind = "default"
machine_providers = ["rack", "lambda", "aws"]   # first to last

# Planned, later: each kind has its own typed keys.
# [[platform]]
# name = "cluster-a"
# kind = "kubernetes"
# kubeconfig_file = "<path>"
# namespace = "<namespace>"
# runtime_class = "<class>"
# node_selector = { "<label>" = "<value>" }
# service_account = "<name>"
#
# [[platform]]
# name = "run"
# kind = "cloud_run"
# project = "<project>"
# region = "<region>"
# service_account = "<name>"
# vpc_egress = "all-traffic"
#
# [[platform]]
# name = "tasks"
# kind = "ecs"
# cluster = "<cluster>"
# launch_type = "fargate"             # or "ec2", with capacity_provider
# subnets = ["<subnet>"]
# security_groups = ["<group>"]
# task_role = "<role>"

[[pool]]
name = "linux-build"
shape = "linux-x86-16"
platforms = ["ours"]                  # first to last
max_machines = 24                     # the pool's total, every provider counted
join_token_file = "<path-to-token-file>"
memory_schedulable_percent = 90

[[pool]]
name = "gpu"
shape = "<gpu-shape>"
platforms = ["ours"]
max_machines = 8
max_queue_wait = "30m"               # a waiting request ends as no-capacity after this
join_token_file = "<path-to-token-file>"

[pool.provider_limits.aws]
allowed_instance_types = ["<instance-type>"]
max_price_cents_per_hour = 5000
max_machines = 4                      # a share of the pool's total
max_spot_machines = 2
quota_backoff = "1h"                 # after a quota refusal, pass over that instance type
acquire_retry = "60s"                 # how often pass 2 retries when every offer was refused
catalog_refresh = "1h"                # catalog cache refresh interval
offer_cooldown = "1h"                 # an offer that failed verify-on-hello is not picked for this long
size_tolerance_percent = 10           # verify-on-hello: measured sizes may be this far below nominal
```

The values shown for the settings after `max_machines` are proposed defaults, not built behaviour.

`max_machines` counts every machine in every state, every provider, in-flight acquires and tainted hosts included. A registered host that says hello when the pool is at `max_machines` is refused with `pool-full`. Only the scheduler's own launches are refused for a cap; there is no minimum anywhere and no machine is kept warm. A pool admits registering hosts only when it has a `join_token_file`. A host belongs to the pool whose join token it presents, and the `static` provider's host table is partitioned by pool: two pools that name the same platform (here `linux-build` and `gpu` both name `ours`) share its machine providers' catalogs but not their registered hosts. `max_machines` and `pool-full` count the machines of that pool.

**As built.** There are no platforms or machine-provider tables. At most one elastic pool and one static pool per server, plus `joined` pools. Provider kinds compiled in: `static` (the agentless pool) and `lambdalabs` (`js-lease-server/src/kinds.rs`). The target `static` kind reuses this name for `js-agent` hosts that register themselves; it is not the same thing (section 3.5, question 27). A fixed order, static pool first and then registered hosts, is the only ordering (`layered.rs`). Each pool has `memory_schedulable_percent` (1 to 100, default 100).

Elastic pool keys (Built): `max_instances`, `max_spend_per_hour`, `max_spend_per_day`, `idle`, `max_age`, `min_host_lifetime`, `boot_timeout`, `launch = plan | live` (default plan), `live_until`, boot-failure brake keys, the pinned agent build (URL and checksum per architecture), and `network = default | tailscale`. The Lambda Labs `[[option]]` rows (`instance_type`, `gpu_class`, `gpus`) are the allow list and the GPU facts today; they are to be aligned with the target limits (`js-provider-lambdalabs/src/options.rs`).

### 4.3 Agent

Flags only; no environment variables (`js-agent/src/lib.rs` flag table). Required: server address, pool, and a token file or a stored host credential. The choices that change behaviour: `--runtime docker|crun` (without the flag the agent uses the container engine of `--container-bin`; step 22 makes `crun` the Linux default and `docker` an explicit opt-in, section 5.7; macOS selects the native runtime with the `--native-*` flags), `--drain-wait` (default the stop grace, at most 3600 s), `--scratch-quota`, `--image-allow` prefixes, `--egress` (the ceiling of job networking, section 5.8), and the `--native-*` flags on macOS. Four modes: the agent, `image-pull`, `update-hook`, `cordon`.

The start check refuses to start rather than run half-configured. With `--runtime crun` it refuses container-engine flags, `--egress` other than `none`, mount roots, and native flags (the `--egress` refusal goes with step 12). `--allow-host-jobs` is refused in every configuration.

### 4.4 How a host reaches the scheduler

This layer is not job networking. It is one service-level layer with `default` and `tailscale` modes. A provider receives only an opaque boot payload. In tailscale mode the service mints an auth key per launch and deletes only a device whose hostname is the launched host's and whose tags include the configured tag. The service has no public listener by design, but the code enforces only that a non-loopback listener has TLS, and that status pages are served only to private peers. The wildcard bind address counts as private. Keeping the API off the public internet is operator posture today (section 13, question 9). What a job may reach is section 5.8.

## 5. How each operation works

### 5.1 Enrolment and registration of a host

![Agent lifecycle: unregistered, hello with a one-time token, welcome with a host credential, registered heartbeats, drain, lost, and forgotten.](diagrams/agent-lifecycle.svg)

This is how a host of the `static` machine provider joins a pool; an elastic provider's machines join the same way (section 5.2). Enrolment today is manual: install the package, run the packaged host setup script, mint a one-time join token (`POST /v1/agent/join-tokens`), and place the token file. A command that does this in one step is planned, in the host-management project rather than here.

1. Agent: `POST /v1/agent/hello` with host id, pool, labels, shape, capabilities, and an optional instance id (`provider:instance`). The default host id is derived from a salted hash of the machine id.
2. Server (`Registry::hello`, `js-registry/src/registry.rs`): in order, valid host id; protocol negotiated; pool exists; host not on the reserved list; credential binding (host id bound to another credential or instance is refused); no live session already (409 `host-live`); the host's execution contract (`HostContract::check`); shape at least the pool floor; room in the pool; then epoch plus one, a fresh session secret, and a record.
3. Welcome carries epoch, heartbeat interval (default 5 s), grace (default 15 s), session secret, and a 256-bit host credential when the hello said the agent keeps one (`accepts_host_credential`) and the token was a fresh one-time token, whatever `require_host_credential` says (`js-lease-server/src/agent_api.rs`, `agents.rs`). The agent writes the credential to the state root (temp file, rename, mode 0600) before using it. The token is retired in the same step that binds the host. The server writes the bindings file after the hello, but the result of that write is not checked and the Welcome is sent either way (section 8, row 36).
4. Agent: heartbeat every interval, carrying epoch, session secret, held runs, unacknowledged results, taint, and load. Reply: commands (start, stop, cancel), result acknowledgements, taint acknowledgement.

The agent's reaction to answers (`session.rs`): 409 `stale-epoch`, `unknown-host`, `host-lost`, or 403 `session-mismatch` means say hello again at once, but only if the last heartbeat was accepted (no hello loop). `pool-full`, `host-live`, 408, 429, 5xx and network errors back off (1 s base, 60 s cap, jittered, honouring `Retry-After`). Every other client error is fatal: the agent exits with status 2 and registers nothing.

Hello is rate limited per join token (burst 60, then one per second). A host id is bound to its first credential and instance, so a join-token holder cannot take a booked host's id after a restart (`.agents` file, section 10).

### 5.2 Acquiring capacity from an elastic provider: launch and join

![Sequence of an elastic launch: the service writes its records, mints a join token, the provider launches the machine with the token in its boot payload, the agent says hello, and the welcome carries a host credential.](diagrams/elastic-launch.svg)

This is what an `acquire` does for any elastic provider; today the only one is Lambda Labs (`js-provider-lambdalabs`). The order matters: records, then token, then machine. A crash at any point leaves either nothing launched or a launched machine the records already name.

1. The service decides to launch (today the scaler, section 6.5). Before any provider call it writes its records file.
2. The service mints a one-time join token (only a digest is kept; a scaler launch gives it `boot_timeout` plus a margin, 20 minutes by default, while a hand-minted token defaults to 30 minutes) and puts it in the boot payload with the pinned agent build and the network step.
3. The provider launches. The instance boots the agent, which says hello with the token and its instance id.
4. If the welcome is lost after the token is retired, the host holds nothing valid; its next hello returns 401, the agent exits, and the host sits until the scaler releases it at `boot_timeout`. It does not register twice. A launch retried after a crash is the same instance (same acquire id, same derived host name) but is handed a fresh token by its caller; whether the first token stays valid for it is not checked here (section 8, row 37).
5. On release the service deletes the instance's join tokens.

### 5.3 Acquire, renew, release

![Sequence of acquire, renew and release including the whole-node 202 pending path with a request_id and the tick that presents the waiting request.](diagrams/lease-sequence.svg)

The steps below are the lease table's contract. Where they say "save", today the store is the lease file and the target is a Postgres commit (section 10); the order of the steps is the same.

**Acquire** (`POST /v1/leases`, `js-lease/src/table.rs`, `js-lease-server/src/api.rs`):

1. Parse the body: class, resources, labels, `max_wall_seconds`, optional `probe_host`, `disk_bytes`, capabilities needed.
2. Take the one mutex around the lease table. Lock order is fixed: table first, registry second.
3. Grant a ttl (default and max clamp; zero refused), check wall time against `max_lease_wall`, allocate the next id `(term, sequence)`, check the class.
4. **Commit before start.** Insert a pending record and save the file. Only then call the provider. A failed save answers 500 `state-write-failed` and the provider was not called.
5. The booking port (today the static pool first, then registered hosts; section 3.5) answers started, refused, or unknown. Started becomes held and the clock starts; unknown stays pending; refused deletes the record.
6. Save again (`acquire-settled`). The result of this second save is ignored (section 8, row 2). A pending record whose booking the registry never made is dropped at the next tick only because the registry answers `Absent` for a booking it does not know.
7. Answer 201 `{id, host, expires_at_ms}`, or 202 `{id, state: pending}`, or an error. Error codes: 409 `no-capacity`, `no-fit`, `unsupported-class`, `no-capable-host`, `exclusive-not-ready`, `lease-ended`; 409 `probe-unsupported` and the `probe-host-*` refusals for a `probe_host` request; 400 for an invalid `disk_bytes`; 422 `wall-time-not-positive`, `wall-time-above-ceiling`, `reserved-label`, `invalid-label`, `unknown-class`, `below-class-minimum`.

A held lease expires at the earlier of now plus ttl and its wall-time end. A whole-node lease is always answered 202: it is held only when the host's agent reports it ready, and its ttl and wall clock start then. If the agent does not report ready within `prepare_timeout` (600 s), the lease ends and the host is excluded or tainted according to the cause.

**Acquire is not idempotent on the main branch.** A client that loses the 201 must list `GET /v1/leases` and match by label before retrying; otherwise it may take a second host that lapses only at its ttl.

**In review: `request_id`.** A pull request adds an optional `request_id` (1 to 63 bytes of letters, digits, dot, underscore, dash) to the acquire body. Same id and same request (class, resources, capabilities, wall time, labels, validation run, probe host; not ttl) is answered from the lease it made: 201 while held, 202 while pending, 409 `lease-ended` while releasing. Same id with a different request is 409 `request-id-conflict`. A whole-node request with an id is queued: the registry reserves a host, drains it, commits it when quiet, and the server re-presents the waiting request on every tick so the client need not poll. Release of the pending lease cancels the request in the same file replace. Without an id the behaviour is byte-identical to today. This section describes the change as proposed; until it merges, a whole-node request that finds no quiet host is answered 409 `no-capacity`, nothing is queued, reserved or drained from HTTP, and the reservation machinery of section 5.5 is exercised by tests and by embedders only.

**Renew** (`POST /v1/leases/{id}/renew`): granted ttl; only a held, unexpired lease (an expired lease is not revived); the new expiry is the earlier of now plus ttl and the wall-time end, and never earlier than the current one. The answer carries `host_reserved` when a whole-node request is waiting on the lease's host: the holder is asked to place no more work and release. Renewal is not refused for it. Drain never kills. Renew does not consult the registry: it says nothing of a host that is lost, tainted or has lost its booking (section 6.3).

**Release** (`DELETE /v1/leases/{id}`): mark releasing and save; ask the provider to stop (stop the runs, free the booking, move the host to restoring); save; answer 200 freed, or 202 releasing when the provider has not yet confirmed absence. Only a stop that reads back absent frees the lease. Releasing an already releasing lease stops it again.

**Expiry** is not a request. It is a step of the tick (section 6.1).

**After a failed save.** Each handler changes memory first and saves after, so a save error does not undo the change (section 8, row 28). Renew: the expiry is extended in memory, the client gets 500, and the extension is lost on restart. Release: the lease is already releasing and the tick will stop it, though the client got an error. Acquire: the id is consumed and the pending record stays in memory. The rule today: a client that gets a 500 from renew or release should treat the operation as possibly done and read `GET /v1/leases`; it must not assume it still holds a lease it asked to release.

### 5.4 Whole-node request

This is the contract of the whole-node path, stated for the engine of section 3.4: a wait list, a reservation pass and host states for a whole lease, over the hosts of all the pool's providers (section 5.6). Today it runs in the registry (`js-registry/src/registry/start.rs`, `js-registry/src/whole/`); step 8 of section 12.1 moves it into `js-sched`. It is reached through HTTP only once `request_id` lands.

**As built: gate order in `Registry::start`.** a lease already pending-ready answers unknown; whole plus probe is unsupported; a request id held by another lease is `no-capacity`; a request that used its tries is `exclusive-not-ready`; then queueing, only if the class is whole, the request is fresh, it fits the pool and the gate passes, and a request id is present.

The wait list (`whole/queue.rs`) holds at most 1024 requests. An entry not presented within `regain_grace` (15 s) is dropped; a presentation is how the list knows the client is alive. **Queue order is advisory**: nothing makes placement follow it, so a later request or one not yet listed can take a host that freed before the front request presents itself again.

The reservation pass (`whole/reserve.rs`) runs each tick and on each start:

1. Drop marks on hosts that became tainted or unknown; drop marks over the pool's ceiling, newest first (held leases are never touched).
2. For each waiting request in order: if a quiet host exists, assign it (no drain, not counted against the draining cap). Else, after the request has waited `drain_after` (default 0), choose among capable, unheld, unmarked, not-excluded hosts the one with the cheapest drain (section 5.5) and mark it with a deadline and the tail it was chosen on.
3. A marked host accepts no other start, except backfill (section 5.5). An unmarked host refuses a whole request that has an id: only the marked host can commit it, which is why the pass must run first.
4. At the deadline, if not capped, give up: the host is excluded for this request for `reserve_backoff` (300 s) and the mark drops. After `max_reserve_attempts` (2) give-ups the deadline becomes the lease's wall and cannot be given up.
5. When the marked host is quiet, the request's next presentation commits: the host becomes `Committed`, the start answers unknown, the lease stays pending (202).

Host states for a whole lease (`whole/exclusive.rs`, memory only): `Free`, `Committed` (lease reads unknown), `Ready` (the agent reported ready for exactly that lease; reads present), `Restoring` (needs a second heartbeat after the stop). A hello turns `Ready` into `Committed` with the commit time reset to the hello time (`after_hello`, deliberate, explained in `exclusive.rs`); only a host that was already `Committed` keeps its time. So a host that goes `Ready`, says hello, reports ready again and repeats gets a fresh `prepare_timeout` each time: a flapping agent can postpone the prepare deadline, and a whole-node lease that was held with runs goes back to waiting for ready for up to 600 s after an agent restart. On timeout it ends as a failed prepare (section 6.2). `Restoring` needs a second heartbeat after the stop; the count assumes one agent has one heartbeat in flight at a time, and the code counts a duplicate or retried heartbeat too (section 8, row 35).

### 5.5 Tail placement

![Timeline of four hosts with different estimated ends. The whole-node request reserves the host with the earliest tail; short leases backfill only where they end inside the remaining tail.](diagrams/tail-placement.svg)

The idea: a whole-node request should wait for the shortest time, and the time the reserved host spends draining should not be wasted.

**The tail of a host** (`js-whole/src/cost.rs`) is the latest end among the leases on it. A lease's end is its learned estimate (start plus P90 wall minutes), cut to its max-wall bound; else the bound; else unknown. A host with no leases has tail zero. A host with any lease that has neither estimate nor bound has an unknown tail.

**Drain cost** orders candidates by: known tail before unknown, then smaller wait, then fewer leases, then lower host id. "Reserve the host with the least estimated remaining time." A caller takes the tail once, when it sets the mark, and keeps it.

**Backfill** is allowed only for a lease of the pool's share class, never a probe, and only when `now + duration + margin <= tail_end` (`fits_in_tail`). The margin is 60 s. It fails closed: unknown duration, unknown tail, or overflow means no. A lease's duration is its P90 cut to max wall, else max wall; a wall above the server ceiling is treated as unbounded and never backfills. A host with an unknown or already passed tail takes none. The registry offers marked hosts to a start only for backfill, and the engine's adapter in the server (`SchedPlacer` today) calls `reserve_outside(worker, time_left)` for each marked host and sets the start's max wall; a start placed elsewhere or nowhere never falls back to a marked host.

**Estimate feed** (`--estimate-feed`, `js-registry/src/lives.rs`): off by default. A lease has no profile key of its own, a run does; the estimated end is the lease's start plus the largest P90 among the keys of the runs queued on it. With the feed off, tail ordering falls back to the max-wall bounds. Tail counters (started on a marked host, backfilled, estimate error buckets) are kept in memory only.

### 5.6 Placement over ordered platforms and machine providers

One engine (`js-sched`, section 3.4) places every request. Status of the walk: Planned. What is Built today for it is the engine itself, the `no-capable-host` refusal over registered hosts, `Offer::fit` (an offer against a demand, naming the first failing dimension), a fixed static-then-registered order, the scaler's choice of the cheapest priced offer that fits (then fewest GPUs, then id; unpriced offers are dropped), and the `capacity` refusal code that tells a caller to fall through. How today's server reaches a placement is in 12.1 step 8.

A request carries a class, resources, typed constraints in the vocabulary of section 3.8 (operating system, architecture, ISA level, accelerator, disk, execution mode, and the network level and host ports its jobs will ask for), a maximum wall time and labels. The steps:

1. **Select the pool** by class and labels. A request that matches no pool is refused at selection.
2. **Admission.** A host is *capable* of a request when it could serve it once it is free: it is not tainted and not draining; its total shape holds the booking; its facts satisfy every constraint. Busy and lost hosts are capable (silence is unknown, not absent); tainted hosts are not, because a taint ends only when a person reimages the host. Then, in order:
   - If a capable host exists in the pool, the request is accepted and queued.
   - Otherwise, if some platform of the pool *could ever* serve it, the request is accepted. A catalog that cannot be read (never fetched and the fetch failed) counts as *may offer*: the request is accepted and queued, and an unreadable catalog is never read as an empty one. For Default that means some enabled machine provider with a share above zero has an offer in its catalog that satisfies the request and the pool's limits (allowed instance types, maximum price); an offer that is unavailable now still counts, because `Offer::fit` ignores availability and "ever" is the question. For another platform it means its capability flags admit the request.
   - Otherwise the request is refused at once with the plain constant `no-capable-host`. It is never queued: no host, catalog or platform, now or after any bounded wait, could serve it, and the caller can act (register a host, change the request, choose another pool). A suitable host that is only busy is not a refusal. A pool whose only machine provider is `static` has no catalog, so it is refused unless a capable registered host exists.
3. **Walk the platforms in order.** The first platform that can serve the request takes it. A platform is passed over when its capability flags lack something the request needs or when every offer it could use has been refused; a later platform is asked only then.
4. **Pass 1 (Default), use what exists.** Among the capable hosts that are free now and serving, take the first in machine-provider order; within one provider the engine's score decides (load placement with the CPU slack setting, and the memory percent). A free machine of a later provider is used before anything is launched: launching while a machine sits idle is waste. Hosts match on what their agents report (the agent measures the hardware itself), never on what a catalog says. A whole-node request does not use this pass; it reserves by tail (section 5.5), as below.
5. **Pass 2 (Default), acquire only if nothing can serve now.** If no free capable host exists in any provider and no machine already booting can serve the request, walk the machine providers in order. For each, take the offers from its `catalog()` that satisfy the request, are inside the pool's limits and are available. The first provider that has any, is enabled, not cooling down, not braked, and fits under its share of `max_machines` and the pool's, gets one acquire for the demand, for the offer picked by policy: provider order first (already applied), then price per hour, then the fewest accelerators, then the least spare CPU and memory, then the purchase order of the provider (`purchase`), then the offer id. This is the order the scaler already applies (cheapest, then fewest GPUs, then id; `js-scaler/src/caps.rs`) with the later ties added, and it replaces the scaler's choice. An offer with no price is dropped, as the scaler drops it today. An offer with more accelerators than the request books the whole machine for the lease: sharing one machine's accelerators between leases is not part of this design. A provider with no usable offer is passed over and a later one is asked only then.
   - A `capacity` refusal fails over to the next zone of the same offer, then to the next offer, then to the next provider.
   - A `quota` refusal invalidates the cache and passes over that instance type in that region for `quota_backoff`; the walk continues with the next offer and provider.
   - When every offer of every provider has been refused or passed over, the request stays queued. Pass 2 runs again every `acquire_retry` and after each catalog refresh. A request that is still unserved after the pool's `max_queue_wait` ends with the existing refusal `no-capacity` (it could be served in principle, so it is not `no-capable-host`); nothing waits for ever.
6. **Book.** The lease is recorded pending before the provider is asked to start it (section 5.3).

**Whole-node requests.** The same engine places a whole-node request over the hosts of all the Default platform's machine providers, in order. Tail cost (section 5.5) picks among the capable hosts of every provider. If no capable host exists, or the cheapest tail would end after the request's drain deadline (by default its maximum wall time), pass 2 applies: the first provider that can acquire a whole machine gets the acquire, and the request waits for that machine to say hello. There is no second path beside the engine for whole requests. Other platforms have no whole-node concept (section 3.7).

A pool whose only machine provider is `static` accepts a request only when a capable registered host exists (a busy one counts); otherwise it is refused with `no-capable-host`, as step 2 says. Once accepted, the request waits for that host to free. A queued request whose last capable path goes away (a host deregisters, is tainted or ages out) is not refused at once: it stays queued for `regain_grace` so a routine restart does not turn into failures, and ends as `no-capable-host` if the grace passes. When a host frees while a machine is booting, the request goes to the host that freed and the booting machine is kept: it was paid for, and the idle rule releases it after `idle` if nothing needs it.

#### 5.6.1 Worked example: `accelerator = nvidia-gb300, count >= 4`

![Sequence of an accelerator request: the client asks for four or more nvidia-gb300; the engine admits it from catalogs and hosts, finds no free host, asks the EC2 provider's cached catalog, filters and orders the offers, acquires one, the cloud launches an arm64 VM that boots js-agent, the agent's hello carries its detected accelerator, the engine verifies it against the offer, and the lease binds; a capacity refusal fails over to the next zone and then the next provider, and a mismatch releases the machine and marks the offer bad for a cool-down.](diagrams/accelerator-request.svg)

1. **Admission.** The engine asks whether anything in the pool could ever offer four `nvidia-gb300`: registered hosts, the machine providers' catalogs (unavailable offers count), the other platforms' capability flags. If not, the answer is `no-capable-host` and nothing else happens.
2. **Pass 1.** Free hosts whose agents report an `nvidia-gb300` accelerator with count at least four, and the other constraints. The agent detects the hardware itself (`nvidia-smi`), and the alias table in `js-caps` turns the name it prints into the canonical model. If one exists, the lease binds to it and no machine is launched.
3. **Pass 2.** The engine walks the Default platform's machine providers in order. The EC2 provider's `catalog()` returns offers already in our vocabulary. The provider crate owns the translation: it reads the instance types and their GPU information from the cloud's instance-type description (for EC2, `DescribeInstanceTypes`), maps the GPU names in code, to the canonical names of section 3.8 through its own source spellings, and adds the zones where each type is offered, the price, the purchase option (spot, on-demand, or a reservation if that is how the type is sold) and the quota. The design names no instance type: the offers are those instance types whose GPU information names GB300, and which they are is decided in the provider crate's code, not in configuration. Each offer states its architecture; GB300 is Grace-Blackwell, so these offers are `arm64`.
4. **Filter and pick.** The engine keeps the offers that satisfy the request (accelerator model, count at least four, memory per device if asked), are inside the pool's limits (allowed instance types, maximum price, maximum machines) and are available. Among them the pick is the policy of 5.6 step 5: provider order, price, fewest accelerators, least spare CPU and memory, purchase order, offer id.
5. **Acquire.** The scheduler records the acquire, mints a join token, and calls `acquire` with the offer and a boot payload whose pinned agent build is chosen from the offer's architecture (an `arm64` build here). The offer names the machine image: the provider's configuration selects the image for each architecture (and region), an image that carries the GPU driver stack, so a GB300 offer boots an `arm64` image with the driver installed, and the boot payload installs the pinned `js-agent` build for that architecture. The cloud launches the VM. A `capacity` refusal moves to the next zone, then the next offer, then the next machine provider; a `quota` refusal passes over that instance type for `quota_backoff` (5.6 step 5).
6. **Verify on hello.** The VM boots `js-agent`, which says hello with the capabilities it detected. The engine compares them with the offer recorded for this acquire. Match means: accelerator model equal by canonical name after alias resolution; accelerator count at least the offer's count (more is a match, fewer is not); architecture equal; CPU, memory and disk each at least the offer's value less `size_tolerance_percent`, because the agent measures less than the nominal size. Anything else is a mismatch: the machine is released, the offer is marked bad for `offer_cooldown` keyed by (provider, instance type, zone), and the mismatch counts as a boot failure toward the pool's brake, so a wrong alias cannot loop acquire, release, acquire on paid machines. A match registers the host, and the lease binds. If the offer record is missing (section 10.1 keeps it, so this means a lost record), the hello is checked against the pool's shape floor only, and binding a lease still matches the request against the host's reported capabilities (pass 1). Binding gives a GPU job a GPU only after step 23: the `crun` path passes none today (section 5.7), so until then GPU jobs run on the container-engine fallback with `network = none`.

**Catalog freshness.** A provider's catalog is cached with a refresh interval (`catalog_refresh`) and invalidated when an acquire is refused for `capacity` or `quota`. Admission reads the cache and so is cheap; pass 2 reads the same cache, and a stale offer costs one refused acquire, not a wrong placement. A refresh that fails keeps the last good catalog and is reported; a catalog never fetched counts as *may offer* in admission (5.6 step 2). Nothing in `ElasticProvider::catalog` caches today: it is a synchronous call that returns `Result<Vec<Offer>, ProviderError>`; the target signature is the async form of section 3.5.

**What is Built and what is not.** Built: `Offer::fit`, offers from the Lambda Labs catalog (with GPU facts from operator rows in the configuration today, to move into the crate's code), the `capacity` refusal code, idempotent acquire, the agent detecting and reporting its GPU class, and an agent build chosen per architecture in the boot payload. Planned: the structured accelerator, the arch spelling and the alias table (step 25), catalog translation, caching, the offer cool-down and verify-on-hello (step 26), the provider walk (step 9), the EC2 provider with its image mapping and purchase options (step 19), and GPU passthrough into `crun` jobs (step 23; until then GPU jobs use the fallback runtime). Linux arm64 has no release archive today (section 6.6, question 19), so serving an `arm64` offer also needs that archive.

### 5.7 The job, and how it runs

![A run from submit to result in eight numbered steps: the client submits it with a create-only upload URL, the service checks and records it and hands it to the host, the agent starts it once, runs it contained, the job may write one object, the agent reports the result and cleans up, and the client reads the result.](diagrams/job-execution.svg)

A job is an OCI image pinned by digest, a command and arguments, environment, limits, optional files and a network policy (section 5.8). The same definition runs on Linux and on macOS; the host's operating system decides how. A digest names one platform's manifest, so a request also carries the `os` and `arch` that match the image, and the platform walk of section 5.6 honours them. A run is one execution of a job inside a lease (section 3.2).

**What happens, step by step.** This walk is the submit-a-job path of section 3.2. With a held lease, step 2 uses that lease; otherwise the service takes a lease of the job's shape first. The numbers match the picture.

1. **The client submits the run.** The client prepares where the job may write: a URL on its own object store that accepts exactly one new object, and a URL to read inputs. It sends both with the job (image digest, command, limits) as files. *Why:* jobscheduler never holds object-store credentials. The client decides what a job may write, and a create-only URL means the job writes once and cannot overwrite or delete.
2. **The service checks and records it.** It checks that the run fits the lease's shape together with the lease's other live runs, refuses it if not, and otherwise stores it under a run id. *Why:* a lease promises a shape, and a host must never be asked for more than was booked. The run id names the run from here on.
3. **The service hands it to the host.** The agent only calls out and the service never connects in, so the service hands the run over on the agent's next call and repeats it until the agent lists the run. A stop is delivered the same way. *Why:* a host behind a firewall works, and repeating covers a lost reply.
4. **The agent starts it once.** A worker thread owns the lifecycle, and the heartbeat loop never calls the runtime, so a slow runtime call cannot make the host look silent and lost. A start whose image is still being fetched is held as `starting` (booking and slot only) and asked again; the container is made when the image is here. *Why the start is idempotent on the run id:* step 3 repeats, so the same start arrives more than once, and a repeat must not run the job twice. An agent that cannot honour a start (for example the host is tainted) creates nothing and answers `failed_to_start`.
5. **The agent runs it contained.** `crun` starts the container as a dedicated non-root job user, with no capabilities, inside a cgroup that belongs to the lease, with a scratch directory of its own. The wall limit ends it: SIGTERM, a grace period, then a cgroup kill. *Why non-root:* the agent stays root to manage the host, and a job that breaks out lands as an unprivileged user. *Why a per-lease cgroup:* the lease's runs together cannot use more than was booked, and one lease cannot starve another.
6. **The job runs and may write one object.** It reads its files, mounted read-only, and, if its network policy allows, uploads through the URL of step 1. The object store refuses a second write to the key. *Why:* the output is fenced by the store, which jobscheduler does not check.
7. **The agent reports, then cleans up.** The result is reported as soon as the container exits, before cleanup, and the agent holds it until the service acknowledges it. Cleanup then removes the container and scratch and checks that nothing was left behind; a failed step is retried and the slot stays held until the checks run clean. A failed check writes the taint marker before the finding is reported (see Taint below). The agent returns the container's exit code; the client verifies what was stored before adopting it. *Why:* the result must not wait on cleanup, a heartbeat can be lost so an unacknowledged result is sent again, and a host is given back only once it is clean.
8. **The client reads the result.** The result belongs to the run record, which is kept after the lease ends, so the order of reading and releasing does not matter. In the one-call submit path the service releases the lease when the run exits and the client reads the result afterwards. *Why:* a client that releases its lease must not lose what the job produced. The retention window is question 14.

#### Linux: crun

Purpose: start a job fast. The agent pulls the image with its own image-registry client, verifies every blob against its digest, unpacks it into a store beside the scratch root, writes an OCI runtime configuration and runs `crun` itself as a dedicated non-root job user (Built, `js-agent/src/crun/`, `js-contain`, `js-oci`). No Docker or containerd daemon, no Podman service and no per-job socket round trip sit in the path: between a start command and a running process there is a store lookup, a configuration file and one `crun` invocation.

#### macOS: the bundle

Purpose: run the same job where there is no container runtime (`crun` is Linux-only). A `js-bundle` is an OCI image manifest of one platform (`darwin/arm64`) whose layers are sibling directory trees; its config says which tree is which, the entrypoints, the environment and what the tools need from the host. The agent pins the bundle's trees for the lease, clones the writable ones into the run's own directory and runs the entrypoint as a throwaway user under a generated sandbox profile (section 5.9). Status is in the table of 3.1.

#### The fallback: Docker and Podman

Purpose: run a job on a host that cannot run `crun`. Docker or Podman runs a job only on a host that fails the `crun` host check (a kernel that restricts unprivileged user namespaces without an AppArmor profile for `crun`, or without delegated cgroup version 2 controllers), and only with `network = none`. `crun` is the default runtime, and the fallback is refused unless the operator opts in. When the fallback is dropped is section 13, question 7. The box below has today's flags.

#### Containment

Purpose: a job must not be able to harm the host, another lease or the object store (`js-agent/src/crun/`, `js-contain`).

- The agent stays root; every container is started by `crun` as the job user, whose subordinate id range maps the job's namespace.
- The image must be pinned by digest and come from an allowed prefix. Private registry credentials, if configured, go only to the token realm the registry names, over https.
- Capability sets empty, no new privileges, a pinned seccomp profile, masked and read-only paths, a private cgroup namespace, core dumps off.
- A per-lease cgroup with the booking as limit; out-of-memory is reported only if the lease cgroup says so.
- A per-run scratch directory owned by the job identity, with an optional filesystem project quota.
- Network: the per-job policy of section 5.8.

#### Write fencing

Purpose: let a job write its output, once, without jobscheduler ever handling object-store credentials. The fence is a capability the client builds (step 1) and the agent carries. The server sends `files` only to a host that advertised the `run-files` capability. The agent keeps them in a memory-backed directory and mounts it read-only into the job; their contents are secret.

What this guarantees, provided the client builds the grant as described and the object store enforces it (jobscheduler checks neither): a job can write one object, once, and cannot delete or overwrite.

#### Taint

Purpose: keep a host that may carry state from one job into the next from taking more work. After a job the agent checks that the container and scratch are removed, that the directories the agent watches for leftovers are unchanged and that no process is left over. A failed check writes a taint marker under the state root before it is reported. A tainted host takes no start, no run and no lease. The marker is permanent and nothing in the code clears it; reimage is the only clear.

> **As built today.** This box holds the constants and the differences from the design that the text above leaves out.
>
> - **Transport.** Runs travel by the interim run API (`js-lease-server/src/run_api.rs`, `js-registry/src/runs.rs`; the API of section 3.6 carries the same calls) on heartbeat polling: a run rides on the reply to the host's next heartbeat and is resent on every reply until the host lists it. A stop is recorded as an intent on `DELETE .../runs/{run_id}` and resent on every reply until the host stops listing the run. Whether the agent protocol moves to the protobuf toolchain is step 20. A result is acknowledged with one of `accepted`, `duplicate`, `stale`, `unknown_lease`, `mismatch`. A run the host lists that the server has no record of is answered `Cancel` and never adopted.
> - **Submit.** Today a client must acquire a lease and then post a run (step 27 closes that). The client's upload grant has a size cap and an expiry of the run's wall limit plus a margin, and is presigned. The server sums the limits of the lease's live runs and refuses a run over the shape with `no-capacity`; nothing is queued.
> - **Wall limit.** The server recomputes it from the time left in the lease at each send (`valid_for_ms`), so renewing a lease cannot stretch a run that was waiting; a start that arrives later than `valid_for_ms` is dropped. It runs on the agent's monotonic clock from the moment the container is made, never past the end of the lease that `valid_for_ms` named, and counted from when the reply reaches the agent. A run can therefore outlast the lease end by transit time, queueing delay and the stop grace.
> - **Start refusals.** The agent answers `failed_to_start` with a code: for example `host-tainted`, `limits-exceed-shape`, `no-capacity` (the job does not fit what is free or names a booked GPU), `gpu-unsupported`, `egress-unchecked`, `invalid-mount`, `host-jobs-not-allowed`. A start whose image fetch fails ends every run waiting on that fetch as `failed_to_start`. The image-registry client (`js-oci`) refuses a multi-platform image index.
> - **Limits.** 256 live runs and 256 finished runs are kept per host; the log tail is the last 8 KiB of stdout and stderr. Runs are in server memory only (step 4 makes them durable).
> - **Results after release.** After a lease ends, its runs are swept and `ack` answers `stale`: a client that reads a result after releasing gets nothing, so a client must read results before releasing (section 8, row 33). The design keeps the result after release; step 27 closes the gap.
> - **`NeverStarted`.** A run the server listed as `Queued` goes `NeverStarted` after a hello gap (`Runs::sync`, `next_life`). That is the server's observation that no heartbeat ever listed the run, and it does not prove the job never executed: a delivered `Start` can run before any heartbeat lists it, and if the `crun` agent then restarts, its new runtime lists nothing. Whether it ran stays uncertain across that window (section 8, row 4). A run the agent lost (for example after a `crun` agent restart) is `Ended` with no result; the absent `result` is the only thing that separates it from an exit.
> - **Agent side** (`js-agent/src/lifecycle/mod.rs`). Replies are handed to the worker without waiting and dropped when its queue is full (the server resends). The agent remembers retired run ids in memory, at most `RETIRED_CAP` (4096, equal to `MAX_HELD`); when full, the oldest id is forgotten and a start for it would run again, and the list is empty after an agent restart (section 8, row 34). The result is sent as soon as the container exits and stays in the heartbeat until acknowledged.
> - **Fencing.** The agent has no `fence` field on a start, so a job whose lease ended keeps running until its wall limit (section 8; step 21). A `crun` job cannot make its upload until networking is wired (steps 12 and 13), so the upload is the planned shape. The agent keeps `files` in a memory-backed directory and refuses to start if the root is not tmpfs; debug output prints names and lengths only.
> - **Linux and macOS.** The repository records no measurement of launch time yet; step 18 takes one on a reference node, and this document states no number until then. Of the macOS path, the bundle format, its extractor and the agent's bundle store are built (`js-agent/src/native/`, `js-bundle`, `js-mac-session`); the single job definition is not (step 17). A native (macOS) job is a different body of the start (`native`: program, arguments, environment, inputs, outputs, network), never combined with an `image`, and a native start with `files` is refused (`native-unsupported`). Step 17 makes the image reference the one definition, pulls the bundle by digest, and adds staging of inputs and collecting of outputs.
> - **Fallback.** `--runtime docker` (`js-agent/src/cli_runtime/`) also serves anything `crun` does not carry yet (it gives jobs no GPUs) and adopts a run after an agent restart by label, which the `crun` path does not yet. Its firewall path (`js_egress`, `--container-network`, `--container-egress-allow`) is retired by section 5.8. Until step 23 it also runs jobs that need GPUs, and `--runtime docker` is refused unless the operator opts in (step 22).
> - **Containment.** The `crun` network is loopback only until it is wired, and jobs get no GPUs.
> - **Taint.** The marker lives under the agent's state root.

### 5.8 Job networking

One concept covers both operating systems: a per-job policy and an agent ceiling.

**The policy (Planned as a job field).** A job asks for `network = none | allow-list | open`, and a list of declared ports, each `{host, container}`.

- `none` (the default): no route out. A Linux job has loopback only, in its own network namespace.
- `allow-list`: the job may reach the endpoints and DNS resolvers the agent is configured with and nothing else. The list is the operator's, set in the agent's flags; a job picks the level and cannot add endpoints. This is how a job reaches the object store: the operator lists the store's endpoint, and the job uses the URL its client minted (section 5.7).
- `open`: the job may reach any address except what the agent always denies: the cloud metadata range `169.254.0.0/16`, IPv6 (dropped), and spoofed source addresses.
- **Ports.** Each declared port becomes one DNAT host-port forward from the host's port to the job's address and container port, with one forward-accept rule for that single destination and port. **There is no ingress otherwise**: new traffic toward a job is refused by default and the declared ports are the only exceptions. A host port is a resource of the lease: two jobs cannot hold the same port on one host, so placement treats a declared port as a constraint (section 5.6). Reaching a forward from the host's own loopback is not promised.

**The ceiling.** The agent's flags are the most a job may ask for: `--egress none|allow-list|open` for the level, and a list of host ports the agent may forward. A job that asks for more than the agent allows is refused at start with a code; it is never run with less than it asked for. (Today the macOS native path lowers a request to the agent's level instead (`NetworkLevel`, `js-agent-wire/src/native.rs`); the design unifies the two, and whether to refuse or lower is section 13, question 8.) The agent advertises its ceiling, and a request that needs more than a host's ceiling finds that host not capable.

**Linux mechanism (Partly built).** The `crun` runtime uses what `docs/design/agent-egress.md` calls option B, which is built in `js-agent/src/egress/` and tested as root against real containers (`crates/js-agent/tests/egress`, CI job `agent-egress`), but is **not wired into the runtime**: nothing calls it, and the agent refuses `--egress` other than `none` with `--runtime crun`.

- `crun` runs as the non-root job user and creates the container's user and network namespaces. The agent, as root, wires the namespace from outside: it makes a veth pair, takes a /30 from a pool (default `198.18.0.0/16`), moves the job's end into the namespace, and configures it with `nsenter` and one `ip -batch`. The job's init (`js-init --wait-route`) holds the job until a default route exists, so no packet leaves a half-wired namespace. It is not rootless because a root-made namespace cannot be joined by a rootless `crun` (proven by `tests/egress/premise.rs`).
- The firewall is two tables in the host's namespace, out of the job's reach: `inet js_crun_egress` for the forward rules and `ip js_crun_egress_nat` (`NAT_TABLE`, a masquerade of traffic that leaves the pool's addresses). In `inet js_crun_egress` replies are accepted, new traffic toward a job is refused, IPv6 is dropped, source spoofing is refused, `169.254.0.0/16` is refused, only the listed TCP endpoints and the listed resolvers' port 53 are accepted, and everything a job sends to the host is refused. `Egress::check` reads the tables back before every create and the job is refused if they differ.
- Limits of the allow-list today: TCP only (UDP is DNS to the listed resolvers), IPv4 `IP:PORT` or `CIDR:PORT` entries, at most 16, no hostnames.
- **What is not built:** the wiring into the `crun` runtime (call `check`, `attach` and `release` around the container start); a network field on the job (`ContainerSpec` has only an image and a command); the `open` level; the DNAT forward (in the NAT table `ip js_crun_egress_nat`) and its accept rule (in `inet js_crun_egress`), both read back by `Egress::check`; a root test that a declared port is reachable, an undeclared one is refused, and a job with no ports has no inbound path. The DNAT rule fits the lifecycle `attach` and `release` already have.
- **Cost.** The wiring is one veth and a few netlink calls per job. No start time has been measured for it; step 18 measures it. Rootless `pasta` and `slirp4netns` were compared in a spike and are not used: the agent already wires the namespace as root, and `pasta` forwards inbound ports on its own, which is the opposite of closed ingress.

**macOS mechanism (Partly built).** There is no network namespace, so no veth and no DNAT. The per-job level maps onto what exists: `none` is the sandbox profile with the network denied; `allow-list` is the packet-filter anchor keyed on the pool's uid range, installed by `js-mac-provision` and checked at agent start and before every job that asks for network (`egress-unchecked`); `open` is the sandbox profile with the network allowed and no filter rule. The ceiling is `--native-network` and the `--native-pf-*` flags. The mapping onto the flag:

| Job level | `--native-network` value | Needs |
|---|---|---|
| `none` | `off` (Built) | nothing |
| `allow-list` | `on` (Built) | the packet-filter anchor, checked before each job |
| `open` | `open`, a new value (Planned, step 15) | no anchor; the sandbox allows the network |

The existing `loopback` value has no job level and stays an agent ceiling. The sandbox cannot restrict hosts, which is why the filter exists. **Declared ports are refused on macOS** until a root test proves that the packet filter handles inbound traffic by uid. Known limits: loopback is shared by every run on the Mac, and DNS through the system resolver is outside the filter.

**The Podman path is retired.** Docker and Podman jobs today get egress from a different table, `js_egress`, installed by an operator script on a Podman bridge, with its own flags. It is one network per host and cannot vary per job. With the per-job policy there is one concept and one module (the `crun` egress module, with its two tables); the old path (`js-agent/src/cli_runtime/egress.rs`, `deploy/js-agent/egress-network.sh`, `--container-network`, `--container-egress-allow`) is removed once the `crun` runtime ships networking (step 16). A job that runs through the container-engine fallback gets `none` only.

| Level | Linux (`crun`) | macOS | Ports |
|---|---|---|---|
| `none` | own namespace, loopback only (Built) | sandbox denies network (Built) | none |
| `allow-list` | veth plus `js_crun_egress` (built, not wired) | packet-filter anchor by uid (Built) | DNAT forward (Planned); refused on macOS until a root test passes |
| `open` | the same wiring without the allow rules (Planned) | sandbox allows network, no filter rule (Planned) | as above |

### 5.9 Mac execution path

![Mac native run: journal write, throwaway user creation, run directory, sandbox profile, network rule, process under the user, then kill, user deletion, journal done.](diagrams/mac-execution.svg)

A Mac agent uses `NativeRuntime` (`js-agent/src/native/`) with a root helper, `js-mac-session`.

1. Write the lease journal entry (synced) **before** creating anything.
2. Helper `user-create`: a throwaway user named from the lease, with a uid from a configured range; names are never reused (a ledger).
3. The agent creates the run directory (exec and tmp) as root and chowns it.
4. Helper `run`: executes the job (a program from the OCI bundle, section 5.7) under a generated `sandbox-exec` profile as that user, with file-size and process-count limits set in the child. The profile is generated from a typed policy: default allow, deny writes and network, grant back narrowly, private floor emitted last. The only supported read model is `open`.
5. Network: the job's level is mapped onto the agent's `--native-network` value, which is also its ceiling (the table in section 5.8: `none` to `off`, `allow-list` to `on`, `open` to a new `open` value). `on` requires a packet-filter anchor keyed on the pool's uid range (`js-mac-provision`), checked at start and before each job that asks for network (refusal code `egress-unchecked`); `open` needs none. The sandbox cannot deny hosts when network is on, which is why the filter exists. Declared ports are refused.
6. Every process the helper or agent starts for a job goes through one spawner that disclaims the parent's privacy-grant responsibility, so a permission granted to the daemon is not inherited by jobs. A source scan test (`spawn_sites`) rejects other spawn sites.
7. Teardown: `kill-uid` repeats up to 10 rounds, killing every process of the uid and booting out its launchd domains; the verdict is the outcome (no process, no domain printing), never an exit code. Then `user-delete` removes the user, its home and its per-user temp entry. Then the journal entry is marked done.
8. If a step fails, the uid stays quarantined in the helper's ledger and the lease stays in the journal; the next worker start reclaims.

Evidence: an isolation test (`js-agent/tests/native_isolation/`) plants one item of each kind of leftover (files, processes, keychain, defaults, launchd agents, shared memory and semaphores), removes the run, starts a second run on the same uid and looks for the items. It prints one of four verdicts. It runs on a Mac only, on manual dispatch.

A bundle store (`--bundle-store`, `js-agent/src/native/bundle.rs`) already gives native jobs pinned toolchain trees. Not done: staging inputs and collecting outputs for a native run (a start with `files` is refused `native-unsupported`, `native/mod.rs`), adopting a run after an agent restart, soft stop (the helper only kills), and the reuse pool of fixed users. Native runs are not stopped when the agent stops; the wall limit runs only while an agent runs.

## 6. Background work

Sections 6.1 and 6.7 describe the file store as built. With Postgres (section 10) a save is a commit and the recovery steps that read a file read the store.

### 6.1 The tick

`js-lease-server/src/server.rs` runs one loop every `tick` (default 5 s, missed ticks delayed, not bursted):

1. Hand the registry's reservation ledger to the table if it changed.
2. `table.tick()`: for each lease, one of idle, expire, retry, resolve. Expire marks releasing and stops. Retry stops again, and observes if the stop did not free the lease (`table.rs`). Resolve observes a pending lease (present becomes held, absent drops, unknown keeps). A whole lease at its prepare deadline is observed first so the provider can count a failed prepare; only if still unknown does the next tick expire it. One atomic file write covers the leases and the ledger.
3. `registry.advance()` outside the table lock (so a renew between steps 2 and 3 reads `host_reserved` from before the advance): heartbeats lost, hosts forgotten after `lost_retention`, reservation passes, prepare deadlines.
4. Usage flush every five minutes; token files re-read.
5. Offer the tick to the scaler over a bounded channel of one. If the scaler is still busy, the tick is finished without it; its demand is handled a tick later.

As built, every save in step 2 is synchronous file I/O (write, fsync, rename, directory sync) inside the table's async mutex with no `spawn_blocking`, so a slow disk stalls the heartbeat handlers too (section 8, row 31).

A panic in the table loop or the scaler task flips an alive flag and `/healthz` returns 503. Supervision and restart are outside the repository.

### 6.2 Lease states as built

Section 3.2 defines the lease states. The code names them `Pending` (recorded, the provider has not confirmed; this covers requested and placed), `Held` (active), `Releasing` (ended, the stop not yet confirmed by the host) and removal. A refused start deletes the record. A freed lease is deleted; **the server keeps no lease history** (section 10).

The tick moves them (section 6.1). Resolve observes a pending lease: present becomes held, absent drops it, unknown keeps it. A whole-node lease at its prepare deadline is observed first, then expired by the next tick if still unknown. Expire marks a held lease releasing when its time to live or wall time ends, saves, then stops it; Retry stops again and observes. Renew works only on a held, unexpired lease and never moves the expiry past the wall-time end. Only `Absent` frees; unknown never does.

One thing the tick does not do: it observes only `Pending` and `Releasing` leases. A `Held` lease is not observed before its ttl runs out, so if the registry ends a held whole-node lease underneath it (a failed prepare, `Registry::advance_at` calling `Ending::fail`, which stops the booking and frees the host), the table keeps the lease as `Held` and renewable, the registry may book the host to another lease, and the first `queue_run` on it returns `NotHeld`. The client learns only then. Which of drop, keep or observe-each-tick is intended is undecided (section 8, row 26). The table's ordinary drop-on-`Absent` path applies to `Pending` leases only.

### 6.3 Presence and loss

A registered host that misses the grace (15 s) is lost: it takes no new lease, but any lease it holds stays booked and reads unknown, never absent, until it is heard or the ttl runs out. "Until the ttl runs out" is true only for a client that stops renewing: renew never consults the registry, so a client that keeps renewing keeps the lease until its wall time end while the host is lost. A tainted host takes no further start, but a lease it already holds is untouched until it ends (`js-registry`). The only signal a client gets is `host_reserved` on renew, which covers reservation only (section 8, row 29). A lost host with no booking is forgotten after `lost_retention`. Tainted and cordoned hosts are not forgotten for being silent.

### 6.4 Whole-node draining

Section 5.4 describes the pass. **Drain never cuts work:** a drain waits for runs to end and kills none; the one exception today is the agent's own drain cap of 3600 s, after which a run is stopped (section 13, question 12). The cap settings (`js-whole/src/config.rs`): `max_whole_node`, `keep_for_ordinary`, `max_draining` (1), `drain_after` (0), drain deadline (default: the request's max wall), `reserve_backoff` (300 s), `max_reserve_attempts` (2), `prepare_timeout` (600 s), `max_replace` (3), `regain_grace` (15 s). There is no per-consumer cap in the single-token service.

On the agent, the whole-node prepare is: the reply names an `exclusive` lease; the agent goes `preparing`, starts nothing for other leases, reports `exclusive_ready` when no run is held, post-job checks are clean and the host is untainted (or `exclusive_refused` with `host-unsafe` or `not-quiet`); at the end it goes `restoring`, rechecks, and returns to `serving`.

### 6.5 The scaler

`js-scaler` is a pure function `step(state, input, now) -> (state, actions)`. The shell in the server snapshots the registry and the provider's list, calls it, and performs actions in order. `Save` comes first whenever the step changed the records or the hold-save timer is due (`js-scaler/src/scale.rs`): all records are written whole to the `.scaler` file before any provider call.

A step: reset brakes, end go if the pool is plan-only, reconcile, cover waiting demand (a reservation, a free host, or a booting machine), undrain, drop unwanted, then launch up to a per-step limit to the cheapest priced offer that fits, if `launch = live` and every cap holds (instances, hourly, daily and total spend, boot-failure cooldown). Idle hosts drain and then release.

Failure brake: the cooldown after a failed boot is 10 minutes and doubles up to a ceiling of two hours; three failures in a row trip the pool to plan-only. The trip survives restart and is cleared only by `POST /v1/pools/{pool}/brake/reset`. Spend caps only block launches; they never release a machine, so overrun is bounded only if the pool also sets `max_age` and the server sets `max_lease_wall`.

The first live launch with a real cloud account is a runbook (`docs/runbooks/first-live-launch.md`) with an explicit go step. This document does not claim a live launch has happened.

In the design the scaler's demand logic folds into pass 2 of the placement walk (section 5.6, step 9 of section 12.1): placement decides which provider is asked to acquire, and the scaler keeps the caps, the brake and the records.

### 6.6 Release and update

![Release and update flow: push to main, ci, plan, build, sums, attest, verify, publish, the deploy step setting desired versions, then the host updater running the agent hook steps prepare, cutover, verify, rollback.](diagrams/release-update.svg)

**Release** (`docs/release.md`, `.github/workflows/release.yml`): every push to main that changes shipped code is the next patch version once every `ci` job except the Mac one passes. Jobs, by their names in the workflow: `ci-passed` (waits for ci, polling), `plan` (version; refuses a commit with a later tag past it), `build` (static Linux x86_64 and macOS arm64 archives), `sums`, `attest`, `verify`, `publish` (draft, upload, ancestor check, publish creating the tag last), `deploy-tool`, `deploy` (the deploy step). The shipped set is computed from the dependency graph of the shipped binaries, not listed. Archives are byte-identical on rebuild (sorted names, commit time as mtime, zero owner, no gzip name). Docs-only pushes end at plan, green.

Shipped: Linux: `js-lease-server`, `js`, `js-agent`, `js-bundle`, `js-mdm`; macOS arm64: `js`, `js-agent`, `js-bundle`, `js-mac-session`, `js-mac-provision`. Not shipped: a Linux arm64 archive. Darwin binaries are not signed or notarized.

**Deploy step** (the `deploy` job): sets the desired version of the server service, then the agent service, on the update coordinator, and polls convergence every 30 s. It passes when converged with no lost hosts; it fails on a blocked host, lost hosts, 20 minutes with no change, 60 minutes total, an authorization error, or another writer replacing the desired version. It is off unless a repository variable names the coordinator.

**Host-driven agent update.** The agent update is a hook, `js-agent update-hook prepare | cutover | verify | rollback | cleanup`, declared in a root-owned file (`packaging/js-agent/updater.d/js-agent.toml`): prepare cordons the host through the server and waits for no runs; cutover moves the release link and restarts the unit; verify waits for the new agent's hello; rollback restores. Exit codes: 0 done, 1 failed, 75 deferred, 76 halt. The declared timeouts (prepare about 65 minutes, cutover 72, verify about 10, rollback 137) are above the agent's drain cap. The generic updater and coordinator live in another project.

There is no update hook for the lease server. Its update path is open (section 13, question 10). An update of the server drops all runs.

**Trust decision.** The declaration names this repository's release workflow as the signer, so whoever can push to main can ship code that runs as root on every host. The repository had no ruleset at the time of writing. Section 13 lists this as a decision.

### 6.7 Restart recovery

![Restart: flock, load lease file, new term, restore ledger with grace, observe held leases, seat hosts as awaiting, agents re-hello, runs forgotten.](diagrams/restart-recovery.svg)

As built with the lease file; the picture shows these nine steps. With Postgres, step 1 takes the advisory lock, step 2 reads the leases, reservations and hosts from the store, and step 3 takes the next term from the term row; steps 4 to 8 are unchanged except that wait-list entries return with their bodies, and step 9 goes away once runs are durable (step 4 of section 12.1).

1. Take the exclusive `flock` on `<file>.lock`.
2. Read the lease file. Refuse it if the count of leases differs from the stored count, an id is duplicated, or a row is bad.
3. Take the next term. Ids are `(term, sequence)` and sequence only grows, so ids never repeat across boots.
4. Restore the reservation ledger: held deadlines are raised to at least boot plus 120 s (never stored plus grace, so restart loops do not stretch a drain); marks are stamped `claim_by = boot + regain_grace`; wait-list entries return without their request body and wait to be presented again.
5. Observe each held lease once: present stays, absent drops, unknown keeps.
6. Seat each lease's host as `awaiting`: unobservable, unbookable, counts toward the pool size, until the agent says hello again. The `.agents` file restores bindings and taints.
7. Agents get 409 `unknown-host` on their next heartbeat and say hello. Whole-node bookings must report ready again.
8. Lease expiry has no boot grace. Expiry is an absolute wall-clock time in the file, so the first tick after boot expires every lease whose time passed during the outage and stops it; its runs are cancelled when the agents say hello. A server outage longer than the ttl therefore drops every lease whose client could not renew. Only the reservation deadline has a boot grace (120 s). A lease that was valid at the crash but whose host never says hello stays booked as `awaiting` until its ttl (section 8, row 30).
9. Runs are gone. A host that lists one is answered `Cancel`; a client asking gets 410 `run-forgotten`.

Marks on hosts that never say hello are dropped after `claim_by`.

## 7. Edge cases handled

Rows about the lease file describe the file store as built; a Postgres transaction replaces the write, sync, rename sequence.

| Case | What the code does | Where |
|---|---|---|
| Crash between pending write and provider start | Pending record is on disk; next boot or tick resolves it by observation; no booking nobody knows about. | `js-lease` |
| Provider says unknown | Start stays pending; stop stays releasing; only absent frees. | `table.rs`, `book.rs` |
| Lease file write fails on acquire | Provider not called; the record stays in memory and is dropped at a later tick because the registry reads `Absent` for a booking it never made. | `table.rs` |
| Lease file replace fails | Write temp, sync, rename, sync directory. A failure before the rename leaves the old file. A failure after it (opening or syncing the directory) returns an error with the new file already in place and its crash durability uncertain. Restart refuses a short file. | `store.rs` |
| Renewal not saved | In-memory expiry only; the tick retries the save while unsaved. | `table.rs` |
| Whole-node ready report races the prepare deadline | Registry is asked first; expire only if still no answer a tick later; host restored. | `book.rs` |
| Flapping agent during whole-node commit | A hello keeps the commit time of a `Committed` host. For a `Ready` host the time is reset to the hello time, so the deadline can be postponed (section 8, row 27). | `exclusive.rs` |
| Scaler double launch | Records written whole before the provider call; acquire ids idempotent; panic flips health. | `scaler/` |
| Hello flood | Per-token bucket; session secret digest on every heartbeat; host id bound to credential and instance. | `registry.rs` |
| Taint | Written before reported; acknowledged only after the server wrote it; permanent. | `lifecycle/mod.rs`, `agent_state.rs` |
| Connection abuse | 256 connections max, permit before accept, handshake, header, read and write timeouts, max requests per connection, 64 KiB head and body caps, 10 s drain. | `lease-server` |
| Result lost | The agent holds results until acknowledged and resends. | `lifecycle/mod.rs` |
| Duplicate start | Idempotent on run id; retired ids do not run again while remembered (the newest `RETIRED_CAP`, 4096; in memory, lost on agent restart; section 8, row 34). | `lifecycle/mod.rs` |
| Welcome lost after token retired | Host holds nothing valid; hello 401; agent exits; scaler releases at boot timeout. | `agent_api.rs` |
| Stop of an unknown run | Its id is retired, so a later start does not run while the id is remembered (same bound as above). | `lifecycle/mod.rs` |
| Mac teardown fails | Uid quarantined; lease stays in journal; reclaim at next start. | `native/mod.rs` |
| Image garbage collection under a lease | A pin keeps a leased image from removal until the lease is removed. | `js-contain` |

## 8. Races and failures the current code loses

This is the test list. "Open" means the code loses it today and a test should be written, first failing, then fixed. "Documented" means the code behaves as documented and a test should pin that behaviour. "Suspected" means read from the code but not confirmed; write the test before relying on it. "Accepted" means a deliberate limit with no test planned. "Partly proven" means some evidence exists and the rest is a gap. Nothing here is a claim that a test exists.

Rows 2, 7, 8, 28 and 31 are properties of the file store; step 2 of section 12.1 closes them by moving to Postgres transactions, and their tests are then written against the new store. Rows 3 and 33 close with step 4, rows 11, 17, 22 and 26 with step 8, and rows 5 and 6 are in step 21.

| # | Failure | State | What happens | Test to write |
|---|---|---|---|---|
| 1 | Acquire answer lost | Open | A retry can book a second host that lapses only at ttl. The proposed `request_id` closes it for clients that send one. `js lease run` adds a random label, so it stays unsafe. | Acquire, drop the answer, retry with the same id: one lease. |
| 2 | Ignored second save on acquire and release | Open | `acquire-settled` and `release-settled` saves are ignored. After a crash following a failed `release-settled` save, a freed lease is found releasing and stopped again: that side converges. After a failed `acquire-settled` save, the lease on disk is pending with no host; the registry's booking was in memory and is gone, so the tick reads `Absent` and drops it. It converges to **dropped**, not held: a client that was told 201 gets 404 on renew. Untested. | Fail the second save of each, restart, assert dropped (acquire) and stopped again (release). |
| 3 | Runs are not persisted | Open | A server restart cancels every run on every host and the work is lost; the client sees 410. | Restart with a run in flight: agent told to cancel; client gets `run-forgotten`. |
| 4 | Agent restart loses crun runs | Open | Containers are in the runtime's memory. The new process removes leases its predecessor left and lists none. The server keeps the booking and the lease stays held and renewable; the missing runs become `Ended` with no result (section 5.7, item 7). The Docker and Podman path adopts by label. | Kill agent mid-job; assert cleanup and re-run budget. |
| 5 | No agent self-fence | Open | Documented in older designs; absent in code. A partitioned agent keeps running until the run's wall limit; a lease the server gave up is not stopped by any token. Native Mac runs are unbounded while the agent is down. | Partition an agent past lease expiry; assert the job stops within a bound. |
| 6 | No fencing token on a start | Open | The wire's `fence` field is not built. An expired lease's job can overlap the host's next lease until its wall limit. | Expire a lease with a running job; assign host; assert the old job cannot write. |
| 7 | Single writer is flock only | Open | Local `flock` stops a second process on one filesystem. On a network filesystem it may not hold and two tables could issue the same ids. | Not testable in CI; document. |
| 8 | Restoring an older lease file | Open | Can reissue `(term, sequence)`. The only fix is hand-editing `term` and `next_sequence`. | A restore verb with a term bump. |
| 9 | Clock stepped | Open | Wall times are wall-clock instants. A step back stretches a lease; a step forward cuts it. Not only leases: host liveness (15 s grace), reservation deadlines, wait-list expiry, join token expiry and a run's end all use the system clock, and a forward step past the grace marks every host lost at once (row 32). | Injected clock test that pins current behaviour. |
| 10 | Drain cost after restart without the estimate feed | Suspected | A restored lease gets a recorded end bound only if the feed is on. A busy restored host may then show an empty bound list and tail zero, ranking as cheapest to drain. Unverified: write the test before relying on it. | Restart with a leased host, feed off; assert the drain cost. |
| 11 | Wait-list order advisory | Documented | A later request can take a host before the front request presents again. | Two waiters; free host; assert who gets it. |
| 12 | Tick as the poll | Documented | If `tick` is not below `regain_grace`, or a tick stalls, a waiting request is dropped and re-enters at the back. Documented only; not enforced at start. | Tick longer than grace; assert re-queue. |
| 13 | Restored marks before hello | Documented | A pass with no capable host drops a restored mark if no agent has said hello yet. | Restart with a mark; no hello; assert behaviour. |
| 14 | Reservation ledger one tick behind | Documented | A crash may lose up to one tick of mark changes. | Crash after mark, before tick. |
| 15 | Whole-node preparation lost on restart | Documented | `Committed` and `Ready` are memory only; a hello voids ready; preparation restarts. | Restart during prepare. |
| 16 | Scaler tick skipped when busy | Documented | Demand handled a tick later. | Slow provider; assert demand covered next tick. |
| 17 | Non-whole large request starvation | Documented | No reservation for share-class requests by design. | Share request vs steady small stream. |
| 18 | Join-token sharing | Open | A holder can say hello for made-up ids to fill the pool or exhaust the shared rate bucket. Closed per host once the host holds a credential; an agent that asks for one gets it. `require_host_credential`, off by default, refuses a first hello that does not ask. | Hello with made-up ids; assert pool-full only for the token holder's share. |
| 19 | Single trust domain | Accepted | One token; sequential lease ids; any holder can run work on and read any lease's runs. | Documented, not tested. |
| 20 | ETXTBSY on installed helper | Open (fix in review) | Writing an executable in a process that also forks can give a child the write descriptor; the exec fails with text busy. Rare. | Fork-while-writing stress test. |
| 21 | Lost `exclusive-not-ready` | Documented | The failure is reported once; a retry is a fresh request. | Retry after lost answer. |
| 22 | Scheduler rebuilt per start, under two locks | Open | Unmeasured beyond tens of hosts. | Benchmark at 1000 leases. |
| 23 | TCC grant inherited by jobs | Partly proven | Every job process is started through the disclaiming spawner and a source scan test (`spawn_sites`) rejects other spawn sites. The hosted-runner readback of the responsible pid is still to be run. | Hosted-runner readback of responsible pid. |
| 24 | Mount-root symlink swap | Documented | Docker and Podman path with a read-write mount root can race a swap; checked after the fact. The crun path accepts no mount roots. | Swap during bind. |
| 25 | Mac uid reuse and shared memory | Partly proven | Profile does not deny SysV or POSIX shared memory; teardown removes them; verified by the dispatch-only isolation test. | Run isolation test on each release. |
| 26 | Registry ends a Held lease the table does not observe | Open | A failed prepare, or a hello that voids `Ready`, ends a held whole-node booking in the registry. The table does not observe held leases, so it keeps the lease renewable; the host can be booked to another lease; `queue_run` returns `NotHeld`. Double-booking path. Undecided: drop, keep, or observe held leases each tick, and what the client sees. | End a held whole lease in the registry; assert the table's state and what renew and a run call return. |
| 27 | Hello resets the commit time of a `Ready` host | Documented | Deliberate (`exclusive.rs`). A flapping or restarting agent gets a fresh `prepare_timeout`; a whole lease that was held waits for ready again for up to 600 s, then ends as a failed prepare (row 26). | Ready, hello, ready, repeat past `prepare_timeout`; assert the deadline moves. |
| 28 | Failed save after the memory change | Open | Renew extends in memory then 500; release marks releasing then errors; acquire consumes the id and keeps the pending record. The client cannot tell whether the operation happened. | Fail each save; assert memory, file after restart, and what a retry does. |
| 29 | Lost, tainted or booking-less host gives the client no signal | Open | Renew never consults the registry. The lease lasts until a client stops renewing or wall time ends. | Lose a host under a renewing client; assert what renew and `GET /v1/leases` say. |
| 30 | Server outage longer than the ttl | Open | The first tick after boot expires every lease whose time passed; clients that could not renew lose their work. A lease whose host never says hello stays `awaiting` until its ttl. No generic boot grace (section 13, question 15). | Restart after a gap longer than the ttl; assert which leases survive. |
| 31 | Synchronous lease-file writes on the async runtime | Open | Each save is write, fsync, rename, directory sync inside the table mutex on a runtime worker. A slow disk stalls heartbeat handlers, and healthy hosts can pass the 15 s grace and read lost. No bound is decided between save time and grace. | Inject a slow save; assert heartbeats are still answered inside the grace. |
| 32 | Wall-clock step beyond leases | Open | See row 9. Host liveness, reservation, wait-list, join token and run end use the system clock. Only the agent's `valid_for_ms` counting is monotonic, and it starts at the reply's arrival, not at send. | Step the clock forward past the grace; assert no host is lost, or document it. |
| 33 | Results unreadable after the lease ends | Open | `ack` returns `stale` and `sweep` drops finished runs once the lease no longer holds the host. The design keeps a run's result after its lease ends, for a retention window (section 13, question 14); the code does not. | Release, then read a finished run's result. |
| 34 | Start resend and the retired-id list | Open | A `Queued` run whose accepted result arrives before the server has seen it listed is resent; only the agent's in-memory retired list stops a rerun. The list holds the newest 4096 ids (`RETIRED_CAP`); an older id is forgotten and can run again (`the_memory_of_stopped_runs_is_bounded`), and the list is empty after an agent restart. | Restart the agent, replay a stale Start; assert the job does not run again. |
| 35 | Heartbeat order around the second heartbeat after a stop | Documented | `Exclusive::after_beat` counts any admitted heartbeat, duplicates and retries included, and assumes one heartbeat in flight per agent. | Send a duplicate heartbeat after a stop; assert the restore count. |
| 36 | Bindings save result ignored on hello | Suspected | `save_bindings` is awaited and its result dropped; the Welcome, which may carry a new host credential, is sent either way. If the credential is persisted only in the bindings file, a crash after a failed save leaves the host at 401 until the scaler releases it. Where the credential is persisted is not confirmed. | Fail the bindings save on hello, restart the server; assert the next hello. |
| 37 | Token on a retried scaler launch | Suspected | A retry is the same instance, but the caller gives `Launcher::acquire` a fresh token. Whether the first token (in the payload the instance booted with) stays valid is not confirmed; the join-token store was not read. | Crash after launch, retry; assert which token the instance can use. |

## 9. Assumptions

- One service process is the active writer of the store. Today operator discipline enforces it, with `flock` on the lease file as a guard; the target is a Postgres advisory lock (section 10.2).
- Clients are trusted with each other's leases.
- A host's own account is not hostile before a job runs. After a job runs, it might be; taint covers that.
- Wall-clock time moves forward. Only the agent's job limits use its own monotonic clock; the server's host liveness, reservation, wait-list, join token and lease times are wall clock (row 32).
- The network between agent and server drops and delays but is not hostile on a private network; TLS and tokens apply off loopback.
- A heartbeat reply can be lost; every command is therefore repeated until acknowledged by the next heartbeat.
- The agent's package and its update hook are installed by a human or by the host updater, not by this service.
- The estimate of a lease's remaining time is advisory. A wrong estimate wastes time; it never makes a job fail early, because backfill is bounded by max wall.

## 10. State

**The store is Postgres.** Leases, runs, hosts and usage history live in one Postgres database, behind the `js-state` `Store` trait (`js-state/src/store.rs`). Status: Partly built. The backend exists and is tested against a real Postgres in CI (`state-postgres`, feature `postgres`, off by default); `js-lease-server` does not enable the feature, and keeps its leases in a file and its usage in SQLite. Step 2 moves the server.

**SQLite** stays in `js-state` as the backend of tests and of single-node development, where a database server is a burden. It is not a supported store for a deployment. **The lease file** and the other state files of today are removed after a one-time import (step 2).

### 10.1 What is stored

| Area | Contents | Today | Target |
|---|---|---|---|
| Leases | Lease records (id as term and sequence, state, expiry, wall end, holder labels), `request_id` records for idempotent acquire | One TOML file replaced whole | Postgres rows |
| Reservations | Whole-node marks, give-ups, exclusions, wait-list entries with their bodies | Same file (bodies lost on restart) | Postgres rows |
| Runs | Run records, state, result, bounded log tail, ack state | Server memory only | Postgres rows, with a retention window |
| Hosts | Bindings (host id to credential and instance), host credential digests, taints, cordons, join-token digests | `.agents` and `.joins` files | Postgres rows |
| Acquires | Acquire intents, the offer chosen for each acquire, offer cool-downs, scaler records, unremoved machines, brake state | `.scaler` and `.scaler.unremoved` files (no offer record or cool-down) | Postgres rows |
| Tail counters | Started on a marked host, backfilled, estimate error buckets | Memory only | Postgres rows (step 3) |
| Usage history | Per-minute node and lease usage, cost curves, P90 wall times by profile key, and a record of every ended lease (holder, host, duration, why it ended, drain and give-up outcome, estimate against actual) | SQLite file (usage and curves only; ended leases are deleted) | Postgres rows |

Not in the database, by design: live sessions, session secrets, heartbeat times, the whole-node `Committed`, `Ready` and `Restoring` states. They are rebuilt by the agents' next hello or measured again; a heartbeat is not a write.

On the host (unchanged): host credential, taint marker, owed checks and ledger sidecars in the agent's state root (Built); results not yet acknowledged and retired run ids in agent memory (Built, lost on agent restart).

### 10.2 Rules

- **Commit before start** becomes a transaction. The pending lease row is committed before the provider or the agent is asked to do anything; a failed commit answers an error and nothing was done. The "save the file" steps of sections 5.3 and 6 are commits.
- **Ids.** `(term, sequence)` with `term` taken from a row incremented in a transaction at boot and `sequence` from the database. Ids never repeat across boots and a restore of an older backup cannot reissue one if the term row is the authority; the restore procedure is still a decision (section 8, row 8).
- **One writer.** The active service holds a Postgres advisory lock for its life; a second process refuses to start or waits. This replaces `flock` on a local file, which does not hold on a network filesystem. Whether a standby takes over is section 13, question 1.
- **Schema changes are additive**: a new table, or a new column with a default. A build one release back must read the new schema.
- **Reads on the hot path** (heartbeat, host book) come from memory rebuilt at start; the database is written when a durable fact changes.

Today's file rules, for the as-built reader: the lease file format is additive; unknown fields are kept and written back; a higher version is accepted; there is no minimum reader version field.

## 11. Limits

- Leases: ttl default 600 s, max 3600 s; wall ceiling from `max_lease_wall`; ids stop at the largest signed 64-bit value.
- Runs: 256 live and 256 finished per host; run command cap per heartbeat reply 1024; log tail 8 KiB per stream.
- Wait list: 1024 requests.
- Connections: 256; head and body 64 KiB.
- Hello: burst 60 per join token, then one per second.
- Agent drain: at most 3600 s, defaulting to the stop grace. A run that outlives it is stopped at termination.
- Pool: at most one elastic and one static pool per server, as built (step 7 lifts it).
- Mac: uid range at most 10000 users; kill rounds 10.
- Throughput of the table, as built: one mutex; every change rewrites the whole file (the Postgres store removes the rewrite, step 2). Fine at tens of hosts; unmeasured above that. Each write is synchronous disk I/O on a runtime worker, and no bound is decided between save time and the 15 s heartbeat grace (row 31).
- Client token: one. No per-client quotas.
- Platforms shipped: Linux x86_64 and macOS arm64.

## 12. Future work

### 12.1 From today's code to the design

"Done" means: every host class under the scheduler as a machine provider of the Default platform in ordered pools; a request by capability finds the provider that has it; one placement engine with whole-host requests reserved by tail and fair; history that feeds the estimates; Macs under isolation we can prove; and jobs that start fast on both operating systems with the network they declared.

**How today differs.** Each row is a difference between the code and the design above, and the steps that close it.

| Today (as built) | Design | Steps |
|---|---|---|
| Placement happens in three places: the registry's first-free-host picker (default), `SchedPlacer` (`js-lease-server/src/sched/place.rs`, a `js-sched` scheduler rebuilt for every start under both locks, only with `--placement js-sched`, no load readings, no CPU slack), and `js-server-core` (a long-lived load-placement engine that kbf embeds and the server does not use). Whole-node reservation lives in the registry. | One engine, `js-sched`, with queue and reservation. | 8, 9, 11 |
| Two models of compute: the registry (`RegisteredProvider`, implements `js_provider::Provider`) and the scaler with `js_elastic::ElasticProvider`, joined only because launched machines register themselves. A third, the agentless `StaticPool`, is a separate lease-level provider. The ordered hierarchy is a fixed static-then-registry order (`LayeredProvider`). One elastic and one static pool per server. No platform concept. | A `ContainerPlatform` layer with `Default`, whose ordered machine providers are one `CapacityProvider` interface and the kinds `static`, `lambdalabs`, `ec2`, `gce`; the other platforms later. | 5, 6, 7, 24 |
| Offers carry a GPU count and a class in the agent's full-model spelling, matched by string equality (`js-elastic/src/offer.rs`); Lambda Labs offers take their GPU class and count from operator-written `[[option]]` rows in the configuration; no alias table in code; hello is checked against the pool's shape floor, not against an offer; no cache or cool-down on catalogs; no Linux arm64 archive. | One vocabulary with a structured accelerator and an alias table in code; catalogs translated in code by the provider crates, with the configuration only selecting and limiting; verify-on-hello; cached catalogs with failover. | 9, 19, 25, 26 |
| One process with a lease file replaced whole on every change, files beside it for bindings and scaler records, SQLite for usage, runs only in memory, ended leases deleted. | Postgres for all of it. | 2, 3, 4 |
| Hand-written `js-client` and `js-cli` over HTTP/JSON; no service definition. | A protobuf service; generated CLI and library. | 1 |
| kbf links `js-sched`, `js-whole`, `js-server-core`, `js-store`, `js-usage`, `js-caps`, `js-sizing`, `js-sim`, `js-agent-usage` and `js-state` (checked in its manifests at the pinned revision). | kbf calls the generated library. | 11 |
| `crun` jobs have loopback only; the egress module exists and is refused at start; the job has no network field; the Podman path has its own firewall. Native jobs carry a `NetworkLevel` lowered to the agent's. | Per-job policy, ports, one concept; `crun` the default runtime. | 12 to 16, 22, 23 |
| A container job is an `image`; a native job is a different `native` body. | One image reference on both platforms. | 17 |
| Hosts, sessions, taint and the whole-node ledger live in `js-registry` memory and a ledger file; runs reach a host through the registry on heartbeat replies; a client must acquire a lease and then post a run; a lease on a lost host expires instead of ending as lost. | Hosts are scheduler state in Postgres; reservations are `js-sched`'s; run delivery is part of the agent protocol; one call submits a job; a lost host ends its leases. | 2, 4, 6, 8, 27 |

**The steps.** Each step names what it needs first. A step lands as its own change with its own tests.

1. **Service definition and generated clients.** Write the protobuf service for leases, runs, hosts, pools and usage. Generate the Rust library and the `js` CLI from it; if question 2 of section 13 resolves to B (the recommendation), generate the HTTP/JSON gateway from the same definition with today's routes unchanged on the wire, with a test that runs the previous release's client against the new server and the reverse; otherwise leave the hand-written routes. Replace `js-client` and `js-cli`. *Needs: nothing.*
2. **Postgres store for leases.** Run the lease table, reservations (wait-list bodies included), host bindings, join-token digests and scaler records over `js-state` with the `postgres` feature. Commit-before-start becomes a transaction; `term` and `sequence` come from the database; an advisory lock replaces `flock`; a one-time import reads the lease file. SQLite remains for tests and single-node development. This removes the file-save races of section 8 (rows 2, 7, 28, 31, and the id reissue of row 8). *Needs: nothing.*
3. **Usage and history in Postgres.** Move `js-usage` to the same store; record every ended lease with the fields of section 10.1; persist the tail counters (the new row of section 10.1). *Needs: 2.*
4. **Durable runs.** Store run records and results in Postgres with a retention window, so a server restart does not lose them and a result can be read after release for that window (section 8, rows 3 and 33). *Needs: 2.*
5. **`CapacityProvider`.** Rename and widen `ElasticProvider` with `capabilities` (flags, so an impossible request is refused up front), `capacity`, `observe` and `verify_instance`; port `LambdaProvider` and `js-elastic-fake`; add a conformance suite every provider runs. *Needs: nothing.*
6. **Static machine provider (the merge).** Put the registry's host table behind one `static` `CapacityProvider` (hosts we install `js-agent` on, which register; `acquire` answers `Refused(capacity)`). Delete `Flavour`, rename the lease-level `Provider` to the internal booking port, and delete the `joined` pool kind (`LayeredProvider` stays until step 8's ordered walk replaces it). Remove the agentless `StaticPool` as a production kind and keep its booking logic as a test fake for the lease-table, `js-conformance` and `js-cli` tests (section 3.5; question 27 first checks that no deployment uses it). *Needs: 5.*
7. **Platform layer with Default, ordered machine providers, configuration and admission.** The `ContainerPlatform` trait with `capabilities` and `can_ever_serve`, and the `Default` implementation that owns the machine providers; the `[[platform]]` and `[[machine_provider]]` tables, `platforms`, and `provider_limits` (allowed instance types, maximum price, maximum machines); several elastic providers and pools per server; admission with `no-capable-host` over registered hosts and catalogs (an unreadable catalog counts as *may offer*; until step 26's cache lands, admission calls `catalog()` directly), pool settings such as `max_queue_wait`, and `regain_grace` for a queue that loses its last capable path (section 5.6). It wires configuration and admission over the current picker; the ordered walk itself is engine behaviour in step 8. *Needs: 5, 6.*
8. **One placement engine.** Move the registry's start path and wait list onto `js-sched`: queue order becomes binding, load readings and the CPU slack setting reach the server, the whole-node reservation ledger moves out of the registry, the give-up is removed, `SchedPlacer` and the registry picker and `--placement` go. `LayeredProvider` and `layered.rs` are deleted here, replaced by the ordered walk. The engine takes hosts per machine provider of the Default platform, in order (the ordered providers come from step 7), and implements pass 1 of section 5.6, including whole-node requests over the hosts of all machine providers; the memory percent moves into the engine. The sub-steps already built (tail rule in `js-whole`, one pick, backfill gate, estimate feed, evaluation counters) stay. Fix rows 11, 17, 22 and 26 of section 8 here. *Needs: 6 (hosts arrive through the provider interface), 7 (the platform layer and the ordered machine providers), 2 (the queue is persisted).*
9. **Placement decides acquires (pass 2 of section 5.6).** Fold the scaler's demand logic into pass 2 of the walk: filter catalog offers by the request in the vocabulary of section 3.8, apply the pool's limits, pick by the policy of 5.6 step 5 (provider order, price, fewest accelerators, spare resources, purchase order, offer id, replacing the scaler's choice), fail over on a `capacity` refusal to the next zone, offer and provider, handle `quota` and the retry and maximum queue wait of 5.6; the scaler keeps caps, the brake and the records. *Needs: 7, 8, 26.*
10. **Estimate feed decision.** Persist the counters, collect evidence, decide the default (section 13, question 3). *Needs: 3, 8.*
11. **kbf as a client only.** In kbf's repository, replace the embedded server crates with the generated library; remove the dependencies; retire `js-server-core` in this repository if nothing else uses it. The service must give kbf what the embedded engine gave it: load placement, tail placement, whole-host leases, run submission. *Needs: 1, 8.*
12. **Wire `crun` networking.** Call `Egress::check`, `attach` and `release` around the container start; delete the refusal in `js-agent/src/lifecycle/mod.rs`, the "loopback only" text in the `js-agent` rustdoc and `crun/mod.rs`, and the stale sentences in `agent-egress.md`. *Needs: nothing.*
13. **Per-job network field.** Add `network` and `ports` to the container job in `js-agent-wire` and `js-types`, the agent ceilings as flags, the refusal above the ceiling, and the `open` level in `egress/config.rs` and `egress/ruleset.rs`. *Needs: 12.*
14. **DNAT forwards.** The forward goes in the NAT table (`ip js_crun_egress_nat`) and its single-destination accept rule in `inet js_crun_egress` (`egress/ruleset.rs`, `egress/host.rs`); `Egress::check` reads both back, and the root test in `tests/egress` (declared port reachable, undeclared refused, no inbound path without ports). Placement treats a declared host port as a constraint. *Needs: 13.*
15. **macOS mapping.** Add the `open` value to `--native-network`, map the job's level onto it as in section 5.8 (the `native` body carries the same `network` and `ports` until step 17 folds it in), accept `ports` and refuse it, and write the root test for inbound filtering by uid that would lift the refusal. *Needs: 13, 17.*
16. **Retire the Podman firewall.** Remove `cli_runtime/egress.rs`, `deploy/js-agent/egress-network.sh`, `--container-network` and `--container-egress-allow`. *Needs: 12, 13.*
17. **One job definition.** An `image` reference on both platforms; the macOS agent pulls the bundle by digest through `js-oci`; the `native` body is folded in; staging of inputs and collecting of outputs for native runs. *Needs: 1.*
18. **Measure launch.** Time from a start command received to the job process running on a reference node, for `crun` with and without network wiring, from the agent-egress CI path; publish the number. A `pasta` comparison only if the veth cost is a problem. *Needs: 12.*
19. **EC2 machine provider (GCP Compute Engine after it).** AWS EC2 (the recommendation of question 4) behind `CapacityProvider`, starting a VM that boots `js-agent`: its `catalog()` reads the instance types, their GPU information and the zones where each is offered, translates GPU names in code to the canonical names, adds price, purchase option and quota, and `acquire` launches the machine image the configuration selects for the offer's architecture with the agent build for that architecture. Check how the newest GPU types are actually purchasable (on-demand, spot, or a reservation or capacity block) and add that purchase option to `Offer` if needed. Add the Linux arm64 archive to the release (section 13, question 19), because the first accelerator offers are `arm64`. GPU jobs on these machines run on the fallback runtime until step 23. *Needs: 5, 7, 25, 26.*
20. **Agent wire.** Decide whether `hello` and heartbeat move from JSON polling to the protobuf toolchain of step 1. *Needs: 1.*
21. **Safety work that stands apart.** An agent self-fence on silence and a fencing token on a start (section 8, rows 5 and 6); adoption of `crun` runs after an agent restart (row 4); per-client tokens; a lease-server update hook; `--allow-host-jobs` with taint recording. *Needs: 4 for the update hook; the rest nothing.*
22. **`crun` is the default Linux runtime.** Make `crun` the default `--runtime`; the start check refuses a host that cannot run `crun` instead of using Docker; `--runtime docker` is accepted only with an explicit opt-in flag and then runs with `network = none` (section 5.7). *Needs: 12, 16.*
23. **GPUs for `crun`.** Pass GPU devices into a `crun` job, or state in section 2.2 that GPU jobs stay on the fallback. This gates dropping the fallback (section 13, question 7). *Needs: nothing.*

24. **Later: the other container platforms.** The non-Default kinds of section 3.7 (Kubernetes, Cloud Run, ECS) behind `ContainerPlatform`: `submit`, `observe`, `cancel`, `logs` and `result`, a conformance suite, the mapping of the vocabulary onto each platform's knobs, and capability flags that refuse impossible requests. One platform first (question 26). Not scheduled and not part of v1. *Needs: 7 and 8; question 28 first.*
25. **Capability vocabulary and alias table.** The structured accelerator (vendor, model, count, memory per device, optional interconnect) in `js_types::Requirements` and `js_elastic::Offer`, replacing `gpus` plus `GpuClass`; ISA level in `Requirements`; the agent reports the same structure; the alias table in `js-caps`, in code, the one place canonical names are defined (canonical model with variant facts, the agent's spelling, unknown names passing through; each provider crate holds its own source spellings in code), with a conformance test that every canonical name a provider emits is in the table; one arch spelling (`arm64`, `aarch64` accepted and normalised at parse) with a test that an `arch=arm64` request fits an `aarch64` offer; the comparators of section 3.8; `Offer::fit` matches on the canonical model. Today's full-spelling requests keep working. *Needs: nothing.*
26. **Catalog translation, cache and verify-on-hello.** Providers return offers in the vocabulary (Lambda Labs first: its GPU facts move from operator `[[option]]` rows into the crate's code, and the rows keep only the allow list of instance types); a per-provider catalog cache with a refresh interval and invalidation on `capacity` and `quota` refusals; an offer cool-down; the offer chosen for each acquire is stored (section 10.1); on hello the engine compares the agent's detected capabilities with the offer by the rule of 5.6.1 step 6 and, on a mismatch, releases the machine, marks the offer bad for `offer_cooldown` keyed by (provider, instance type, zone) and counts a boot failure toward the brake; a hello for a machine with no recorded offer is checked against the pool's shape floor only. *Needs: 5, 25.*
27. **Submit a job as one call; end leases on a lost host.** One call that takes a job, takes a lease of the job's shape, starts one run and releases the lease when the run exits, so a client that wants one job never handles a lease (section 3.2). The lease and run calls stay for clients that want warm capacity or a whole host. A lease whose host is lost ends as lost instead of expiring (section 8, row 29). A run's result stays readable after its lease is released (section 8, row 33). *Needs: 1, 4, 8.*

Order that follows from the dependencies: 1, 2, 5, 12 and 25 can start now. 3 and 4 follow 2. 6 and 26 follow 5 (26 also 25). 7 follows 5 and 6. 8 follows 2, 6 and 7, and is the largest. 9 follows 7, 8 and 26; 10 and 11 follow 8. 13 follows 12, and 14, 15, 16 follow 13. 17 and 20 follow 1. 18 follows 12. 19 follows 5, 7, 25 and 26 (its GPU jobs use the fallback runtime until 23). 22 follows 12 and 16; 23 stands alone, though GPU jobs on any machine wait for it; 24 is later, unscheduled, and follows 7 and 8. 27 follows 1, 4 and 8.

### 12.2 Other gaps, unchanged by this revision

**Whole-host reservations.** Built: the tail rule; registry reservation, drain, give-up, caps; wait list; persisted ledger; estimate feed; tail counters; backfill gate. In review: `request_id` on acquire, which makes the whole path reachable from HTTP. Remaining: a proof that queue order is respected (step 8); the restart test of section 8 row 10; deciding rows 26, 29 and 30 of section 8.

**Macs.** Built: native runtime; throwaway users; Seatbelt profile; packet-filter rule; teardown verdicts; isolation test; package and updater hook; disclaiming privacy-grant responsibility for every job process. Remaining: staging inputs and collecting outputs (step 17); the pool of reused users; adoption after restart; soft stop; a VM runtime with a network path; read model `allow`; notarized binaries; automated isolation test per release. MDM placement is out of scope here.

**Cross-cutting.** A readiness endpoint; a Kubernetes or other supervisor definition in the repository; a deny list of hosts; spot reclaim handling; a Linux arm64 archive; a VM runtime; a generic boot grace; a safe restore verb.

## 13. Open questions

Settled by the maintainers and therefore not listed: Postgres as the store, kbf as a client only, the platform model (a pool names container platforms in priority order: `Default`, then Kubernetes, Cloud Run and ECS later; Default takes an ordered list of machine providers, of which `static` is hosts we install `js-agent` on; self-registered `js-agent` hosts are the `static` kind, and the agentless pool is removed as a production kind (recommended, question 27); sections 3.5, 3.7), one capability vocabulary owned by the scheduler with provider-owned catalog translation and verify-on-hello (section 3.8, 5.6), interfaces and mappings in code with configuration only selecting and limiting, and accelerator aliases shipping in code so that a new accelerator needs a release (formerly question 29; operator alias rows were rejected), per-job networking with an agent ceiling, and `crun` as the default Linux runtime with a Docker fallback (section 5.7). Each question below needs an owner's decision before this document can state it as fact. Where there is a real choice, the options and a recommendation are given.

1. **One active writer, or high availability, on Postgres.**
   - *A. One active service holds an advisory lock; a person or a supervisor restarts it.* Simple; an outage lasts as long as the restart. Leases whose clients could not renew can expire during it (question 15).
   - *B. A hot standby takes the lock when the active service's session ends.* All durable state is in Postgres, so takeover needs no copy; agents re-hello and live state is rebuilt. More moving parts, and the whole-node and host states that are rebuilt from hellos reset on every takeover.
   - *C. Several active services.* Needs partitioning of pools or a distributed queue; not justified by the load.
   - **Recommendation: A first, designed so that B is a matter of running a second process.** Keep the lock the only coordination and keep nothing durable outside Postgres.
2. **Does the HTTP/JSON API survive beside gRPC?**
   - *A. gRPC only.* One surface; every client must speak it; curl and shell scripts need a tool.
   - *B. A gateway generated from annotations on the same definition, routes unchanged.* Compatible with today's clients, scripts and a browser, with no second source of truth.
   - *C. Keep the hand-written routes.* Two sources of truth.
   - **Recommendation: B.** It costs a generator step, and the previous release's client must keep working against it (a test).
3. **Is the estimate feed on by default, and what evidence in the tail counters would justify it?** Off by default today. Recommendation: persist the counters (step 3), then turn it on for a pool once its estimate error is bounded by the margin that backfill uses.
4. **Which cloud machine provider comes after Lambda Labs: EC2 or GCE first?** Both start a VM through the cloud's VM API that boots `js-agent` (step 19).
   - *A. AWS EC2:* the largest user base and a signed instance identity that removes the join token; the design (`providers.md`) already specifies its launch and identity rules.
   - *B. GCP Compute Engine.*
   - **Recommendation: AWS EC2**, because it exercises the most of the interface (signed identity, tags, spot reclaim events) and its rules are already written; GCE second.
5. **Does the agent need a real self-fence?** What does the design promise about the gap between a lease ending and the agent stopping a job? Today a job keeps running to its wall limit (section 8, rows 5, 6). Recommendation: a fence field on a start and an agent-side stop a bounded time after silence; decide the bound.
6. **Runs and agent restart.** Run records become durable in Postgres (step 4), but a `crun` container lives in the agent's runtime memory: after an agent restart the new process removes its predecessor's leases and lists nothing, and the run ends with no result. Options: *adopt* running containers by label as the container-engine path does, or *accept* the loss and have the client requeue. Recommendation: adopt, because a restart of the agent for an update drains first and a crash should not cost a long job.
7. **When is the Docker and Podman fallback dropped?** It is kept for now (section 5.7). *Drop* when `crun` carries what it lacks (GPUs, step 23; run adoption after restart, step 21) and every supported host meets `crun`'s requirements; *keep* longer if some host class cannot. Recommendation: drop it once steps 21 (adoption) and 23 land, so there is one runtime to reason about.
8. **Above the ceiling: refuse or lower?** A job that asks for more network than the agent allows could be refused (the client sees why) or run with less (it may fail later and confusingly). macOS lowers today. Recommendation: refuse on both, with a code that names the ceiling.
9. **Should the API listener refuse a public non-wildcard address unless an explicit flag is set?** Today only status pages are private-gated; keeping the API off the public internet is operator posture.
10. **What is the service's update procedure?** With state in Postgres a restart no longer drops runs, so the choice is drain to zero runs or restart in place. Today there is no update hook for the service and an update drops all runs. Recommendation: an update hook that restarts in place once runs are durable (step 4).
11. **Host-updater trust.** The declaration names this repository's release workflow as the signer, so whoever can push to main can ship code that runs as root on every host. Is that accepted, or must the client republish under its own signer, with branch protection first? The repository had no ruleset at the time of writing.
12. **Remove the agent drain cap of 3600 s, or keep it and document that it cuts work?** Section 6.4 states that drain never cuts work; a run that outlives the cap is stopped. Recommendation: no default cap, a configurable one, so a drain never fails a build.
13. **Idempotency scope.** Ids are single-tenant and the comparison excludes ttl. Is a per-client scope needed before a second token exists?
14. **History retention.** How long are ended-lease records and results kept in Postgres? A result is readable after release for that whole window (section 8, row 33). Recommendation: a configurable window, default seven days, for runs; ended-lease history kept as long as estimates need it.
15. **Generic boot grace.** Should a restart not expire leases whose clients could not renew during the outage? Today there is none (section 6.7, row 30).
16. **Should the agent offer a command that records a reimage?** A taint is permanent until reimage (G5, section 5.7). The open point is whether the server learns of the reimage from a new host credential only, or from an explicit command.
17. **Where is MDM owned?** The Mac device-management server is still in this repository; the host-management project is meant to own it. This document treats it as out of scope.
18. **Coverage rule.** Should this repository keep its zero-uncovered-lines ratchet (`coverage-baseline`), or use a percentage gate?
19. **Linux arm64.** Planned or dropped? The release has no archive for it, and the first accelerator offers we want to serve (GB300 is Grace-Blackwell) are `arm64`, so the Default platform cannot serve them without it (section 5.6). Recommendation: planned, as part of step 19.
20. **Does a pending whole-node lease with a `request_id` survive a rollback to an older build?** The in-review change says it is served as a lease without an id, with no re-presentation.
21. **When the registry ends a held lease** (failed prepare, voided ready), must the lease table drop it, keep it, or observe held leases every tick, and what does the client see? (row 26)
22. **What does a client learn when its host is lost, tainted or has lost its booking?** A field on renew, a state on a lease read, or lease end? (row 29)
23. **Should a flapping agent be able to postpone a whole-node prepare deadline,** or should the commit time survive a hello for a `Ready` host? (row 27)
24. **How rich is the allow-list?** Today it is the operator's list of IPv4 `IP:PORT` and `CIDR:PORT` entries, TCP only, at most 16. Options: add hostnames (needs a resolve-and-refresh loop), add UDP, let a job narrow the list. Recommendation: no change until a use case needs one; the object-store case is served by a listed endpoint.
25. **Quota check before a slow launch.** The platform-and-vocabulary design already takes from SkyPilot's provider layer: capability flags so an unsupported request is refused up front (step 5), failover over zones and providers on a `capacity` refusal (step 9), and a cached catalog (step 26). Section 5.6 states what a quota refusal does (the instance type is passed over for `quota_backoff`). Left open: a cheap quota check before a slow launch. Recommendation: decide with the EC2 provider (step 19).
26. **Which other container platform first, when they are built (step 24)?** Kubernetes is the most common and runs on customers' own clusters; Cloud Run and ECS are simpler to submit to and bill per run. The choice decides which network-policy mapping and which log and result path the first implementation has to carry. Not needed for v1; no recommendation until a user needs one.
27. **Does any deployment rely on the agentless static pool?** Step 6 removes `StaticPool` as a production kind (its kind name `static` is reused by the `js-agent` host provider, so an old configuration must be migrated), because a host without `js-agent` cannot be verified, tainted, drained or updated.
   - *A. Remove it; migrate any such host to run `js-agent` (it becomes a `static` machine provider host).*
   - *B. Keep it as a separate, unmanaged kind.* Two ways to own a host, and the agentless one has none of the safety rules.
   - **Recommendation: A**, after checking which projects list hosts in a pool file today; `js-lease-server`, `js-conformance` and `js-cli` use it in this repository.
28. **Does the Default platform's lease-on-a-host model share a trait and client surface with hostless platforms?** On Kubernetes, Cloud Run and ECS a lease can only be one run; on Default it is a host with runs inside it.
   - *A. One trait; a lease is a lease, and a hostless lease has no host field.*
   - *B. Keep the host lease for Default and add a hostless lease kind to the service definition when the first other platform is built.*
   - **Recommendation: B.** v1 is unaffected, and the service definition (step 1) can reserve the room.

## Related work

Several systems schedule work across heterogeneous compute. We found none that combines scheduling onto both Linux and macOS hosts, launching OCI images directly through a low-level runtime, and placing by learned per-job cost. The statements about other projects were checked against their public pages and source at the time of writing and may have changed. What follows separates what is built here from what is only designed.

- **SkyPilot** (https://github.com/skypilot-org/skypilot) places jobs on the cheapest feasible infrastructure across clouds, Kubernetes clusters and user-supplied machines, with failover. *History-based placement:* its optimizer prices a candidate from static per-cloud catalogs and a runtime that is either a user-supplied estimator or a default of one hour; we found no learned runtimes or per-job profiles in it. It keeps within-launch failover memory (a blocked-resources set) and tracks preemptions per location for spot services, which is hedging, not learned cost. Here, usage history fits a per-key cost curve (P90 cores, maximum working set, P90 wall time) that is charged at admission and feeds tail estimates (`js-usage`, `js-sizing`; partly built, section 3.4). *Provider interface:* SkyPilot's is broader and proven by use, with over twenty clouds behind a catalog and pricing class plus a provisioner module per cloud, a quota probe on two clouds, capability flags, failover loops and a plugin hook. It is dispatched by name and signalled by exceptions: we found no typed closed set of refusals, no idempotent-acquire contract and no conformance suite. Ours is stricter about correctness (idempotent acquire id, `Unknown` never launches or frees, read-back on release, tag-scoped `list`, section 3.5) and narrower in implementations: one real elastic provider is built, and the ordered hierarchy is designed, not built. Its machines you own run through a Kubernetes layer on Debian-family Linux hosts; we found no macOS execution hosts. Its pre-warmed sandbox pools for fast launch are the nearest goal to ours and the thing to measure against (step 18).
- **dstack** (https://github.com/dstackai/dstack) offers cloud and Kubernetes backends together with SSH fleets for existing machines.
- **HashiCorp Nomad** (https://github.com/hashicorp/nomad) is a general scheduler. Its `raw_exec` driver runs a task with no isolation under the agent's own account (https://developer.hashicorp.com/nomad/docs/deploy/task-driver/raw_exec) and is available on every operating system Nomad supports.
- **Orchard** (https://github.com/cirruslabs/orchard) orchestrates Tart virtual machines across Apple Silicon hosts: it schedules Mac virtual machines, not containers or native processes.
- **Kueue MultiKueue** (https://kueue.sigs.k8s.io/docs/concepts/multikueue) dispatches jobs from a manager cluster to worker clusters; every worker is a Kubernetes cluster.
- **crun** (https://github.com/containers/crun) is the OCI runtime this design drives directly on Linux.

Where the systems overlap with this design, the overlap is the provider model and failover, which we adopt in the same spirit. SkyPilot spans both of our layers: its cloud backends provision VMs, like the machine providers of our Default platform, and its Kubernetes backend submits to a cluster, like our later Kubernetes platform kind (section 3.7); Kueue MultiKueue is container-platform orchestration only. Our Default platform uses a cloud solely to start a VM that boots `js-agent`, and places and launches every job itself. The differences are the learned cost model, the macOS execution path, and whole-host requests placed by remaining drain time. All of these are designs until the steps of section 12.1 land; section 3.1 says which parts are built.

## 14. Glossary

- **Acquire (lease).** `POST /v1/leases`. Asks for a host.
- **Acquire (provider).** A provider's call that launches a machine for a pool.
- **Agent.** `js-agent`, the process on every host, whatever provider supplied it.
- **Accelerator.** In the vocabulary: `{ vendor, model, count, memory per device, interconnect }`, with `model` the canonical lower-case name such as `nvidia-gb300` (section 3.8).
- **Alias table.** The map from every spelling of an accelerator to its canonical model and variant facts. The canonical names and the agent's spellings are code in `js-caps`; each provider crate holds its own cloud's spellings in code (section 3.8). No configuration defines an alias.
- **Allow-list.** A job network level: the endpoints and resolvers the agent is configured with and nothing else.
- **Awaiting.** A host seated from the store after restart, not yet heard.
- **Backfill.** Short work placed on a reserved host because it will end inside the tail.
- **Booking.** The record that a lease owns part or all of a host.
- **Bundle.** The macOS form of a job image: an OCI image manifest of one platform whose layers are directory trees (`js-bundle`).
- **Capable.** A host that could serve a request once it is free.
- **CapacityProvider.** The machine-provider interface (section 3.5).
- **Ceiling.** The most network a job may ask for; set by the agent's flags.
- **Capability vocabulary.** The one set of terms for what a request needs and what a host, offer or platform has (section 3.8).
- **Catalog.** A machine provider's list of offers, already in the capability vocabulary (`catalog()`).
- **Class.** What a lease asks for: a share class (part of a host) or a whole-node class.
- **Commit before start.** The pending record is committed before the provider is called.
- **Cordon.** A host takes no new lease; its current work continues.
- **Container platform.** What runs a job's OCI image; a pool names them in priority order. Kinds: `Default` (ours, v1), then Kubernetes, Cloud Run and ECS (section 3.7). Each has its own typed parameters and publishes capability flags.
- **Default (platform).** Our container platform: `js-agent` with `crun` on Linux and native runs on macOS, with our own placement. It takes an ordered list of machine providers. The one platform v1 builds.
- **Drain.** Waiting for a marked host to become quiet. Drain never kills work.
- **Elastic provider.** A machine provider that launches and terminates machines through a cloud's VM API (a dynamic VM provider).
- **Epoch.** Counter of an agent's sessions; stale epochs are refused.
- **Exclusive.** The whole-node state of a host (committed, ready, restoring).
- **Grace.** Silence allowed before a host is lost (agent); delay before a presentation is dropped (`regain_grace`).
- **Held.** A lease whose provider confirmed it (an active lease, section 3.2).
- **Hierarchy.** The order of a pool's platforms, and of a Default platform's machine providers.
- **Join token.** One-time credential for a first hello.
- **Lease.** A client's hold on capacity of a given shape, or on a whole host, on one host, for a time, kept alive by renewal and capped by a wall time. Requested, placed, active, then ended as released, expired or lost (section 3.2).
- **Job.** What a run executes: an OCI image pinned by digest, a command, environment, limits, optional files and a network policy (section 5.7).
- **Machine provider.** What supplies a Default platform's machines, every one of which runs `js-agent`: `static` (hosts we install `js-agent` on, which register), or a dynamic VM provider (Lambda Labs; AWS EC2 and GCP Compute Engine planned). It never runs a job. The `CapacityProvider` trait is its interface (section 3.5).
- **Mark.** A reservation of a host for a waiting request.
- **Cool-down.** The time an offer that failed verify-on-hello, or an instance type refused for quota, is not picked again.
- **Offer.** What one launch of a machine provider would give: shape, accelerator, os, arch, zone, price and availability (`js-elastic/src/offer.rs`).
- **Open.** A job network level: any address except what the agent always denies.
- **Pending.** A lease recorded but not confirmed.
- **Placement engine.** `js-sched`, the one place a placement is decided.
- **Probe.** A lease pinned to one named host, even a cordoned one.
- **Provider.** Short for machine provider.
- **Quiet.** A host with no leases and no runs.
- **Registry.** `js-registry`, today's host book. The design has no registry: host facts are scheduler state in Postgres (section 3).
- **Run.** One execution of a job inside a lease: queued, started, then exited, stopped or lost. A run never outlives its lease (section 3.2).
- **Static (machine provider).** Hosts we install `js-agent` on, which reach out and register; bare-metal Linux and macOS. It never acquires.
- **Tail.** The latest estimated end among a host's leases.
- **Taint.** A permanent mark that a host failed a post-job check.
- **Tick.** The loop step where time acts.
- **Unknown.** The provider's answer when it cannot see; never frees anything.
- **Whole-node.** A lease that owns an entire host and is held only when the agent reports it ready.
