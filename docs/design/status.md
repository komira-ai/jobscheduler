# Design status: what main builds

Every page under `docs/design` that says Designed, not yet, Planned, Partly built, Open, or lists a build step, with each such claim checked
against the code on `main` as of komira-ai/jobscheduler#1106. Each claim was read against the crate that would hold it, not matched by keyword.
The page records state; it builds nothing. A pull request that builds a row, or turns a page's tag from Designed to Built, updates the row.

Columns:

- **On main**: the file (and line) that builds it; `partly:` with the file of the built part; `no`; `superseded` when a later page or a merged
  pull request replaced or dropped it; `decision, no code` for an open question that waits on a decision, not on code; `not a gap:` with
  the reason, for a claim the check found already settled.
- **Tracking**: the issue or pull request that tracks the work. `(new)` marks an issue filed with this page for a gap that had none. `-` means
  nothing tracks it, which for a `no` row means the page's text says the work is deferred on purpose, kbf's, or a measurement, not code.

## Open pull requests the pages depend on

- komira-ai/jobscheduler#873 (partly on main): main's `docs/design/multi-instance.md` (stateless instances on Postgres, komira-ai/jobscheduler#926 and later) is newer than the pull request's leaderless design and supersedes it; most of its mechanisms (presence rows, dispatch queue, the agent's dispatch poll) are built. Not on main: the pull request's `docs/design/multi-instance-review.md` (its review notes and open questions on a Mac LAN listener route and heartbeat ordering).
- komira-ai/jobscheduler#868 (not on main): none of it is on main. `crates/js-contain/src/oci/hardening.rs` still gives a job no capabilities, `crates/js-contain/src/oci/user.rs` does not exist, and the js-agent image-user changes, the golden and the design text are absent.

## Gaps not filed here

- Multi-instance G3 (host identity is the host certificate; join tokens, pool tokens and host credentials go) is in progress: komira-ai/jobscheduler#1118 is its first step.
- The whole-node start-order tests still missing (W2 and W10: work ahead of a reserved request, a lower-QoS request behind a stream): komira-ai/jobscheduler#1133. The heartbeat's disk_gib: komira-ai/jobscheduler#328. The Seatbelt fence for TCC-only places: komira-ai/jobscheduler#1025, komira-ai/jobscheduler#1020 and komira-ai/jobscheduler#1117. Linux jobs on a Mac: komira-ai/jobscheduler#1066.
- The per-consumer whole-node cap is not a gap: `docs/design/jobscheduler-design.md` says there is no per-consumer cap in the single-token service, and `whole-node.md` section 6 is stale (komira-ai/jobscheduler#1193).
- Stale status lines and comments found on the way: komira-ai/jobscheduler#1193.

## Pages

| Page | Rows | Built | Partly | Not built | New issues |
|---|---|---|---|---|---|
| multi-instance.md | 29 | 3 | 6 | 16 | komira-ai/jobscheduler#1136, komira-ai/jobscheduler#1137, komira-ai/jobscheduler#1138, komira-ai/jobscheduler#1139, komira-ai/jobscheduler#1140, komira-ai/jobscheduler#1141, komira-ai/jobscheduler#1142 |
| placement-profiles-live.md | 26 | 10 | 6 | 3 | komira-ai/jobscheduler#1143, komira-ai/jobscheduler#1144, komira-ai/jobscheduler#1193 |
| start-latency.md | 14 | 2 | 2 | 4 | komira-ai/jobscheduler#1145, komira-ai/jobscheduler#1146, komira-ai/jobscheduler#1147 |
| whole-node.md | 27 | 13 | 4 | 3 | komira-ai/jobscheduler#1148, komira-ai/jobscheduler#1149, komira-ai/jobscheduler#1150, komira-ai/jobscheduler#1193 |
| lease-limits.md | 5 | 1 | 2 | 1 | komira-ai/jobscheduler#1151 |
| held-beat.md | 0 | 0 | 0 | 0 | - |
| rollout.md | 21 | 5 | 4 | 3 | - |
| risks-f.md | 16 | 2 | 5 | 3 | komira-ai/jobscheduler#1150, komira-ai/jobscheduler#1152, komira-ai/jobscheduler#1172, komira-ai/jobscheduler#1174 |
| merged-code.md | 34 | 15 | 5 | 3 | komira-ai/jobscheduler#1168, komira-ai/jobscheduler#1169, komira-ai/jobscheduler#1170, komira-ai/jobscheduler#1171 |
| jobscheduler-design.md | 82 | 38 | 20 | 7 | komira-ai/jobscheduler#1153, komira-ai/jobscheduler#1154, komira-ai/jobscheduler#1155, komira-ai/jobscheduler#1156, komira-ai/jobscheduler#1157, komira-ai/jobscheduler#1158, komira-ai/jobscheduler#1174 |
| pools-providers-network.md | 30 | 6 | 8 | 10 | - |
| job-rootfs.md | 17 | 3 | 2 | 9 | komira-ai/jobscheduler#1159 |
| job-retry-and-completion-callback.md | 17 | 12 | 0 | 1 | komira-ai/jobscheduler#1150, komira-ai/jobscheduler#1193 |
| cpu-over-time.md | 24 | 9 | 6 | 6 | komira-ai/jobscheduler#1143, komira-ai/jobscheduler#1144 |
| live-logs.md | 11 | 5 | 0 | 4 | - |
| mac-account-per-job.md | 17 | 6 | 6 | 1 | komira-ai/jobscheduler#1163 |
| mac-one-program.md | 45 | 5 | 6 | 21 | komira-ai/jobscheduler#1160, komira-ai/jobscheduler#1161, komira-ai/jobscheduler#1162, komira-ai/jobscheduler#1163, komira-ai/jobscheduler#1164, komira-ai/jobscheduler#1165, komira-ai/jobscheduler#1166, komira-ai/jobscheduler#1193 |
| kbf-on-js-agent.md | 51 | 16 | 14 | 10 | - |
| extraction-steps.md | 32 | 10 | 12 | 0 | - |
| extraction-steps-d.md | 49 | 17 | 11 | 5 | komira-ai/jobscheduler#1150, komira-ai/jobscheduler#1167, komira-ai/jobscheduler#1168, komira-ai/jobscheduler#1169, komira-ai/jobscheduler#1170, komira-ai/jobscheduler#1172, komira-ai/jobscheduler#1173, komira-ai/jobscheduler#1174, komira-ai/jobscheduler#1175 |
| agent-wire.md | 16 | 3 | 2 | 5 | komira-ai/jobscheduler#1146, komira-ai/jobscheduler#1156, komira-ai/jobscheduler#1177, komira-ai/jobscheduler#1193 |
| configuration.md | 4 | 0 | 0 | 3 | komira-ai/jobscheduler#1153 |
| deployment.md | 13 | 5 | 3 | 3 | - |
| hosts.md | 41 | 2 | 12 | 17 | komira-ai/jobscheduler#1150, komira-ai/jobscheduler#1173, komira-ai/jobscheduler#1174, komira-ai/jobscheduler#1175, komira-ai/jobscheduler#1176 |
| placement-by-load.md | 25 | 6 | 6 | 4 | komira-ai/jobscheduler#1178, komira-ai/jobscheduler#1179, komira-ai/jobscheduler#1180, komira-ai/jobscheduler#1181, komira-ai/jobscheduler#1182 |
| mac-lease-teardown.md | 5 | 1 | 1 | 1 | - |
| decisions.md | 22 | 6 | 4 | 7 | komira-ai/jobscheduler#1150, komira-ai/jobscheduler#1174, komira-ai/jobscheduler#1175 |
| amendments.md | 9 | 6 | 1 | 1 | - |
| capabilities.md | 26 | 3 | 4 | 15 | komira-ai/jobscheduler#1168, komira-ai/jobscheduler#1169, komira-ai/jobscheduler#1170, komira-ai/jobscheduler#1171, komira-ai/jobscheduler#1173, komira-ai/jobscheduler#1189 |
| kbf-today.md | 15 | 3 | 1 | 3 | - |
| machines.md | 1 | 0 | 0 | 1 | - |
| mac-runtime.md | 18 | 4 | 5 | 7 | komira-ai/jobscheduler#1183 |
| sizing.md | 13 | 0 | 2 | 3 | komira-ai/jobscheduler#1184 |
| retries-and-runs.md | 5 | 2 | 1 | 1 | komira-ai/jobscheduler#1150 |
| vm-backend.md | 17 | 2 | 4 | 8 | komira-ai/jobscheduler#1155, komira-ai/jobscheduler#1185, komira-ai/jobscheduler#1186, komira-ai/jobscheduler#1187, komira-ai/jobscheduler#1188, komira-ai/jobscheduler#1189 |
| placement.md | 7 | 3 | 1 | 1 | - |
| tree-inputs.md + estimate-feed.md + ucdk.md | 16 | 3 | 0 | 0 | - |
| 14 other pages | 27 | 9 | 8 | 8 | komira-ai/jobscheduler#1167, komira-ai/jobscheduler#1190, komira-ai/jobscheduler#1191, komira-ai/jobscheduler#1192 |

## multi-instance.md

As read: Status: design, living document. Most gaps are Built (G2, G4 mostly, G5 mostly, G7, G12, G15, G19 server and agent). Still Designed: G3 removal, G5 leftovers, G6, G9, G11, G16.

| Section | Claim | On main | Tracking |
|---|---|---|---|
| G3 (s9, s3.2, s3.6, s6, s7, s12) | Host identity is the host certificate: delete join tokens, pool tokens (--agent-token-file), host credentials and the scaler's token mint/revoke; refuse a request with no certificate; agent token and credential flags go | partly: `crates/js-lease-server/src/host_cert.rs:86`, `crates/js-lease-server/src/join_tokens.rs` | G3 is in progress; komira-ai/jobscheduler#1118 is its first step |
| G3 (s3.6 token times) | Join-token and credential expiry times use the process clock; G3 removes them | no | goes with G3 |
| G2 (s9, s4) | One clock: store's now() for every shared deadline; Store::now fallible | `crates/js-state/src/store.rs:121`, `crates/js-lease-server/src/job_pass.rs:200` | komira-ai/jobscheduler#938 |
| G7 (s4, s9) mark claim_by | The whole-host mark's claim_by becomes a stored deadline on the store clock (new optional field in `mark/<job>`) | no | komira-ai/jobscheduler#1138 (new) |
| G4/G7 (s3.3 whole-host start taint) | The whole-host start reads the host's taint from rows, not from this instance's registry | no | komira-ai/jobscheduler#1138 (new) |
| G4 (s9, s3.2) after-G3 clean-up | After G3: HostBound, the binding's credential and its cordon copy go; the row keeps only the taint; save_bindings goes | no | goes with G3 |
| G5 (s3.5, s5, s9) host_silent_since_ms | A job's host_silent_since_ms reads the node/ rows, not this instance's registry | no | komira-ai/jobscheduler#1136 (new) |
| G5 (s3.3, s5) HostLive in the row's transaction | HostLive is judged in the node row's own transaction, not only on the registry refreshed before the hello | no | komira-ai/jobscheduler#1136 (new) |
| G15 (s6 row 4, s0) | Same agent process takes the host over at once via an optional hello field | `crates/js-agent-wire/src/hello.rs:349` | komira-ai/jobscheduler#942; komira-ai/jobscheduler#948 |
| G15 (s0 cost) measure the two forms | G15 chooses between node/ through the Store and a plain node table by measuring both at the deployment's host count; s4 says the write is measured in G15's PR | decision, no code | - |
| G6 (s3.6, s5, s9, s12 order step 4) | Settlement by node rows: the claimed job pass loses a host's allocations by node/ heard_at and version; the local gone map and the claim bypass go | no | komira-ai/jobscheduler#1137 (new) |
| G9 (s3.6, s3.5 brake, s9) | The scaler under the rule: one step per pool per period by duty claim; records at row version; each launch a row at Cond::Absent before the provider call; caps/budgets in the transaction; brake a row; release via hostmark; launched machine joins with a certificate | no | komira-ai/jobscheduler#1139 (new) |
| G11 (s3.7, s3.1, s9) usage rows per instance | A host-minute flushed by two instances keeps both: per-instance minute rows beside the hour pack, reader sums | no | komira-ai/jobscheduler#1140 (new) |
| G11 (s3.7) usage prune | Usage prune claimed by periodic/usage-prune | no | komira-ai/jobscheduler#1140 (new) |
| G11 (s3.7) profile eviction | Profile curves eviction at the scanned version | no | komira-ai/jobscheduler#1140 (new) |
| G11 (s3.1) import loser starts | The first-start import's loser reads the marker and starts instead of exiting 2 | no | komira-ai/jobscheduler#1140 (new) |
| G11 (s9) stale doc | Fix the stale 'one writer' doc in js-usage/src/store.rs | no | komira-ai/jobscheduler#1140 (new) |
| G12 (s6, s9, s11) | Job-store cutover tools: a dry-run copy of `<state-file>.jobs` into the store with counts and checksum per prefix, and a Postgres-to-SQLite export of job and usage rows, round-trip test | `crates/js-lease-server/src/cutover.rs:161` (`js-lease-server job-store copy`, a dry run unless `--write`), `crates/js-lease-server/src/cutover.rs:191` (`job-store export`), `crates/js-lease-server/src/cutover/rows.rs:178` (counts and SHA-256 per prefix); round trip `crates/js-lease-server/src/cutover_tests.rs:496`, through Postgres `:526`; runbook `docs/runbooks/state-database.md` section 6 | komira-ai/jobscheduler#1141 |
| G16 (s0, s3.3 held beats, s9, s12 order step 4) | LISTEN/NOTIFY wakes placement (queue channel) and the host's dispatch poll (host channel); relisten wakes every waiter once | no | komira-ai/jobscheduler#1142 (new) |
| G19 (s0.2, s9) wake on NOTIFY | A held dispatch poll wakes on the host's NOTIFY from another instance | no | komira-ai/jobscheduler#1142 (new) |
| G19 (s0.2, s9) latency measurement | Measure start latency placement-to-ack, p50 and p90, with and without the poll, on the deployment, and report both in the PR | no | - |
| G19 (s0.2, s9, s0.1) rest of G19 | Dispatch queue, route, claim, ack, drain, hello capability, agent poller | yes: js-jobs/src/store/dispatch.rs:183,242; js-lease-server/src/agent_api/dispatch.rs:97,157,248; agent_api.rs:341; js-agent/src/dispatch/ (poller.rs, serve.rs), js-agent/src/contract.rs; js-agent/src/lifecycle/mod.rs:12-25. | komira-ai/jobscheduler#944; komira-ai/jobscheduler#949; komira-ai/jobscheduler#1067; komira-ai/jobscheduler#1080 |
| s0.1 version-skew tests | Tests: the release before hands out a message the new placer wrote, and the agent starts it; a job placed by the release before reaches a polling agent through the beat's reply | partly: `crates/js-lease-server/src/per_job.rs:181`, `crates/js-agent/tests/agent/skew/record.rs` | - |
| s12 hazard table (live hazards) | Hazards: admission before hellos, binding save, taint beside host job, taint clear, host on two instances, token 401, pool cap, ListHosts, lost whole-host start, minute of usage, drain | partly: Closed ones: pool cap nodes.rs pool_admits, ListHosts hosts.rs:352, taint placement host_rows.rs:80, drain drain.rs:28 | komira-ai/jobscheduler#1136 (new); komira-ai/jobscheduler#1137 (new); komira-ai/jobscheduler#1138 (new); komira-ai/jobscheduler#1139 (new); komira-ai/jobscheduler#1140 (new); komira-ai/jobscheduler#1141 (new); komira-ai/jobscheduler#1142 (new) |
| s9 pull request list: order of safety (s12) | Order: G2, G3 before G15; G15, G4, G5, G7, G19, G11 minute fold close the live hazards; then G6, G9, G16, rest of G11 (G12 built) | partly: Done: G2, G15 (komira-ai/jobscheduler#942, komira-ai/jobscheduler#948), G7 (komira-ai/jobscheduler#946), G5 mostly, G19 (komira-ai/jobscheduler#944, komira-ai/jobscheduler#949), G4 mostly, G12 (komira-ai/jobscheduler#1141) | komira-ai/jobscheduler#1136 (new); komira-ai/jobscheduler#1137 (new); komira-ai/jobscheduler#1138 (new); komira-ai/jobscheduler#1139 (new); komira-ai/jobscheduler#1140 (new); komira-ai/jobscheduler#1141 (new); komira-ai/jobscheduler#1142 (new) |
| s9 G1, G8, G10, G13, G14, G17, G18 | Gaps shrunk into G15 or deleted | superseded: The page itself strikes these out in s9 (lines 515-521). | komira-ai/jobscheduler#883 |
| s10 supersession pointers | deployment.md gets a pointer here; jobscheduler-design.md s10.1 amended by G15; configuration.md 'What it keeps' changed by G3 and G4 | partly: `docs/design/deployment.md:146`, `docs/design/configuration.md:15` | - |
| s11 decisions | G12 is built; G3 removes tokens instead of making them N-safe; settlement needs heard_at; PoolFull takes a pool row lock | partly: heard_at: js-jobs/src/store/nodes.rs:267-284 (forget_silent_nodes uses it); PoolFull: nodes.rs pool_admits and keys.rs:38-42 pool/. G3 not built (see rows above); G12 built (see its row). | - |
| Page header 'Status: design' and s2/s3 tag claims | The page is a living doc: each PR turns Designed to Built and names code | yes: Most rows carry Built with paths; spot checks matched (dispatch.rs:97, nodes.rs, keys.rs:36, forget_silent_nodes). | - |

## placement-profiles-live.md

As read: Status: built behind --placement-curves on (off by default); header says the memory shadow of section 2 is built under shadow and on, and only the decide-in-shadow line of section 4 for CPU is not built. Still accurate on main.

| Section | Claim | On main | Tracking |
|---|---|---|---|
| Status header / §4 Farm-wide step 2 / §5 Stages step 2 | Decide in shadow: under shadow the service also runs the curve packing rule and logs 'a job's curve booking would place it differently' (job, class, flat_host, curve_host, slot) for two working days before on. | no | komira-ai/jobscheduler#1143 (new) |
| §2 Memory over time, in shadow (the memory half of decide-in-shadow) | Under shadow and on, each host's memory is projected from its jobs' memory curves (slot p90 with the flat booking's margin, capped at flat; flat until 16 scored runs); each placement counts same, sooner or later and logs the last two; each reading is compared with the last walk's projection; /metrics totals them. Books nothing. | `crates/js-sched-placer/src/memory_shadow.rs`, `crates/js-sizing/src/memory_curve.rs`, `crates/js-lease-server/src/metrics/render.rs` | komira-ai/jobscheduler#1143 |
| §4 Per class: four checks (checks 3 and 4 need new per-class state in js-sizing) | Checks 3 (peak overrun ring) and 4 (series usable ring) are computed at each pass. | `crates/js-sched-placer/src/sizes_profile.rs:12`, `crates/js-cpu-replay/src/shadow.rs:17-22` | komira-ai/jobscheduler#1087 |
| §4 'shadow gains three columns' | js-cpu-replay shadow prints CPU peak over %, CPU series usable %, CPU curve. | `crates/js-cpu-replay/src/shadow.rs:322-323` | komira-ai/jobscheduler#1087 |
| §4 Farm-wide step 1: a day of check lines | At least 24 h of check lines from every server task before on; the one measured window was 4 h 10 min. | decision, no code | - |
| §4 Farm-wide step 3: the replay on held-out days | The replay must show no loss against the flat step, throughput within 2%, and fewer core-seconds past cores. The trace must carry each job's series; today it has only mean and peak and the replay synthesises shapes. | partly: `crates/js-cpu-replay/src/sim/curve.rs:1-8`, `crates/js-cpu-replay/src/trace.rs:52-86` | komira-ai/jobscheduler#1144 (new) |
| §4 Memory: for a later memory step (question 4) | Booking memory below today's figure needs a per-class 64-run check and a host-OOM signal on /metrics. The memory shadow measures how often a projection is too low; it books nothing. | no | - |
| §5 Switch: placement_curves gains on; server.curve_hosts; server.curve_max_wait_seconds (0-300, default 30); refusal text | The on value, the curve_hosts canary and the wait bound are read once at start, from the file keys or from the flags `--curve-hosts` and `--curve-max-wait-seconds`, by one code path. | `crates/js-lease-server/src/config/placement_curves.rs:68-120`, `crates/js-lease-server/src/file/server.rs:568-579` | komira-ai/jobscheduler#1087; komira-ai/jobscheduler#1250 |
| §5 Stages 1 to 4 (flat step; checks and decide-in-shadow; replay; on with curve_hosts one host, half, all) | Staged rollout: flat step, checks and decide-in-shadow, replay, then on by canary. | partly: Flat step: komira-ai/jobscheduler#1085 merged (mergedprs.tsv) | komira-ai/jobscheduler#1085; komira-ai/jobscheduler#1087 |
| §5 Stages 5 / §8 Q4: memory is a separate decision | Memory booking from the profile is not part of these stages. | decision, no code | - |
| §5 Metrics | cpu-curve reason on js_placement_refusals_total, js_curve_wait_expiries_total, js_curve_classes{state}, js_host_cpu_booked_peak_millicores{host}; under shadow and on, js_memory_curve_placements_total{verdict}, js_memory_curve_sooner_milliseconds_total and the js_host_memory_projected/measured/projection_* families. | `crates/js-lease-server/src/metrics/render.rs:201` | komira-ai/jobscheduler#1087 |
| §6 What the build pull requests test | The listed minimum tests (flat equals flat step, bursts apart, clamp, survival, tail, residual, past the flat sum, each check boundary, wait bound 30 s, pack share, canary, settings, shadow table). | partly: `crates/js-sched-placer/src/curve_room_tests.rs:333`, `crates/js-lease-server/src/config/placement_curves.rs:163`, the canary through the flags `crates/js-lease-server/src/config/placement_curves_canary_tests.rs:160` | komira-ai/jobscheduler#1087 |
| §7 Stale agent doc | crates/js-agent/src/lifecycle/machine/mod.rs:500-502 still says memory is held by the lease's memory.max, which contradicts the code since komira-ai/jobscheduler#1027. Needs a one-line doc fix. | no | komira-ai/jobscheduler#1193 (new) |
| §7 Pack-limit default conflict (komira-ai/jobscheduler#1076 message says 0.8) | The commit message differs from the code (0.5); the doc uses 0.5. | decision, no code | - |
| §7 MAX_PROFILES (and §8 Q10) | MAX_PROFILES is 2048 against 5,335 classes in 4 h; eviction may keep the wrong classes (INFERRED). Question 10 says do not raise it now. | decision, no code | - |
| §8 Q1 margin per job on flat number | Recommend none. | decision, no code | komira-ai/jobscheduler#1087 |
| §8 Q2 rule 1 replaces the flat CPU sum on a curve host | On a curve host the slot check replaces the summed flat CPU check; past the flat sum is allowed. | `crates/js-sched-placer/src/curve_room_tests.rs:333` | komira-ai/jobscheduler#1087 |
| §8 Q3 slot books the 90th percentile at that age, held to a quarter of the host | The curve slot is the p90 clamped to a quarter of the host. | `crates/js-sched-placer/src/curve_room.rs` | komira-ai/jobscheduler#1087 |
| §8 Q4 memory from the profile | Not now; revisit only if three conditions hold. | decision, no code | - |
| §8 Q5 four checks and thresholds | 16 runs, 80%, 3 of 16, 12 of 16 are starting values to be set from the decide-in-shadow days. | partly: `crates/js-cpu-replay/src/shadow.rs` | - |
| §8 Q6 canary by named hosts | server.curve_hosts, a list of host ids (flag `--curve-hosts`). | `crates/js-lease-server/src/config/placement_curves.rs:77`, `crates/js-lease-server/src/file/server.rs:568-579`, `crates/js-sched-placer/src/curves.rs:3` | komira-ai/jobscheduler#1087; komira-ai/jobscheduler#1250 |
| §8 Q7 wait bound 30 s then flat | A curve-class job that waited 30 s is tested flat. | `crates/js-sched-placer/src/curve_room.rs:445`, `crates/js-lease-server/src/config/placement_curves.rs:100` | komira-ai/jobscheduler#1087 |
| §8 Q8 pack limit's CPU share is the fullest booked slot | The pack share reads the fullest booked slot. | `crates/js-sched-placer/src/curve_room.rs:86`, `crates/js-sched-placer/src/curve_room_tests.rs:874` | komira-ai/jobscheduler#1087 |
| §8 Q9 gate on on no loss against the flat step, not 10% against today | The gate is the replay against the flat step on held-out days with series. | partly: `crates/js-cpu-replay/src/sim/curve.rs` | - |
| §8 Q10 raise MAX_PROFILES | Not now; raise only if evicted classes with 16 runs show up. | decision, no code | - |
| Header: cpu-over-time.md steps 3 and 4 | Steps 3 (decide in shadow) and 4 (enforce) are fixed by this page. | partly: `docs/design/cpu-over-time.md:3` | komira-ai/jobscheduler#1087 |

## start-latency.md

As read: Six-part note; each part's code changes are built on main; each part ends with "What is left" / "Still to measure" lists whose farm-measurement items are operational and four items are code gaps. No header status line; no superseding page found.

| Section | Claim | On main | Tracking |
|---|---|---|---|
| Part 1 What is left #1 | The beat after a start ends when the container runs (done in second part). | `crates/js-agent/src/session.rs:534` | komira-ai/jobscheduler#941 |
| Part 1 What is left #2 | A placement on another server process reaches the host only on its next heartbeat, up to one hold (5 s) later; the wakes are in-process. | partly: `crates/js-lease-server/src/held_beat.rs` | komira-ai/jobscheduler#903 |
| Part 1 What is left #3 | Farm measurement: run a cold build on the farm with this agent and server and print js-start-timeline's table over each node's journal. | decision, no code | - |
| p99 tail: what it may be waiting on | Which of three candidates (provider Mount on cold store, worker held by image budget turn, step interval/heartbeat) causes the p99 is 'Not yet proved'; the new line (provided, placed_at_ms) is to tell them apart. | partly: The line is built: placed_at_ms in JobStartSpec, received_at_ms and provided step in the Started line, js-analysis js-start-timeline | - |
| Part 1 Decisions: Prepare then Mount stays two calls | Prepare then Mount stays two calls (decision, not a gap). | `crates/js-agent/src/lifecycle/machine/providing.rs:198-218` | - |
| Part 2 Still to measure | The farm table after the second-part agent ships (p50/p90 per segment on the slow node and a fast one). | decision, no code | - |
| Part 3 What is left #1 | The farm table after the agent and server ship, on a busy node and a normal-load node. | decision, no code | komira-ai/jobscheduler#941 |
| Part 3 What is left #2 | Mount sent by the provider client as soon as Prepare is answered, saving one worker step per source. | no | komira-ai/jobscheduler#1145 (new) |
| Part 3 What is left #3 | The connection of each heartbeat: a new TCP and TLS handshake every beat (js-client). | no | komira-ai/jobscheduler#1146 (new) |
| Part 3 What is left #4 | The p99: the integrity check's settle wait (1 s, on the worker, after a job that left a process or mount behind) and the image budget's first walk after a restart, both on threads a start waits behind. | no | komira-ai/jobscheduler#1147 (new) |
| Part 4 What is left #1 | The farm measurement after this agent ships: received->taken p50 in a same-size burst, 'job worker is busy' counts, Cleaned lines' worker_ms against steps. | decision, no code | - |
| Part 4 What is left #2 | The post-job checks and the owed record's syncs still run on the worker, once for each job. | not a gap: Part 4 (docs/design/start-latency.md, lines 375-377) records a decision: "The owed record, the checks and the quota stay on the worker" | - |
| Part 5 What is left | The farm measurement after this server ships: time from container running to server job state running, from job store records against Started lines. | decision, no code | komira-ai/jobscheduler#1067 |
| Part 6 What is left | After deploy: queue_ms of Started lines, target p90 under 20 ms on every node. | decision, no code | - |

## whole-node.md

As read: No status header and no "Designed / not yet / Open question" tags. Reads as live design with italic notes marking what was deleted or built; the unbuilt claims sit in prose and in section 12's step list.

| Section | Claim | On main | Tracking |
|---|---|---|---|
| §1 Admission (Whole) | Admission::Whole books a whole host with reservation, quiet gate, caps (steps 46, 56, 57). | `crates/js-lease-server/src/agent_pool.rs:253`, `crates/js-lease-server/src/file.rs:850` | - |
| §1 Admission | ucdk Job classes stop using BookAll and book their declared size (step 59). | superseded: `crates/js-jobs` | - |
| §1 gpu-host alias | gpu-host gains the reservation, quiet gate and caps when steps 46, 56 and 57 land. | no | komira-ai/jobscheduler#1148 (new) |
| §1 / §12 step 54 | GPU machines register into a whole-node pool. | partly: The mechanism is built: class whole-node and js-agent registration (agent_pool.rs:253, js-agent). Which machines run in which pool is an operator deploy, not repo code. | - |
| §3 Queueing/reservation | Reserve the cheapest-to-drain host (latest end, fewest leases, lowest id) and keep the mark. 'Does not switch to a cheaper one later (no thrash).' | `crates/js-whole/src/cost.rs`, `crates/js-sched/src/scheduler/whole.rs:5-20`; without whole-node settings the least tail, one hold per pool: `crates/js-sched/src/scheduler.rs` (`Scheduler::place`) | komira-ai/jobscheduler#582; komira-ai/jobscheduler#1127; komira-ai/jobscheduler#1129 |
| §3 Queueing / drain_after | drain_after (default zero) delays reservation. | `crates/js-sched/src/scheduler/whole.rs`, `crates/js-lease-server/src/agent_pool.rs:262` | komira-ai/jobscheduler#1132 |
| §3 Persistence (D55) | Reservations persisted in the lease file; the lease-side grace is 'a separate setting, not yet wired'. | superseded: `crates/js-jobs/src/lib.rs:218` | - |
| §3 Persistence (D55) | Restart restores marks, counters and bars; restore_whole, bar_whole, cancel_whole, whole_state and whole_summary exist. | `crates/js-sched/src/scheduler/whole/restore.rs:103`, `crates/js-sched/src/scheduler/whole_view.rs:149` | komira-ai/jobscheduler#778 |
| §4 Draining (deadline) | The drain deadline is informational and shown as drain_deadline_ms; a drain is never given up. | `crates/js-lease-server/src/hosts.rs:113-118` | - |
| §5 items 1-5 | Preparing, ready, exclusive_ready, typed exclusive_refused, readiness hook, prepare_timeout, max_replace, exclusive-not-ready. | `crates/js-agent/src/exclusive.rs:57`, `crates/js-agent/src/readiness.rs:93` | komira-ai/jobscheduler#834; komira-ai/jobscheduler#1071 |
| §5 item 6 | The wall-time clock starts at the token, not at the commit (D52). | superseded: On the job path the job's own wall limit applies; the allocation records the wall limit (page §5 item 7). No lease token exists after the lease API was deleted  | - |
| §6 Caps | max_whole_node, max_draining, keep_for_ordinary, allow_last_capable and the effective ceiling are enforced; validation at load. | `crates/js-whole/src/caps.rs`, `crates/js-sched/src/scheduler/whole.rs:56` | komira-ai/jobscheduler#1193 (new) |
| §6 Caps (per consumer) | max_whole_node_per_consumer is enforced 'where the caller's identity exists: the lease server or the registry'. | superseded: `docs/design/jobscheduler-design.md:817` | komira-ai/jobscheduler#1193 (new) |
| §1 / §6 Mixed pools | A mixed pool (class action or vm) offers whole-node when max_whole_node is set, protected by keep_for_ordinary and last-capable. | no | komira-ai/jobscheduler#1149 (new) |
| §7 Elastic providers (step 58) | A whole-node request acquires a fresh machine instead of draining; the acquire carries the request id and the machine is committed to it at registration ('reserved at birth'); a launch counts toward the ceiling from the acquire; a quiet host that appears first takes the request and the machine is released by idle rule. | `crates/js-lease-server/src/scaler/jobs.rs:1-35`, `crates/js-scaler/src/pass.rs:39` | - |
| §7 Elastic pool at machine cap (D62-65, step 64) | At max_machines with a growing queue, drain, evict and swap up one elastic machine; never a host with a whole-node lease or mark. | no | komira-ai/jobscheduler#1150 (new) |
| §8 Host jobs (D51) | A lease may carry host_jobs only if whole-node (W11). | superseded: `crates/js-lease-server/src/admit.rs:82` | komira-ai/jobscheduler#786; komira-ai/jobscheduler#795 |
| §8 End | The host is bookable only after the agent restores; a tainted host is never bookable. | `crates/js-sched-placer/src/whole_jobs.rs:63-80`, `crates/js-jobs/src/store/whole.rs` | komira-ai/jobscheduler#1071 |
| §9 kbf's use | kbf consumer config maps whole_machine to whole-node; kbf rungs and max_leased_machines; ladder rung 'whole host' as a whole-node request. | superseded: The lease API this speaks to is deleted (PR komira-ai/jobscheduler#831). On main a whole-host request is a job whose needs name the whole host (page §3 note). | komira-ai/jobscheduler#571 |
| §10 Wire | HeartbeatReply.exclusive, Heartbeat.exclusive_ready and exclusive_refused fields are additive. | `crates/js-agent-wire/src/heartbeat.rs:189-198` | - |
| §11 Tests and invariants W1-W14 | Invariants checked over seeded runs of the fake agent and simulation. | partly: `crates/js-sched/tests/whole_node_caps.rs` | komira-ai/jobscheduler#1133 |
| §12 step 46 | The Whole admission, commit at quiet, readiness gate, end. | `crates/js-agent/src/exclusive.rs`, `crates/js-sched-placer/src/whole_jobs.rs` | - |
| §12 step 54 | GPU machines register into a whole-node pool. | partly: Mechanism built (js-agent join, class whole-node). The deployment is not repo code. | - |
| §12 step 56 | Reservation and drain. | `crates/js-sched/src/scheduler/whole.rs`, `crates/js-whole/src/cost.rs` | komira-ai/jobscheduler#582; komira-ai/jobscheduler#1127; komira-ai/jobscheduler#1129; komira-ai/jobscheduler#1131; komira-ai/jobscheduler#1133 |
| §12 step 57 | Caps and the host-job rule. | partly: `crates/js-whole/src/caps.rs` | dropped by design: jobscheduler-design.md says there is no per-consumer cap in the single-token service; whole-node.md is stale |
| §12 step 58 | Elastic machines for whole-node requests. | `crates/js-lease-server/src/scaler/jobs.rs:14` | komira-ai/jobscheduler#582 |
| §12 step 59 (via §1) | Packing and demand, with ucdk classes admitted Book. | superseded: `crates/js-sched-placer` | komira-ai/jobscheduler#833; komira-ai/jobscheduler#906 |

## lease-limits.md

As read: No status header, no What-is-left list, no open questions or build steps. The page is written as current design ("always on"). Its only explicit status markers are one "planned ... dropped" note and one "planned in scheduler.md" pointer.

| Section | Claim | On main | Tracking |
|---|---|---|---|
| 12.3 Labels, ucdk adapter | A ucdk adapter that carried the label set into ucdk's Stamp.extra was planned, then dropped. | superseded: `docs/design/decisions.md:39` | komira-ai/jobscheduler#28 |
| 12.2 Usage record | The scheduler's usage record is the 'Accounting' record planned in docs/design/scheduler.md. | partly: `docs/design/scheduler.md`, `crates/js-lease-server/src/job_api/fleet.rs:768` | komira-ai/jobscheduler#1151 (new) |
| 12.1 Reaper machine ownership (leaked instance) | The reaper releases a leaked instance: one that list returns, carries our tags, and no pool host or acquire record accounts for, after a grace of ticks. | no | - |
| 12.1 Lease end | The lease table `js-lease` is deleted; the job service of js-lease-server ends a job at its wall. | `crates/js-lease`, `crates/js-jobs` | - |
| 12.3 Labels, instance tags and sweep | Providers tag instances with jobscheduler.dev/pool, acquire and scheduler; destroying a Job definition or the environment sweeps billable leftovers by the instance label as part of the closed-world delete, with a test on the fake. | partly: instance tags and a report-only leftover sweep are built (`crates/js-provider-gce/src/own.rs:109`, `crates/js-watch/src/lib.rs:11`: leftovers are counted and reported, never released; `abort` acts only when told to apply). Not built: the cleanup on deleting a Job definition or environment, which has nothing to hang on, as jobscheduler has no Job definitions or environment delete (docs/design/ucdk.md is superseded) | - |

## held-beat.md

As read: No status header; the page is written as built and describes current behaviour. It has no Designed, Planned, Open question, What is left or build-step sections.

No claim of the kinds above.

## rollout.md

As read: No status header and no Designed/Planned/Open labels. It is a design for rolling out js-agent under kbf's coordinator, and it still presents that plan in the present tense. Most of its premise is overtaken: hostkeeper, packages.komira.ai and macos-pkg replaced kbf's coordinator and signer for js-agent. The code-side compatibility rules are built.

| Section | Claim | On main | Tracking |
|---|---|---|---|
| header / step 55 | The compatibility checks (D44) are step 55 of extraction-steps-d.md: refuse an unknown major, a previous-release matrix, a client that reads the server's advertised version and features and fails closed. | partly: `crates/js-agent-wire/src/protocol.rs:8-10`, `crates/js-agent/tests/agent/skew/mod.rs:1-30` | - |
| What exists, bullet 2 | The kbf updater and helper handle described services (kbf#1826 open, Linux local socket only; the Mac helper serves kbf only). | superseded: `crates/js-agent/src/update_hook/` | komira-ai/jobscheduler#841; komira-ai/jobscheduler#885; komira-ai/jobscheduler#898 |
| Who updates what: js-agent row | js-agent is updated by kbf's coordinator as service js-agent with after = ["kbf"], and is republished under kbf's signer until the signer decision. | superseded: Release signs files (komira-ai/jobscheduler#807) and uploads the .deb and the Mac package to pkg/jobscheduler. The hostkeeper deploy is in release.yml per AGENT | komira-ai/jobscheduler#807; komira-ai/jobscheduler#841; komira-ai/jobscheduler#889; komira-ai/jobscheduler#898 |
| Who updates what: js-lease-server row | js-lease-server updates through its own update hook and restarts in place. | `crates/js-lease-server`, `crates/js-update-hook` | komira-ai/jobscheduler#765 |
| Who updates what: Action images and kbf-task row | The frontend names the image digest and the kbf-tools version in each run; a host without them refuses the run with a constant code. | partly: `crates/js-agent-wire/src/refusal.rs:37` | - |
| Who updates what: Tierstore front row | The presigned-PUT verifier is rolled before the frontend that mints for it. | decision, no code | - |
| Who updates what: Elastic machines row | Elastic machines are not updated; a new machine boots the current pinned agent and an old machine leaves by the idle rule (5.10). | `crates/js-elastic` | komira-ai/jobscheduler#793 |
| Who updates what: Macs row | Macs are updated by the Mac helper, and stay on the kbf daemon until a Mac task runtime exists. Task bundles and the VM base are OCI artifacts pinned by digest in the signed release manifest, written to a root-owned allow file. | superseded: `crates/js-mac-session` | komira-ai/jobscheduler#888; komira-ai/jobscheduler#898; komira-ai/jobscheduler#913; komira-ai/jobscheduler#988 |
| Version compatibility rule 1 | Protocol majors, with additive fields inside a major. A new major is a new package or service that runs beside the old one. | `crates/js-agent-wire/src/protocol.rs:3-24` | - |
| Version compatibility rule 2 | The server speaks every major its agents use and retires one only after the last agent has gone. A skew test in CI runs the newest agent against the pinned server and the reverse. | partly: `crates/js-agent/tests/agent/skew/mod.rs:1-30` | komira-ai/jobscheduler#836 |
| Version compatibility rule 3 | The server refuses an unknown major with a Welcome refusal code. The agent exits with status 2 and keeps its containers for adoption. | yes: Server: js-lease-server/src/agent_api/refused.rs:74 and agent-wire/src/refusal.rs:130 | - |
| Version compatibility rule 4 | Security fields are gated on Hello.capabilities, never on version text. An old agent that ignores `upload` returns no upload outcome, which the frontend treats as a failed action. | `crates/js-agent-wire/src/caps.rs:21-27` | - |
| Version compatibility rule 6 | One pack format, one crate (js-pack-format), written by the task tool and verified by the frontend at N and N-1. | no | - |
| Rollout order 2 | Per host the updater installs on trial. On a crun host the running agent hands its jobs over within seconds. Any other host is cordoned and drained, bounded by a cap, then the helper switches, the agent says Hello, the ready check passes, and the helper confirms or reverts. | `crates/js-agent/src/update_hook/step/`, `crates/js-agent/src/cordon/mod.rs` | komira-ai/jobscheduler#947; komira-ai/jobscheduler#972 |
| Rollout order 3 | Rollback of the executor path is the flag --executor kbf-daemon (default), needing no package change. | no | - |
| Rollout order, retiring a major | A change that retires a major adds a step at the end: the server drops the major only after the agents that used it have gone. | no | - |
| What would change the plan (D45) 1 | A js-agent population that does not run kbf and must be updated would need a second coordinator instance, or replace-not-update. | superseded: Every host updates through hostkeeper (the .deb, komira-ai/jobscheduler#841, and macos-pkg, komira-ai/jobscheduler#898), and the agent runs without kbf on the h | komira-ai/jobscheduler#841; komira-ai/jobscheduler#898 |
| D45 3 / K36 | Signer decision: kbf republishes the agent under its own signer, with branch protection on jobscheduler first. Trusting jobscheduler's workflow is reopened only after rulesets and a protected release environment. | partly: The release now signs its own files (komira-ai/jobscheduler#807) and uploads to packages.komira.ai, so jobscheduler's workflow is trusted directly | komira-ai/jobscheduler#1115 |
| D45 4 | A Mac task runtime would change the plan; until then Macs are not on this plan. | superseded: `crates/js-mac-session` | komira-ai/jobscheduler#888 |
| What stays open | The Mac default, the elastic provisioner's home, the lease-server integration and the other remaining decisions (D3 to D9) are open in kbf-on-js-agent.md section 15. | decision, no code | - |
| What stays open (last sentence) | Deploying a service, installing a package on hosts and any registry upload need the project owner's go (R7). | decision, no code | - |

## risks-f.md

As read: No status header and no Designed/Planned/step/What-is-left labels. It is a risk register (K19 to K48). Only a few mitigations are marked unbuilt, open or superseded; the rest are stated as built-in-design. Several rest on a lease-file/agentless-era design that has since moved to Postgres state and agent-only pools.

| Section | Claim | On main | Tracking |
|---|---|---|---|
| K36 | The host updater verifies only kbf's signer; a js-agent package signed by jobscheduler's key is not installable. Page says: still open, a precondition (kbf republishes the agent under its own signer before any production install; owner security decision). | decision, no code | komira-ai/jobscheduler#1115 |
| K35 | Agent fixes wait behind the kbf package. | superseded: Page's own marker. kbf-on-js-agent.md:727 says the assumption that js-agent ships inside the kbf package is gone. | - |
| K25 | A boot per VM run; a warm pool of pre-booted VMs is future work. | no | - |
| K29 | The lease file is rewritten whole on every change; an external store replaces it when the rewrite is a noticeable share of the tick. The page says the lease file is deleted. | superseded: `crates/js-state/src/postgres/mod.rs` | komira-ai/jobscheduler#741 |
| K28 | One pod is one point of outage. Not mitigated: the outage itself; an external store and an election are the way out (D33, 5.16). Mitigations named: durable state, boot grace, a PodDisruptionBudget. | partly: `crates/js-state/src/postgres/mod.rs`, `crates/js-lease-server` | komira-ai/jobscheduler#741 |
| K30 | Volume is the state: block volume, Retain, '--boot resume', scheduled snapshots, and a verify mode that checks the job store and binding file against each other before a restore (5.16, step 50). | superseded: `crates/js-state/src/postgres` | - |
| K31 | The container runs as root to read root-owned secrets; a server change to accept a root-owned file on a read-only mount would remove the exposure. | decision, no code | - |
| K34 | A static test provider reaches production. Mitigated by the --allow-test-providers flag and a layering test that no production example names the kind. | superseded: `crates/js-lease-server/src/file/agentless.rs:2-23` | - |
| K20 | The consumer reads the server's features and refuses to send needs or runtime=vm to a server lacking needs support (10.3, capabilities.md). | no | komira-ai/jobscheduler#1172 (new) |
| K21 | Guest channel and hypervisor attack: a bounded, fuzzed frame codec, no shared writable host path, host checks after every VM job, taint on failed destroy. | partly: `crates/js-guest-wire`, `crates/js-agent-wire/fuzz` | komira-ai/jobscheduler#1152 (new) |
| K23 | Base image drift: immutable names and a digest pinned in Start and verified. | partly: `crates/js-vmm/src/image.rs`, `crates/js-agent-wire/src/caps.rs:45` | - |
| K19 | Constraint matching: refuse unknown ops, absent-is-unmet, one-image guest scope, and a constraint suite (5.13). | `crates/js-api-gateway/src/props.rs:17`, `crates/js-bundle/tests/validate.rs:542` | - |
| K24/K38/K40/K41 | Whole-node class: prepare_timeout and retry cap, drain deadline, max_draining, keep_for_ordinary, max_replace, regain_grace, persisted reservation. | `crates/js-whole`, `crates/js-lease-server/src/agent_pool.rs:48-73` | - |
| K33/K42/K44/K45/K46 | Elastic scaler: idle rule, max_machines, cooldown, boot-failure brake, offer cooldown, capacity signal, spot reclaim taint, a conformance case with a lying option. | partly: `crates/js-scaler/src/brake.rs:1-22`, `crates/js-elastic/src/cache.rs:14` | komira-ai/jobscheduler#1174 (new) |
| K47/K48 | Swap of a victim machine for a bigger type: swap_drain_grace, max_evictions, protected hosts, launching before draining (D62 to D65). | no | komira-ai/jobscheduler#1150 (new) |
| K22/K43/K26/K27/K32/K37/K39 | VM vs container accounting, shared-host blast radius, wire growth limits, unverified facts, no-capable-host with regain_grace, host-job taint, quiet-is-a-claim. | partly: `crates/js-agent-wire/src/refusal.rs:278`, `crates/js-lease-server/src/admit.rs:7` | - |

## merged-code.md

As read: Status: "a plan of the extraction steps, kept as history"; header already says Pool, BoxedPool, FakePool, RegisteredProvider and the lease table are deleted. Table body is stale in places (rows still describe Pool, RegisteredProvider, js-lease, lease API). Remaining real gaps: typed facts and needs (steps 36-39), the vms dimension (44), the Sandbox seam (41-43).

| Section | Claim | On main | Tracking |
|---|---|---|---|
| Row 1 Provider to Pool | Provider renamed Pool; still to do ProviderId to PoolId and capacity() to hosts() | superseded: `crates/js-provider/src/lib.rs` | komira-ai/jobscheduler#831 |
| Row 2 Flavour | Flavour, flavour(), 'delegated flavour only' doc deleted | `crates/js-lease-server/src/sched/jobs_tests.rs:611`, `crates/js-elastic/src/provider.rs:135` | komira-ai/jobscheduler#831 |
| Row 3 StartAck/StopAck/Observed - AcquireAck and ReleaseAck added, fence and no-retries rules still to model (step 23) | AcquireAck and ReleaseAck added beside the three answers; fence and no-retries rules to model | superseded: `crates/js-provider/src/ack.rs:5` | komira-ai/jobscheduler#831 |
| Row 4 HostCapacity/capacity(); js-provider crate (step 23) | capacity() renamed hosts() on Pool; CapacityProvider::list added; crate gains CapacityProvider beside Pool | superseded: `crates/js-elastic/src/provider.rs:129` | komira-ai/jobscheduler#831 |
| Row 5 StaticPool to FakeHostPool (steps 28, 54) | StaticPool is FakeHostPool in js-pool-fake, tests only; --pool-config refused; GPU machines run js-agent and register with a default provider | `crates/js-pool-fake/src/lib.rs:1-10`, `crates/js-lease-server/src/config.rs:565-571` | komira-ai/jobscheduler#840 |
| Row 6 HostBook et al (step 6) | HostBook, Candidate, ChooseOver, Held, SeatError stay in js-provider | `crates/js-provider/src/book.rs:11` | - |
| Row 7 RegisteredProvider, Registry (step 30c) | RegisteredProvider becomes the core of DefaultProvider with capable-host check, no-capable-host refusal, regain_grace, pool-full refusal, kind name default | `crates/js-registry/src/capacity.rs:44`, `crates/js-agent-wire/src/refusal.rs:278` | komira-ai/jobscheduler#831 |
| Row 8 LayeredProvider | Deleted; order is the configured provider order of the default platform (rank, set_provider_rank) | `crates/js-lease-server/src/sched/rank.rs:1-25`, `crates/js-sched/src/provider_rank.rs` | komira-ai/jobscheduler#833 |
| Row 9 pool.seat/Seated/Unseated (step 32) | For aws, seating becomes re-adoption by tag | `crates/js-provider-ec2/src/http/launch_calls.rs:37-42`, `crates/js-scaler/src/pass.rs:227` | - |
| Row 10 CapacitySource (steps 29, 30, 32) | One implementation over any CapacityProvider; RegisteredAgentSource serves default provider; call made asynchronous | superseded: `crates/js-elastic/src/provider.rs:164` | - |
| Row 11 js-provider-fake (step 28) | Merged fake becomes the fake capacity provider (fake aws) with scripted failures | `crates/js-elastic-fake/src/lib.rs:1-12` | komira-ai/jobscheduler#831 |
| Row 12 js-conformance (steps 28, 32) | Machinery stays; checks re-keyed to capacity-provider and agent contracts; labels-on-created-resources follow-up lands with first resource-creating provider | partly: `crates/js-conformance/src/lib.rs:13-15`, `crates/js-elastic/src/conformance.rs` | - |
| Row 13 js-lease (steps 29, 30b) | js-lease gains acquiring state, pool name in lease file, acquire record, tainted-host set | superseded: `crates/js-lease` | komira-ai/jobscheduler#831 |
| Row 14 js-lease-server (steps 29 to 33) | POST /v1/leases accepts class and label selectors; registration endpoint serves agent hello; pool-scaler tick beside the reaper | partly: `crates/js-api-gateway/tests/gateway.rs:139-140`, `crates/js-lease-server/src/agent_api.rs:173` | komira-ai/jobscheduler#831 |
| Row 15 js-cli (steps 33, 30b) | js-cli gains --pool, --class, --label on acquire and host clear-taint | superseded: `crates/js-cli/tests/cli/help.rs:141-170` | komira-ai/jobscheduler#831 |
| Row 16 js-agent-wire Hello.facts, Heartbeat.facts_rev/facts, HeartbeatReply.facts_ack (step 37) | Typed facts fields added as optional wire fields | no | komira-ai/jobscheduler#1169 (new) |
| Row 16b exclusive, exclusive_ready (step 46) | HeartbeatReply.exclusive and Heartbeat.exclusive_ready added as optional fields; Preparing/Restoring phases serve whole-node sequence | `crates/js-agent-wire/src/heartbeat.rs:253`, `crates/js-agent-wire/src/hello.rs:34-36` | - |
| Row 16c JobSpec.runtime and vm (step 42) | JobSpec.runtime and vm added as optional fields | superseded: `crates/js-agent-wire/src/kind.rs:15` | komira-ai/jobscheduler#740 |
| Row 17 js-types Capabilities / Shape::satisfies / Dim vms (steps 36, 44) | New js-caps holds typed model and constraint evaluator; Dim gains vms | partly: the scheduler's `sched::Resources` has a `vms` field, filled from vm.slots (`crates/js-types/src/sched/work.rs:109`). Not built: the named `Dim` has no `vms` (`crates/js-types/src/resources.rs:19`, and `dimension_names_round_trip_and_unknown_is_refused` refuses it), and the typed model and evaluator of js-caps are step 36 (komira-ai/jobscheduler#1168) | komira-ai/jobscheduler#1168 |
| Row 18 js-registry/js-lease-server agent routes (steps 37, 39) | Hold newest facts snapshot per host, ack revisions, match needs at the grant, record matched revision and image in the lease | no | komira-ai/jobscheduler#1171 (new) |
| Row 19 js-agent facts probes (step 38) | Gain os_version, xcode, sim.* and VM-store probes and --cap, reusing the bounded command runner | partly: `crates/js-agent-wire/src/caps.rs:60`, `crates/js-agent/src/xcode/` | komira-ai/jobscheduler#1170 (new) |
| Row 20 js-agent lifecycle Sandbox seam (steps 41 to 43) | Lifecycle reaches Runtime through a Sandbox seam; VmRuntime is a second backend; HeldState gains nothing | superseded: `crates/js-agent/src` | komira-ai/jobscheduler#740 |
| Extraction step 23 (Pool rename, hosts(), AcquireAck, ReleaseAck) | Rename PR | superseded: Pool deleted (see Row 1); acquire/release answers exist in js-elastic. | komira-ai/jobscheduler#831 |
| Extraction step 28 | Fake capacity provider and conformance re-keyed | `crates/js-elastic-fake/src/lib.rs`, `crates/js-elastic/src/conformance.rs` | - |
| Extraction step 29 | Lease table acquiring state, async acquire | superseded: `crates/js-scaler/src/pass.rs` | komira-ai/jobscheduler#831 |
| Extraction step 30 / 30b | Agent registration, reuse and integrity, isolation consents, taint set | `crates/js-lease-server/src/agent_api.rs:173`, `crates/js-jobs/src/lib.rs:38` | - |
| Extraction step 30c | default provider from RegisteredProvider | `crates/js-lease-server/src/file.rs:163`, `crates/js-registry/src/capacity.rs:44` | - |
| Extraction step 32 | Elastic provider aws; seating becomes re-adoption by tag | `crates/js-provider-ec2`, `crates/js-scaler/src/pass.rs:503` | - |
| Extraction step 33 | Consumer pool selection: selectors in lease request, --pool/--class/--label | superseded: `crates/js-cli/tests/cli/help.rs:141-170` | komira-ai/jobscheduler#831 |
| Extraction steps 36, 37, 38, 39 | js-caps typed model; wire facts; agent facts and --cap; needs and matching | no | komira-ai/jobscheduler#1168 (new) |
| Extraction steps 41, 42, 43 | Sandbox seam; VmRuntime, GuestChannel, js-guest-wire, runtime/vm JobSpec fields; VM lifecycle | partly: `crates/js-agent/src/vm/runtime.rs` | komira-ai/jobscheduler#740 |
| Extraction step 44 | Mixed-host accounting with the vms dimension | `crates/js-sched/src/scheduler/worker.rs:191-200` | komira-ai/jobscheduler#129 |
| Extraction step 46 | Whole-node class and exclusive sequence | `crates/js-types/src/class.rs:13`, `crates/js-agent/src/exclusive.rs:8-22` | - |
| Extraction step 54 | GPU machines register with a default provider | `crates/js-lease-server/src/config.rs:156` | komira-ai/jobscheduler#840 |

## jobscheduler-design.md

As read: Current entry-point design doc (target plus as-built notes); not superseded, but its Planned / Partly built / step marks are widely stale: most were overtaken by merged code (lease API deleted, job store on Postgres, platform layer, GPU crun, arm64, live logs, per-client tokens).

| Section | Claim | On main | Tracking |
|---|---|---|---|
| intro / 3.1 table: generated CLI and library | Generated js job/host/pool/provider/usage commands and JobClient exist; three hand-written loop commands and js join-token stay by design | `crates/js-client/src/jobs/`, `crates/js-cli/src/jobs/` | - |
| 3.1 Jobs and allocations; 4.1 store; 3.1 Postgres store | Job/allocation tables on Postgres (not only SQLite file beside the lease file) | `crates/js-lease-server/src/job_store.rs:1-30` | komira-ai/jobscheduler#874; komira-ai/jobscheduler#878 |
| 3.1 Host table | Host book (hosts, sessions, credentials) as scheduler state in Postgres, with no js-registry component | partly: `crates/js-lease-server/src/node_rows.rs:1`, `crates/js-registry` | komira-ai/jobscheduler#873 |
| 3.1 Queue and tail marks | Whole-node lease wait list/ledger leave the engine memory with the lease API | `crates/js-lease-server/src/job_whole.rs` | komira-ai/jobscheduler#831 |
| 3.1 Usage history and estimates | Estimate feed (--estimate-feed) default decision; feed reads leases only | superseded: `docs/design/estimate-feed.md:3` | - |
| 3.1 Placement engine | Several elastic pools in one server | no | komira-ai/jobscheduler#1153 (new) |
| 3.1 Placement engine / step 8 | One engine owns allocations; layered.rs, registry picker, --placement removed | `crates/js-lease-server/src/layered.rs`, `crates/js-lease-server/src/sched/rank` | komira-ai/jobscheduler#831; komira-ai/jobscheduler#837 |
| 3.1 Container-platform layer; step 7 | Server calls DefaultPlatform / ContainerPlatform | `crates/js-lease-server/src/platform.rs:25`, `crates/js-platform/src/lib.rs:292-296` | komira-ai/jobscheduler#678 |
| 3.1 Static machine provider; step 6 | Server calls StaticCapacity as the static CapacityProvider | partly: `crates/js-registry/src/capacity.rs` | - |
| 3.1 Capability vocabulary; step 25 | Structured accelerator, ISA level, alias table in code; Resources still books gpu count and class | `crates/js-types/src/accel.rs:29`, `crates/js-types/src/isa.rs` | komira-ai/jobscheduler#661 |
| 3.1 Catalog translation, verify-on-hello; step 26 | Catalog cache, cool-down, stored offer, verify-on-hello | `crates/js-elastic/src/cache.rs`, `crates/js-scaler/src/verify.rs` | - |
| 3.1 Other container platforms; 3.7; step 24 | Kubernetes, Cloud Run, ECS platforms | no | - |
| 3.1 Job network policy; steps 12-14, 16 | crun networking wired: network and ports fields, ceilings, DNAT forwards, Podman firewall retired | `crates/js-agent/src/egress/forward.rs:7`, `crates/js-agent/src/cli.rs:421-433` | komira-ai/jobscheduler#668 |
| step 15 / 5.8 macOS mapping | open value on --native-network, level mapping, ports refused, root test for inbound filtering by uid | partly: `crates/js-agent/src/native/network.rs:46-132` | komira-ai/jobscheduler#679 |
| 3.1 macOS runtime; step 17; 12.2 Macs | One image definition on both platforms: native body folded in; stage inputs and collecting outputs | partly: `crates/js-agent-wire/src/command.rs:233`, `crates/js-agent/src/native/mod.rs:111` | komira-ai/jobscheduler#1154 (new) |
| 3.1 macOS runtime | A job cannot ask for a VM on macOS | partly: `crates/js-agent-wire/src/kind.rs:15`, `crates/js-vmm` | komira-ai/jobscheduler#1155 (new) |
| 3.1 Agent | Per-run start/stop dropped from the agent wire (rest of step 28) | partly: `crates/js-agent-wire/src/command.rs:265`, `crates/js-agent/src/lifecycle/machine/mod.rs:239` | komira-ai/jobscheduler#1156 (new) |
| 3.5 provider table | lambdalabs row 'Partly built: built as ElasticProvider' | `crates/js-provider-lambdalabs` | komira-ai/jobscheduler#652 |
| 3.5 ContainerPlatform sketch | capabilities and can_ever_serve; logs for Default | partly: `crates/js-platform/src/lib.rs:292` | komira-ai/jobscheduler#678 |
| 3.5 CapacityProvider sketch | events: Interruption notice (Reclaimed built) | no | komira-ai/jobscheduler#1174 (new) |
| 3.6 service definition, live logs, generated clients | Service definition, GetLogs/live logs, generated CLI and library | `crates/js-proto/proto/jobscheduler/v1/`, `crates/js-lease-server/src/job_api/` | komira-ai/jobscheduler#831 |
| 3.6 / 11 per-client tokens and quotas | Per-client tokens (and quotas) | partly: `crates/js-lease-server/src/file/clients.rs:8-30` | - |
| 4.1 [logs] and store rows | [logs] table and store setting | `crates/js-lease-server/src/file/logs.rs:8` | - |
| 4.2 target config | [[platform]], [[machine_provider]], provider_limits, queue settings | `crates/js-lease-server/src/file/platform.rs` | komira-ai/jobscheduler#678 |
| 4.3 --log-file-cap | --log-file-cap agent flag | `crates/js-agent/src/cli.rs:275` | - |
| 5.1 | One-step host reimage/enrol command | no | - |
| 5.3 | Submit/place/cancel served over job API (Planned, step 27) | `crates/js-lease-server/src/job_api/submit.rs:21` | komira-ai/jobscheduler#831; komira-ai/jobscheduler#1015 |
| 5.4 / 12.2 whole-host | Proof queue order respected; restart test for row 10; rows 25/28/39/40 closed by design | partly: `crates/js-lease-server/src/agent_api_whole_restart_tests.rs` | komira-ai/jobscheduler#1133 |
| 5.6 placement walk | Walk status: several elastic pools; pass 1 telling two launching providers' hosts apart | partly: `crates/js-lease-server/src/file.rs:422`, `crates/js-sched/src/acquire.rs:188` | komira-ai/jobscheduler#1153 (new) |
| 5.6.1 / 3.8 GPU on crun; step 23 | GPU passthrough into crun jobs | `crates/js-agent/src/crun/gpu.rs:1` | - |
| 5.6.1 / step 19 / Q18 Linux arm64 | Linux arm64 archive in the release | `.github/workflows/artifacts.yml:2` | - |
| 5.7 as-built box; 6.x | Idempotent start, step 21 fence token (job id with allocation) | `crates/js-agent-wire/src/per_job.rs:43-45`, `crates/js-agent/src/cli.rs:329-334` | - |
| 5.8 macOS mechanism / table | open value, DNAT forward on Mac | partly: native/network.rs:46 (open built); DNAT forward on Mac refused (native/mod.rs:111) | - |
| 6.5 scaler | Scaler demand logic folds into pass 2 of the walk; scaler keeps caps, brake, records | `crates/js-sched/src/acquire.rs:188`, `crates/js-scaler` | komira-ai/jobscheduler#837 |
| 8 row 1-3, 27, 29, 30 (lease table rows) | Lease-era races (lost submit answer, ignored saves, runs not persisted, state-file writes) | superseded: `crates/js-lease-server/src/job_store.rs:1` | komira-ai/jobscheduler#831 |
| 8 row 20 ETXTBSY | ETXTBSY on installed helper | yes: closed issue komira-ai/jobscheduler#604 js-contain ETXTBSY | komira-ai/jobscheduler#604 |
| 8 row 22 Full Disk Access reaching a job | Supervisor that keeps TCC responsibility | `docs/design/spawn-disclaim.md:3`, `crates/js-mac-session/src/disclaim.rs` | komira-ai/jobscheduler#589; komira-ai/jobscheduler#1020; komira-ai/jobscheduler#1025; komira-ai/jobscheduler#1117 |
| 8 rows 44-51 live logs | Live-log failure cases tested | partly: `crates/js-lease-server/src/agent_api_log_tests.rs`, `crates/js-agent/src/logs/tests.rs` | - |
| 8 row 21 benchmark at 1000 jobs | Benchmark scheduler at 1000 jobs | partly: `crates/js-measure/src/lib.rs:76` | komira-ai/jobscheduler#836 |
| 10 State (Partly built) | Postgres for jobs, allocations, hosts, usage; tail counters | partly: job_store.rs:1; usage in --store; tail counters deleted with lease table (estimate-feed.md:3); host book rows partly node_rows.rs | - |
| 11 Limits: one elastic pool per server | Lift 'at most one elastic pool' (step 7) | no | komira-ai/jobscheduler#1153 (new) |
| 12.1 step 1 | Service definition and generated clients; HTTP/JSON gateway | `crates/js-proto`, `crates/js-api-gateway/src/annotation.rs` | - |
| 12.1 steps 2-4 | Postgres job/allocation tables, usage, durable results and retention | `crates/js-lease-server/src/job_store.rs:1` | komira-ai/jobscheduler#874 |
| 12.1 step 5, 6 | CapacityProvider trait with conformance suite; static provider; StaticPool removed | `crates/js-elastic/src/provider.rs:140-216`, `crates/js-registry/src/capacity.rs` | komira-ai/jobscheduler#652 |
| 12.1 step 7 | Platform layer, configuration and admission | `crates/js-platform/src/lib.rs:292`, `crates/js-lease-server/src/platform.rs:53` | komira-ai/jobscheduler#678 |
| 12.1 step 8 | One placement engine that owns jobs and allocations | `crates/js-sched-placer/src/jobs.rs`, `crates/js-lease-server/src/sched/` | komira-ai/jobscheduler#831; komira-ai/jobscheduler#879 |
| 12.1 step 9 | Placement decides acquires (pass 2) | `crates/js-sched/src/acquire.rs:188` | komira-ai/jobscheduler#837 |
| 12.1 step 10; Q3 | Estimate feed decision | superseded: `docs/design/estimate-feed.md:3` | - |
| 12.1 step 11 | kbf submits jobs; retire js-server-core | partly: js-server-core crate no longer in crates/; kbf side is in another repository | komira-ai/jobscheduler#831 |
| 12.1 steps 12-14 | Wire crun networking, per-job network field, DNAT forwards | `crates/js-agent/src/crun/mod.rs:136` | - |
| 12.1 step 16 | Retire Podman firewall | `crates/js-agent/src/cli.rs:421-433` | - |
| 12.1 step 18 | Measure launch time and publish the number | partly: `docs/design/agent-egress.md:251-285`, `.github/workflows/js-launch-measure.yml` | komira-ai/jobscheduler#668 |
| 12.1 step 19 | EC2 and GCE providers; arm64 archive | `crates/js-provider-ec2`, `crates/js-provider-gce` | komira-ai/jobscheduler#757; komira-ai/jobscheduler#767 |
| 12.1 step 20; Q: agent wire | Decide whether hello/heartbeat leave JSON polling | decision, no code | komira-ai/jobscheduler#670 |
| 12.1 step 21 | Self-fence on silence, fencing token, per-client tokens, update hook, host-job taint | `crates/js-agent/src/cli.rs:334` | - |
| 12.1 step 22 | crun is the Linux runtime | `crates/js-agent/src/platform` | komira-ai/jobscheduler#890 |
| 12.1 step 23 | GPUs for crun | `crates/js-agent/src/crun/gpu.rs:1` | - |
| 12.1 step 24 | Other container platforms behind ContainerPlatform | no | - |
| 12.1 steps 25, 26 | Capability vocabulary and alias table; catalog translation, cache, verify-on-hello | `crates/js-types/src/accel.rs`, `crates/js-elastic/src/cache.rs` | - |
| 12.1 steps 27, 28 | Job calls served; agent protocol per job | partly: job_api/submit.rs:21; job_expiry.rs; job_loss.rs; agent-wire per_job.rs; lease/run messages still in js-agent-wire command.rs:265 | komira-ai/jobscheduler#1156 (new) |
| 12.1 steps 29, 30 | Remove renewal; delete lease API | yes: PR komira-ai/jobscheduler#831; no renew/lease-ttl code in js-lease-server; js-lease crate gone | komira-ai/jobscheduler#831 |
| 12.1 step 31 | Live logs | `crates/js-logstore`, `crates/js-logmint` | - |
| 12.2 Macs remaining | read model allow; notarized binaries; automated isolation test per release; VM runtime with network path | partly: `crates/js-agent/src/bundle/settings.rs:253`, `xtask/tests/macos_pkg_workflow/main.rs:561` | komira-ai/jobscheduler#1157 (new); komira-ai/jobscheduler#1158 (new) |
| 12.2 Cross-cutting | Supervisor definition, deny list of hosts, spot reclaim handling, safe restore verb | no | komira-ai/jobscheduler#1174 (new) |
| 13 Q1 one writer or HA | Active writer vs hot standby vs several active | superseded: `crates/js-lease-server/src/lib.rs:422` | komira-ai/jobscheduler#873; komira-ai/jobscheduler#878 |
| 13 Q2 HTTP/JSON gateway | B: generated gateway under /v1/jobs | `crates/js-api-gateway/src/annotation.rs` | - |
| 13 Q3, Q10, Q4 | Estimate feed default; service update procedure; which cloud next | superseded: Q3 estimate-feed.md:3; Q10 js-lease-server/src/update_hook/mod.rs (hook built); Q4 EC2 and GCE both built | - |
| 13 Q5, Q6, Q7 | Self-fence; agent restart adoption; drop Docker/Podman fallback | partly: `crates/js-agent/src/cli_runtime/` | - |
| 13 Q8 | Above the ceiling: refuse or lower | superseded: `crates/js-agent/src/native/mod.rs:194` | komira-ai/jobscheduler#679 |
| 13 Q9 | Refuse public non-wildcard API listener without explicit flag | decision, no code | - |
| 13 Q11, Q15, Q16, Q19, Q20, Q22 | Host-updater trust; reimage command; MDM ownership; flapping agent prepare; allow-list richness; first other platform | decision, no code | - |
| 13 Q12 | Agent drain cap of 3600 s | decision, no code | - |
| 13 Q13 | Idempotency scope per client | decision, no code | - |
| 13 Q14 | Retention window and default | `crates/js-lease-server/src/config.rs:327` | - |
| 13 Q17 | Zero-uncovered ratchet vs percentage gate | superseded: AGENTS.md 'Coverage here is the ratchet in coverage-baseline' | - |
| 13 Q18 | Linux arm64 planned or dropped | `.github/workflows/artifacts.yml:72-92` | - |
| 13 Q21 | Quota check before a slow launch | partly: `crates/js-provider-ec2/src/api.rs:29` | - |
| 13 Q23 | Agentless static pool removal | `docs/design/configuration.md:38`, `crates/js-pool-fake` | - |
| 13 Q24 | Reservations of capacity | decision, no code | - |
| 13 Q25 | silence_limit for a host silent for good | decision, no code | - |
| 13 Q26, Q27 | Log retention lifecycle; live log coverage over 64 MiB | decision, no code | - |
| 13 Q28 | Object stores without AssumeRole: presigned_prefix | `crates/js-lease-server/src/file/logs.rs:8` | - |

## pools-providers-network.md

As read: Header says "draft for review", written against 73132d5f7. It has no per-item Designed or Built tags. Status lives in section 12 (PR table plus a "Later" list), section 14 (Open questions) and scattered "not built", "Later", "PROPOSAL" and "not yet" phrases. Most of the PR table is built and the text was kept current, but the intro and the "Later" list are stale in places (the failure brake is built).

| Section | Claim | On main | Tracking |
|---|---|---|---|
| 12 PR 12 / 6.5 / Q18 / Q20 | Several registering or elastic pools in one server, with several elastic pools sharing one registry. | partly: `crates/js-lease-server/src/agents.rs`, `crates/js-sched/src/acquire.rs:1` | komira-ai/jobscheduler#678; komira-ai/jobscheduler#837; komira-ai/jobscheduler#906 |
| 12 PR 12 / 6.5 | Part-host classes: first-fit pack of pending demand onto one new host, and status showing no_offer with the failing Dimension per waiting request. | superseded: agent_pool.rs:88-89 and file.rs:878-891 still refuse a mixed pool with 'not served by this server yet'. But hosts are now booked at the capacity they report and | komira-ai/jobscheduler#808; komira-ai/jobscheduler#1100 |
| 12 Later / 1 / 6.4 / 8 (list) | Boot-failure counting and the failure brake. | `crates/js-scaler/src/brake.rs:1-46`, `crates/js-lease-server/src/brake_api.rs` | - |
| 12 Later / 6.2 | Per-offer suppression with doubling backoff. | partly: `crates/js-elastic/src/cache.rs:195-240`, `crates/js-lease-server/src/catalogs.rs:136-140` | - |
| 12 Later | Price-drift marking. | no | - |
| 12 Later / 9 / 6.4 | zeroize on drop, RLIMIT_CORE 0 and a panic hook that strips payloads for the service. | no | - |
| 12 Later / 1 | A separate js-network crate. | no | - |
| 12 Later / 6.4 / Q8 | release_orphans: a pool setting that releases orphans automatically. | no | - |
| 12 Later / Q19 / 2 | The exec kind, for out-of-tree providers. | no | - |
| 12 Later / Q19 | A script network mode. | no | - |
| 12 Later / Q12 | A public scheduler listener with a full in-service client-auth story. | no | - |
| 3 end (disk heartbeat) | The registry acts on the heartbeat's disk_gib: a host that fills up stops booking disk promises. | partly: `crates/js-agent-wire/src/heartbeat.rs:268-273`, `crates/js-lease-server/src` | komira-ai/jobscheduler#328 |
| 1.1 / 5.1 / 3.2 | A launch_template key for ec2. | no | - |
| 5.1 | Further provider kinds beyond lambdalabs, ec2, gce (azure, k8s). | partly: ec2 and gce are built (kinds.rs:175-177). No azure or k8s kind. providers.md:17 names them as later. | - |
| 7.3 step 6 | A pool-level setting for the image pull policy (instead of the fixed --container-pull missing). | no | - |
| 13.3 end | Pool status lists machines. | `crates/js-lease-server/src/status.rs:104` | - |
| Q1 / PR 10 | Server flags move into [server]; other flags refused beside --config. | `crates/js-lease-server/src/flags.rs:1-90` | - |
| PR 10 / Q17 | Amend providers.md K6, registration.md and configuration.md, and fix the stale --ca-cert comment in js-agent/cli.rs. | yes: cli.rs:83-88 now says platform roots are not used and bundled roots are. registration.md:33,65 describes user data delivery. providers.md:94. configuration.md:9. | komira-ai/jobscheduler#344; komira-ai/jobscheduler#1126 |
| Q3 | The service creates an ssh key `js-<scheduler>` from a throwaway key when the account has none. | `crates/js-provider-lambdalabs/src/provider.rs:306-335` | - |
| Q4 / PR 7b | Host credential after first hello; join token retired. | `crates/js-agent-wire/src/credential.rs:33`, `crates/js-lease-server/src/host_cert.rs` | - |
| Q23 / PR 4 | Required agent_listen, serving only agent routes. | `crates/js-lease-server/src/file/server.rs:32` | - |
| Q22 / 8.3 | A policy-read warning in check(): warn when the Tailscale policy does not limit the tag to the agent port. | no | - |
| Q9 | A lifetime total per pool, beyond the day window. | partly: max_spend_per_day is required | - |
| Q21 | Whether a reservation counts as covering demand however long its drain_deadline is, or only when the deadline is within boot_timeout. | decision, no code | - |
| Q2 / 5.3 | Lambda API field names for user_data and firewall rulesets, and the user_data size limit, taken from live reference. | partly: Fixtures with field-by-field parse tests and a dry-run-by-default live conformance suite (komira-ai/jobscheduler#769) | komira-ai/jobscheduler#769 |
| Q5 / 7.3 | Stock Lambda image facts: driver, runtime, toolkit, userns-remap with GPU containers, nft and python3 present, metadata address; state root on its own device. | partly: `crates/js-lease-server/src/bootstrap/` | komira-ai/jobscheduler#310 |
| Q10 / 13.4 / 12 (the go) | Human check of Lambda's account-wide firewall (inbound 22), and the operator's go: one launch with max_instances = 1 and the live checklist. | decision, no code | komira-ai/jobscheduler#310 |
| Q11 | Whether the 10-minute Tailscale key outlasts boot, measured live. | decision, no code | - |
| Q6, Q7, Q12-Q16, Q20 | Decisions on agent binary source (operator pin), one provider per pool, no status token, http over tailnet refused, defaults, price_unit cap, require_digest. | yes: Digest is required (the server refuses unpinned images) | - |
| 6.2 / 4.1 / 7.3 / 8 (PROPOSAL tags) | The PROPOSAL choices: the 4.1 example spends nothing as written; the floor; unknown_hold 30 min; bundled-roots catalogue; secret file format; root-disk handling in 7.3. | partly: unknown_hold is a setting (js-scaler/src/settings.rs). The launch default is plan. The [shape] floor was removed (komira-ai/jobscheduler#1100). The other proposals are not tracked separately. | - |

## job-rootfs.md

As read: Status: design for the owner's review; items 2-6 built (header). Stale: items 1, 7 (read-only/read-write part) and 8 (runbook) are also built; items 9-14 and the macOS copy-on-write/prepare remain unbuilt.

| Section | Claim | On main | Tracking |
|---|---|---|---|
| Status line | Header lists only items 2-6 as built; 1, 7 and 8 are not named | `crates/js-agent/src/native/mod.rs:100`, `crates/js-agent-wire/src/refusal.rs:193-206` | - |
| 11 item 1 | Retire the tree wire | `crates/js-agent-wire/src/refusal.rs:193-206` | - |
| 11 item 7 | macOS: links and profile built for READ_ONLY/READ_WRITE; native runtime prepare and chosen 5.3 candidate wait | partly: `crates/js-agent/src/native/mounts.rs` | komira-ai/jobscheduler#888 |
| 5 intro / 5.1 | macOS host_ports (job port equal to host port): not built, refused input-not-allowed | no | komira-ai/jobscheduler#1159 (new) |
| 5 intro / 5.1 | macOS native runtime's prepare for lookups | no | - |
| 5.3 / LR10 / Q1 | Copy-on-write mount on macOS: three candidates, none chosen | decision, no code | - |
| 5.4 / 5.1 | Host sockets on macOS (designed) | no | - |
| 11 item 8 | End-to-end run on one Linux node as a runbook | `docs/runbooks/fuse-layers-and-mounts.md` | - |
| 11 item 9 / 2.4 | tmpfs mounts (Mount.tmpfs, mount-tmpfs, mount-source-unsupported-here on macOS) | no | - |
| 11 item 10 / 2.4 | Image mounts (Mount.image, mount-image) | no | - |
| 11 item 11 / 2.4 | UDP host ports (PORT_PROTOCOL_UDP, host-port-udp) | no | - |
| 11 item 12 / LR20 / 4.5 | Host sockets (JobSpec.host_sockets=33, HostSocket, --host-socket, host-sockets) | no | - |
| 11 item 13 / 2.4 | Receiving propagation (MOUNT_PROPAGATION_RECEIVE) | no | - |
| 11 item 14 / LR5 / 2.4 | Id-mapped mounts (Mount.id_mapping=15, IdMapping, --map-id, mount-idmap) | no | - |
| 13 Q1 | Which macOS copy-on-write candidate | decision, no code | - |
| 13 Q2 | Forwarder throughput measured; splice(2) if short, then host sockets | partly: `crates/js-agent`, `crates/js-agent/src` | - |
| 13 Q3 | Retry of a start that failed input-unavailable on another host | superseded: recent merged work on job retry: 360dd73df (komira-ai/jobscheduler#1104 placement honours backoff), fe2f5d63e (komira-ai/jobscheduler#1097 attempt store) | komira-ai/jobscheduler#1097; komira-ai/jobscheduler#1104 |

## job-retry-and-completion-callback.md

As read: Header line 4-5 says "Status: decided, not built ... Nothing here is built yet." That is stale: tasks A to G and J are merged on main, H is in open PR komira-ai/jobscheduler#1120, and only I and kbf are not built. Since then task H merged (komira-ai/jobscheduler#1120) and task I is built (komira-ai/jobscheduler#1217), which also updates the header; only the kbf row is left. The page has no "Open question" section and no "What is left" section; its open items are the task table in section 10, plus one "not built" aside in section 2.1 (eviction) and "Decisions this page makes" (owner may reverse).

| Section | Claim | On main | Tracking |
|---|---|---|---|
| header (lines 4-5) | Whole page is 'decided, not built; nothing here is built yet'. | not a gap: the header says built (komira-ai/jobscheduler#1217) | - |
| 10 task A. Interfaces | Proto fields, agreed Rust signatures, SubmitJob refuses retry and callback; js-callback added to layers.toml. | `crates/js-types/src/retry.rs:170`, `crates/js-jobs/src/store/` | komira-ai/jobscheduler#1089 |
| 10 task B. Retry decision | Pure classing table, job-fault refusal codes, backoff and jitter in js-types. | `crates/js-types/src/retry.rs:170` | komira-ai/jobscheduler#1093 |
| 10 task C. Attempt store | Tags 67 and 69-73, retry_txn, pre-run counter reset, history bound, attempt/ rows and sweep, UsageRecord.attempt. | `crates/js-jobs/src/store/attempts.rs:57` | komira-ai/jobscheduler#1097 |
| 10 task D. Callback store | callback/ row armed at submit, due at end_txn, claim, answer, expire, key erasure, boot raise, sweep. | `crates/js-jobs/src/store/callbacks.rs:221` | komira-ai/jobscheduler#1095 |
| 10 task E. Delivery | Standard Webhooks signer, address rule with resolver argument, ureq sender (no proxy, no redirects, no body read, time limit), answer classes. | `crates/js-callback/src/sender.rs:34`, `crates/js-callback/src/sign.rs:89` | komira-ai/jobscheduler#1096 |
| 10 task J. Attempt logs | Attempt log prefix and length rule; GetLogsRequest.attempt; reader of an earlier attempt from attempt_history or attempt/ row. | `crates/js-lease-server/src/job_api/logs_attempt_tests.rs` | komira-ai/jobscheduler#1094 |
| 10 task F. Server retry (act) | end_of and the loss paths ask decide; placement honours retry_not_before, the 15-minute bar, start-fence, the freed previous allocation; queue wait counts from backoff end; API mapping; metrics. | `crates/js-lease-server/src/job_retry.rs:98`, `crates/js-lease-server/src/metrics/golden.prom:45` | komira-ai/jobscheduler#1104 |
| 10 task G. Server callback (act) | Submit checks, delivery threads, callback_status, key redaction, metrics; also the heartbeat peer address kept in memory for the address rule (section 5.3). | `crates/js-lease-server/src/job_api/callback_check.rs`, `crates/js-lease-server/src/callbacks.rs:140` | komira-ai/jobscheduler#1103 |
| 10 task H. Simulation and conformance | Section 9 invariants and scenarios in js-sim-cell; reference service and scenarios (retry_on_host_loss, retry_on_start_failure, no_retry_on_nonzero_exit, no_retry_on_cancel, callback_once_after_retries) in js-conformance. | `crates/js-sim-cell/tests/job_retry.rs`, `crates/js-conformance/src/jobs/retry.rs` | komira-ai/jobscheduler#1120 |
| 10 task I. Accept (part 1: turn the gates on) | A release after F, G and J removes the two refusals (retry-not-enabled, callback-not-enabled) so SubmitJob takes a retry policy and a callback. | `crates/js-lease-server/src/job_api/validate.rs`, `crates/js-lease-server/src/job_api/callback_tests.rs` | komira-ai/jobscheduler#1217 |
| 10 task I. Accept (part 2: js-client reports a dropped policy) | js-client reports that the server dropped a retry policy or callback (test a_dropped_policy_is_reported; section 6, line 625). | `crates/js-client/src/jobs/dropped.rs` | komira-ai/jobscheduler#1217 |
| 10 task I. Accept (part 3: doc updates) | Update design.md 8.1, retries-and-runs.md, queue-status.md, live-logs.md (attempt segment), http-gateway.md and jobscheduler-design.md (3.2, 5.3). | `docs/design.md`, `docs/design/live-logs.md`, `docs/http-gateway.md`, `docs/design/jobscheduler-design.md` | komira-ai/jobscheduler#1217 |
| 10 kbf row (komira-ai/kbf) | kbf-job-client reads the new fields and goldens; kbf takes the callback with its own key to wake the executor; kbf sets no retry policy. | not a gap: The kbf row is work in komira-ai/kbf (kbf-job-client, kbf-server) | - |
| 2.1 Older RetryPolicy: eviction | Eviction (an evicted attempt requeued without charging max_attempts, with its own outcome value) is not built; D63. | no | komira-ai/jobscheduler#1150 (new) |
| Decisions this page makes (owner may reverse any) | The choices listed (attempt is a placement; the retry keeps the job id; one allocation at a time; 15-minute bar; history of 256; hash segment; jitter from job id; key only in the callback row; private addresses allowed) are provisional. | decision, no code | - |
| 0. Decided (owner questions) | The page has no remaining open question. Section 0 lists the API owner's nine answers (C1 and 1-9). | decision, no code | - |

## cpu-over-time.md

As read: Status: design for review; header says replay, step 1 and step 2 built, steps 3 and 4 built as placement-profiles-live.md revised them behind --placement-curves on (header is partly wrong: the decide-in-shadow part of step 3 is not built for CPU; its memory half, the memory shadow, is).

| Section | Claim | On main | Tracking |
|---|---|---|---|
| Status line / Build step 1 Record | Agent records per-job CPU series, host busy and pressure per second, agent run length; wire, store and GET usage carry it. | `crates/js-agent-wire/src/per_job.rs:470`, `crates/js-jobs/src/codec/result_fields.rs:107` | komira-ai/jobscheduler#1037 |
| Build step 2 Learn, in shadow | Per-class CPU and memory profiles learned, self-check logged per run, js-cpu-replay shadow reads the log. | `crates/js-sched-placer/src/sizes_profile.rs:42`, `crates/js-cpu-replay/src/cli.rs:62` | komira-ai/jobscheduler#1057; komira-ai/jobscheduler#1077; komira-ai/jobscheduler#1083 |
| Build step 3 Decide in shadow (replay gate) | Log what the curve rule would have done beside the flat rule, so the replay gate can be judged; no decide-in-shadow line exists for CPU. The memory half is built: the memory shadow (placement-profiles-live.md section 2). | partly: `crates/js-sched-placer/src/memory_shadow.rs` | komira-ai/jobscheduler#1143 (new) |
| Build step 4 Enforce | Curve rule books curves and packs slot by slot on named curve hosts; off returns to today's rule. | `crates/js-sched-placer/src/curve_room.rs` | komira-ai/jobscheduler#1087 |
| Build step 5 / Section 11 horizon of minutes | A horizon beyond 128 s, only if measured worth it (share of CPU after 128 s in bursts). | no | - |
| Build step 5 / Section 11 credit for jobs about to end | Credit for jobs about to end, decided by whether the step 3 shadow shows flat sum over-counting hosts. | no | - |
| Section 11 delay on purpose; spread term | Delay a job on purpose (perfect-curve arm) and a spread term (step 3), only if measured. | no | - |
| Decision 1 Approve step 1 | Owner approval of step 1. | superseded: Step 1 is built and merged (komira-ai/jobscheduler#1037). | komira-ai/jobscheduler#1037 |
| Decision 2 Success measure / pass mark | Total time success measure; pass mark 10% less total time vs today's rule, throughput within 2%; stop if prize smaller. | decision, no code | - |
| Decision 3 Keep flat ceiling beside the curve rule | Curve rule only holds a job, never admits more. | superseded: Page itself says reversed by placement-profiles-live.md s7; engine.rs/curve_room.rs: on a curve host the curve rule replaces flat CPU checks. | komira-ai/jobscheduler#1087 |
| Decision 4 Hold bound 30 s | A job held on a forecast at most 30 s, then today's rule decides. | `crates/js-lease-server/src/config/placement_curves.rs` | komira-ai/jobscheduler#1087 |
| Decision 5 Gate on the replay | Gate on the replay and stop if shape does not pay. | partly: Replay curve arm exists (docs line 'arm A median, curves packed' in js-cpu-replay) but on synthesised series; trace still carries no series; no held-out-day run. | komira-ai/jobscheduler#1144 (new) |
| Section 3 Open question: keep first 128 s at 1 s for every job (second series past 128 s) | Send a second series so long jobs keep 1 s detail for the first 128 s; recommended, 514 B. | no | - |
| Section 3 Wire: heartbeat per-second host arrays | Heartbeat carries whole-host busy cores and cpu.pressure per second. | `crates/js-agent/src/lib.rs:104`, `crates/js-lease-server/src/hosts.rs:172-201` | komira-ai/jobscheduler#1037 |
| Section 8 unknown 1 Learning from held and contended runs; step 1 reports share of clean runs per class | Report the share of clean runs per class. | partly: `crates/js-cpu-replay/src/shadow.rs:25-29` | - |
| Section 8 unknown 2 Start-time lag | Placement-to-start lag not in usage export; rows hold it. | partly: `crates/js-cpu-replay/src/cli.rs:41` | - |
| Section 8 unknown 3 Whether jobs are phased at all | Step 1's series answers it. | partly: Series recorded (step 1) and shadow output shows 8.4% of classes CPU-shaped (page line ~361); no further analysis in code. | - |
| Section 8 unknown 4 Class count (replay stand-in classes) | convert --rows reads real class keys. | `crates/js-cpu-replay/src/cli.rs:34-44`, `crates/js-cpu-replay/src/rows.rs` | komira-ai/jobscheduler#1077 |
| Section 10 Next: convert --rows then step 1 series, learn on two days and replay a held-out third | Replay on real recorded series with two-day learn and held-out day; the pass mark check. | partly: convert --rows built (cli.rs:41); trace format still has no series per live doc line 6 ('the trace still carries no series'); no held-out-day mode found in js-cpu-replay/src. | komira-ai/jobscheduler#1144 (new) |
| Section 10 First check not yet met (replay waits 2.3x shorter than recorded) | Replay's own waits within 20% of recorded waits. | no | - |
| Section 10 With readings or without: rerun with recorded readings and measured-load baseline | Re-run comparison once komira-ai/jobscheduler#1036 readings flow. | no | - |
| Section 10 Kept Rust tests for the curve rule when built | Tests: flat equals flat rule; burst-first/quiet-first fit; empty host fits any job; B_h never negative and fades; each release reads the other's results. | `crates/js-sched-placer/src/curve_room_tests.rs`, `crates/js-lease-server/tests/server/placement_curves.rs` | komira-ai/jobscheduler#1087 |
| Section 3 Guard: server ignores series whose CPU differs from cpu_usage_usec by more than 5% | Series total checked against exact CPU time. | `crates/js-sched-placer/src/sizes_profile.rs:210-213` | komira-ai/jobscheduler#1057 |
| Section 12 Mac hosts | Mac runtime reports usage and cpu-series so Mac classes learn. | `crates/js-agent/src/lib.rs:112` | komira-ai/jobscheduler#1037 |

## live-logs.md

As read: Status: "Planned, partly built." Body shows the parts of sections 3 to 8 built on main; design section 12.1 step 31 still says "Status: Planned" (stale). Only deferred or open items remain, and no live-logs work is tracked.

| Section | Claim | On main | Tracking |
|---|---|---|---|
| Status header | Whole feature is 'Planned, partly built'; lists wire fields, js-logstore, [logs] table, js-logmint (STS and prefix grant), server logs module, agent uploader for crun and macOS native, and GetLogs reader as built. | `crates/js-logstore/src/presigned.rs:241`, `crates/js-logmint` | - |
| §10 Crates (step 31 of design 12.1) | Crates and fields to build: js-logstore, js-logmint, js-agent-wire log fields, GetLogs fields in js-proto with comment rewritten, agent logs module, server job_api/logs and host credential table; each new crate in layers.toml. | `.github/workflows/ci.yml:244`, `crates/js-testkit/src/s3.rs` | - |
| design section 12.1 step 31 (cited by this page) | jobscheduler-design.md still says 'Status: Planned' for step 31 Live logs. | `docs/design/jobscheduler-design.md:1063` | - |
| §10 Later and open: Other runtimes (D6) - container-engine fallback | The container-engine fallback does not report log-upload, ignores a sink and sends no logs report; live logs for it are later work. | no | - |
| §10 Later and open: Other runtimes - native agent does not report per-job so native runs carry no sink | A native agent does not yet report per-job, so its job starts carry no log_sink until it does. | `crates/js-agent/src/contract.rs:150-156` | - |
| §10 Later and open: Who deletes old logs (D4) | Scheduler never deletes log objects; retention is the operator's bucket lifecycle rule. Whether the scheduler should delete is design question 26. | decision, no code | - |
| §10 Later and open: Live log coverage | A stream is live only up to --log-file-cap; alternatives are a larger default or a capture that frees uploaded bytes. Design question 27. | decision, no code | - |
| §10 Later and open: Presigned read URLs | GetLogs could return presigned read URLs instead of bytes; not chosen for v1. | no | - |
| §10 Later and open: The CLI's follow loop | The hand-written loop that calls GetLogs again with next_offset until eof and backs off on too-many-follows: js job follow and js job run, over js_client::jobs::follow_logs. | `crates/js-client/src/jobs/loops.rs:325`, `crates/js-cli/src/jobs/follow.rs:57` | - |
| §10 Later and open: Per-chunk timestamps lost in the final file | A final file has no first-byte times; keeping them (an index object beside the final) is later work if a client needs times after the end. | no | - |
| §10 Decided: object store without AssumeRole - tierstore side | Checking grants in tierstore, and testing against the real tierstore, are left to the tierstore side. | no | - |

## mac-account-per-job.md

As read: Status line says steps 1 and 3 built, step 2 designed and not yet landed. Stale: step 2 was built in a different shape by komira-ai/jobscheduler#1023 (slot accounts, fresh account per job) after komira-ai/jobscheduler#1014 was closed. The helper's pool (step 5) is still on main.

| Section | Claim | On main | Tracking |
|---|---|---|---|
| Status line, steps 1 and 3 | Signing, the Full Disk Access profile, the agent's own grant check and refusal of native jobs without it are built. | `crates/js-agent/src/native/grant.rs:1-60`, `crates/js-agent-wire/src/refusal.rs:349` | komira-ai/jobscheduler#1004; komira-ai/jobscheduler#1016 |
| Status line / Step 2 (whole) | A new account for every native job, deleted afterwards, lands in its own change once the grant is seen in force and step 4 is measured. | partly: `crates/js-agent/src/mac_accounts/mod.rs:1-120`, `crates/js-agent/src/native/accounts.rs:44-75` | komira-ai/jobscheduler#1014; komira-ai/jobscheduler#1023; komira-ai/jobscheduler#1032; komira-ai/jobscheduler#1034 |
| Step 2: The account | The account is hidden, has shell /usr/bin/false, no password and is never an administrator. Its primary group is the lease group and its home is made mode 0700. Its name is `kbf-lease-<term>-<seq>` and it is made and deleted by the helper's user-create and user-delete verbs. | partly: `crates/js-agent/src/mac_accounts/mod.rs:42-50`, `crates/js-agent/src/mac_accounts/make.rs` | komira-ai/jobscheduler#1023 |
| Step 2: Choosing a uid | The uid is the next free one after the last given, wrapping within the range. A uid is free only when no live lease, record, process or owned file remains. | `crates/js-agent/src/mac_accounts/uid.rs:5-24`, `crates/js-agent/src/mac_accounts/mod.rs:52-70` | komira-ai/jobscheduler#1023 |
| Step 2: After the job, steps 1-6 | After a job, kill-uid, IPC removal and the shared-place sweep run. Then the record and home are deleted and the proof shows them gone. Only then is the lease marked gone and the uid free. | partly: `crates/js-agent/src/mac_accounts/teardown.rs:175-200`, `crates/js-agent/src/mac_accounts/proof.rs` | komira-ai/jobscheduler#1023; komira-ai/jobscheduler#1106 |
| Step 2: ledger format | The ledger keeps its created/deleted format, a previous release can read it, and the pool file is no longer written. | superseded: `crates/js-agent/src/mac_accounts/ledger.rs` | komira-ai/jobscheduler#1023 |
| Step 2: A deletion that does not complete | A failed deletion holds the lease and uid, and the delete is retried at the next job's end and at start. A held account makes the agent admit one native job fewer. Two held accounts taint the host with account-not-deleted. A held uid is never reused silently. | partly: `crates/js-agent/src/native/start.rs:150-160` | - |
| Step 2: A crash between create and delete | Crash points are covered. Boot reset finds a live lease with no account and a recorded account whose record is gone, and the delete is repeated. An unknown kbf-lease-* account in the range is recorded and deleted. Old `kbf-lease-pool-<n>` accounts are deleted at the first boot with the grant. | partly: `crates/js-agent/src/mac_accounts/mod.rs:136-175`, `crates/js-agent/src/native/adopt.rs:1-15` | komira-ai/jobscheduler#1023 |
| Step 4: what it costs (measurements) | The page says step 2 waits for three answers: the time to make an account, a deletion that succeeds with the grant, and whether the grant lets root remove sunlnk /var/folders entries. | superseded: `crates/js-agent/src/mac_accounts/mod.rs:22-40` | komira-ai/jobscheduler#1023 |
| Step 4: accounts made ahead of time | A few accounts made ahead and each used once. Worth it only if making an account is a noticeable part of a short job's start; the default is none. | `crates/js-agent/src/mac_accounts/mod.rs:3-8`, `crates/js-agent/src/native/accounts.rs:24-31` | komira-ai/jobscheduler#1023 |
| Step 5: what goes with step 2 | The pool goes: its file, pool-create, pool-status, the slots, the adoption of left accounts, and the reset in place with its record write-back. The helper's full-disk-access verb goes with the socket. | no | komira-ai/jobscheduler#1163 (new) |
| Step 5: what stays | The Seatbelt profile, kill-uid and the judgment of what is left, the shared-place sweep and IPC removal, and the leak scan with its taint all stay. | `crates/js-agent/src/mac_accounts/sweep.rs`, `crates/js-agent/src/native/fence.rs` | komira-ai/jobscheduler#1023; komira-ai/jobscheduler#1029 |
| Step 6: the proof | The isolation probe's two-phase persistence half judges a new account per job, and every marker kind must come back gone. | `crates/js-isolation-probe`, `crates/js-agent/tests/native_isolation/lines.rs` | komira-ai/jobscheduler#1041; komira-ai/jobscheduler#1048; komira-ai/jobscheduler#1050 |
| Step 6: POSIX named semaphore row | One marker row stays open: a POSIX named semaphore outlives every account change until the Mac restarts, and the profile cannot deny it without breaking Python's multiprocessing locks. | decision, no code | - |
| Step 6: tests on Linux against fakes, and the root test on the hosted runner | Tests of the change run on Linux against the helper's fakes (account shape, uid choice, each step's failure, each crash point). A root test on the hosted runner proves the not-granted side. | `crates/js-agent/src/mac_accounts/tests/fake.rs`, `crates/js-mac-first-install/tests/mac_first_install.rs:15` | komira-ai/jobscheduler#1023; komira-ai/jobscheduler#1047 |
| Step 7: owner's steps 1-4 | Merge and release, upload the profile in Apple Business, restart the agent on each Mac and check the host list, then after step 2 releases, run one native job and look for its account by uid. | decision, no code | - |
| What changes for an operator, After step 2 column | With the grant, a native job runs as a new account deleted after the job (home, ~/Library/Preferences and per-user folders included) and shown gone before its uid is reused. | partly: `crates/js-agent/src/native/mod.rs:6-12`, `crates/js-agent/src/mac_accounts/mod.rs:22-40` | komira-ai/jobscheduler#1023 |

## mac-one-program.md

As read: Status: accepted 2026-11-22; header says "apart from the accounts of section 11, nothing here is built yet". Stale: sections 11-14 are built (accounts, supervisor, native runtime over accounts, put ingest). Sections 3-7 are partly superseded by 12-13 (D5 is marked superseded; adoption, hand-over and helper removal remain unbuilt).

| Section | Claim | On main | Tracking |
|---|---|---|---|
| Status header | Apart from section 11, nothing here is built yet. | no | komira-ai/jobscheduler#1193 (new) |
| 1 / header | The package ships one program (js-agent) and one launchd job. | no | komira-ai/jobscheduler#1162 (new) |
| 1 / 5 / 7 step 6 | An agent update with jobs running hands over on a Mac; the new agent adopts the runs. | no | komira-ai/jobscheduler#1161 (new) |
| 1 / 7 step 5 / 3 | Update hook on a Mac only stops and starts the agent (helper.rs and pf.rs steps, helper_unit, pf_unit, socket_group deleted). | no | komira-ai/jobscheduler#1162 (new) |
| 1 / 7 step 5 | Agent plist has no --native-helper-socket; native jobs are switched on by --native-accounts in the flags file. | `packaging/js-agent-macos/README.md:41` | komira-ai/jobscheduler#1023; komira-ai/jobscheduler#1032 |
| 1 (per-job accounts box) | If accounts are made and deleted per job: supervisor never makes accounts, remove ends supervisor then kills uid then deletes account, ledger created/deleted records, reset at start keeps adopted accounts. | `crates/js-agent/src/mac_accounts/` | komira-ai/jobscheduler#1014; komira-ai/jobscheduler#1023 |
| 3 | Pool is renamed Pool and called in-process by NativeRuntime instead of a socket Client. | partly: `crates/js-agent/src/native/accounts.rs:22`, `crates/js-agent/Cargo.toml:39` | - |
| 3 | js-mac-session stops being a program: delete proto.rs, server.rs, client.rs, caller.rs, main.rs, socket binding, wire_golden tests. | no | komira-ai/jobscheduler#1163 (new) |
| 3 / 8 Q2 / 7 step 7 | Delete uncalled helper verbs (session-*, role-*, image-*, baseline, reboot-dirty, macos/gui.rs as-user). | no | komira-ai/jobscheduler#1164 (new) |
| 3 / 8 Q3 / 7 step 7 | Delete node-admin (node_admin.rs, phase0.rs, role_user.rs, macos/admin.rs). | yes: deleted with their tests; `crates/js-mac-session/src/main.rs:7` takes no `node-admin` mode, and `crates/js-mac-session/tests/binary.rs` holds the word refused as an argument. | komira-ai/jobscheduler#1165 |
| 3 | Administrator lease (grant.rs, grant keys, user-create grant, pool replace-by-admin path) removed by a separate change before the switch. | yes: PR komira-ai/jobscheduler#1003 merged 'Mac pool: no lease user is ever an administrator; the admin grant, its keys and set_admin are removed'. | komira-ai/jobscheduler#1003 |
| 3 | js-mac-provision program deleted (main.rs, verb dispatch); plist verbs move to cargo xtask pkg; flags-file reader moves to library flags.rs which the agent calls at start; operator verbs check/remove/render/probe dropped; pf_as_root.rs test calls the library. | no | komira-ai/jobscheduler#1162 (new) |
| 3 | Pool users are permanent: reset empties in place, replace becomes attribute reset (a change to the old pool that lands on its own). | superseded: `crates/js-agent/src/mac_accounts/teardown.rs` | komira-ai/jobscheduler#1023 |
| 3 / 4.1 / 7 step 3 | js-supervise crate (layer runtime) takes the platform-free core out of oci/supervise.rs; layers.toml updated; macOS SupervisorId from proc_pidinfo. | `crates/js-supervise/src/`, `crates/js-contain/src/oci/supervise.rs` | komira-ai/jobscheduler#996 |
| 4.2 / 7 step 4 | macOS supervise arm: js-agent __supervise-native `<state-dir>` `<lease>` `<uid>` ... -- `<argv>`, root supervisor, own process group, holds the deadline with no agent, per-lease lock file, agent-written supervisor file, tested against FakeHost. | superseded: `crates/js-agent/src/mac_supervisor/` | komira-ai/jobscheduler#1030 |
| 4.2 Agent's side | Agent adopts running supervisors after a restart (journal supervisor {pid,start}, adopt on live supervisor or exit record, reset at start skips adopted leases, supervisor file for an unjournaled supervisor). | no | komira-ai/jobscheduler#1160 (new) |
| 4.2 One process at a time (D3) | Agent holds an exclusive flock on ledger.lock for its whole life; a second agent fails to start. | no | - |
| 4.2 Privacy attribution | Agent holds Full Disk Access; agent, supervisor and job all keep responsibility (Kept::Supervisor / Kept::Job); spawn_sites KEEPERS holds this to two files. | `crates/xtask/src/spawn_sites.rs:94`, `packaging/js-agent-macos/mdm/js-agent-full-disk-access.mobileconfig` | komira-ai/jobscheduler#1016; komira-ai/jobscheduler#1021 |
| 4.2 Not decided | How to close the Seatbelt fence gap (job holding the agent's FDA reads TCC-only places). | partly: `crates/js-agent/src/native/fence.rs` | komira-ai/jobscheduler#1025 |
| 4.2 Not decided | VM launcher (js-vmm) stays disclaimed or keeps responsibility. | decision, no code | - |
| 4.2 Not decided | GUI-session path (launchctl asuser, disclaim::exec) stays disclaimed; not measured. | decision, no code | - |
| 4.2 Not measured | What macOS does with the responsible process when the agent that started a supervisor exits. | decision, no code | - |
| 4.3 / 7 step 1 | Root probe of launchd in the system domain, with result recorded in the document. | yes: Status header says the probe has held in the system domain. Section 4.3 body still reads 'Not proven yet' and records no result. | komira-ai/jobscheduler#997 |
| 7 step 1 | Add a probe to js-mac-probe that runs on the beta macOS runner: a process group under a launchd job survives bootout with parent pid 1. | no | komira-ai/jobscheduler#1166 (new) |
| 5 / 7 step 5 | Cutover with one launchd job: write handover file, boot out js-agent, start staged agent; rollback same three steps swapped; agent applies pf anchor itself. | no | komira-ai/jobscheduler#1162 (new) |
| 5 | The switch release's package drops js-agent.session.plist and js-agent.pf-pool.plist; cutover boots both out and removes their plists; rollback to a two-daemon release bootstraps them back. | no | komira-ai/jobscheduler#1162 (new) |
| 5 / 7 step 6 | hands_over gains a launchd-plus-native case gated by a native-handover word on both agents' stops: line; steps_handover.rs launchd cases. | no | komira-ai/jobscheduler#1161 (new) |
| 6 / 7 step 2 | pf anchor applied by the agent at start (install --flags-file call), reloaded every 30 s on a thread, first NotReferenced failure retried, jobs refused until in force; pf enable token kept in `<state-dir>/pf-token.` | superseded: PR komira-ai/jobscheduler#1010 closure comment (2026-11-22): a native job's network is limited by its Seatbelt profile, so the agent does not load a pf anchor | komira-ai/jobscheduler#1193 (new) |
| 6 | Operator verbs check/remove/render/probe of js-mac-provision dropped; pf_as_root.rs and the privileged-tests workflow call the library. | superseded: PR komira-ai/jobscheduler#1010 closed as superseded (agent does not load a pf anchor; Seatbelt profile limits network); step 5 of the design still pending, with | - |
| 7 step 5 | Switch: NativeRuntime calls the pool in-process and starts every run through a supervisor. | yes: native/start.rs start_steps (account-lent, supervisor-started, job-ready) per s13; mac_supervisor/launch.rs SystemLauncher; no --native-helper-socket. | komira-ai/jobscheduler#1023; komira-ai/jobscheduler#1030; komira-ai/jobscheduler#1032 |
| 7 step 5 | Adoption reads exit records; boot_reset gains the adopted list; agent locks the ledger. | no | komira-ai/jobscheduler#1160 (new) |
| 7 step 5 | Package drops js-mac-session and the helper's plist; socket, protocol, client and caller check deleted; workflows js-agent-macos-pkg.yml, js-agent-native-privileged-tests.yml, js-mac-provision-privileged-tests.yml change; docs mac-runtime.md, spawn-disclaim.md, mac-lease-teardown.md, mac-hostkeeper-package.md, design.md, packaging README updated. | no | komira-ai/jobscheduler#1163 (new) |
| 7 step 5 tests | fake_helper.rs (772 lines) removed; runtime tests run over a pool with a fake host; kept tests renamed (adopt.rs, bound.rs, journal.rs); new tests (restart keeps adopted users, lost journal lease reset, supervisor without = line ended, second agent fails, journal-reader and exit-record cross-version). | partly: `crates/js-agent/tests/agent/native/fake_accounts.rs` | komira-ai/jobscheduler#1160 (new) |
| 7 step 5 | Real-macOS restart test on the beta stage: install the package, start a long native job, boot out and bootstrap js-agent, assert the job ran on, the new agent lists the run, exit reported once; holds back promote-macos. | no | komira-ai/jobscheduler#1160 (new) |
| 7 step 6 | Hand-over on a Mac: stops: native-handover in macOS --version; launchd cases in steps_handover.rs; second real-macOS case for an update with hand-over. | no | komira-ai/jobscheduler#1161 (new) |
| 7 step 7 | If owner agrees (Q2, Q3): delete uncalled verbs and node-admin, each its own PR. | partly: node-admin deleted (`crates/js-mac-session/src/main.rs:7`); the uncalled verbs not | komira-ai/jobscheduler#1164 (new); komira-ai/jobscheduler#1165 |
| 7 (end) | The fixes of komira-ai/jobscheduler#988 (open) must be released before step 5 reaches them. | yes: komira-ai/jobscheduler#988 is MERGED ('js-agent on a Mac: the update hook compares the release link by real path ... first-install test'). | komira-ai/jobscheduler#988 |
| 8 Q1 | One program on a Mac with a root supervisor per run keeping runs alive across restarts. | partly: Single program in the run path (native runtime, supervisor verb), but the helper still ships and runs is not kept across restarts (native/adopt.rs:1-12). | komira-ai/jobscheduler#1160 (new) |
| 8 Q4 | A Mac run's deadline is held by its supervisor while no agent runs. | superseded: s12: supervisor 'holds no deadline: the lifecycle's wall limit stops the job'; s13 'no other process holds a deadline'. | - |
| 8 Q5 | The switch release and a rollback across it drain each Mac once; hand-over starts the release after. | decision, no code | - |
| 9 item 4 | The real-macOS restart test holds back promote-macos. | no | komira-ai/jobscheduler#1160 (new) |
| 11 | Slot accounts (Pool, ledger, DELETE_ROUNDS) are built; __account-cycle goes once the native runtime proves the cycle itself. | partly: `crates/js-agent/src/mac_accounts/cycle.rs:72` | komira-ai/jobscheduler#1106; komira-ai/jobscheduler#1121 |
| 13 | Not built yet: adoption across an agent update, the retirement of js-mac-session's pool and the helper program. | no | komira-ai/jobscheduler#1160 (new) |
| 13 Checks | A Mac agent runs no post-job leak scan (NoChecks) because the helper's baseline is not recorded. | no | - |
| 14 | The run's result carries no put key; a result field comes with the build service's change for declared outputs. | no | - |

## kbf-on-js-agent.md

As read: Header (line 3): "Status: design only, DRAFT for the owner. Nothing here is built or approved to run." Marked "Partly superseded" by docs/design/jobscheduler-design.md:53 (kbf's side of the integration; lease-then-run replaced by the job service). It is a draft with no approved build plan. Many js-side pieces were built under other pages' designs (RunFile, egress, bundle, Mac native, VM backend, held beat). The kbf-side pieces are not in this repo.

| Section | Claim | On main | Tracking |
|---|---|---|---|
| Header / s1 / s15 D1-D9 | Whole page is a draft: nothing built or approved to run; D3-D9 remain owner decisions. | partly: `docs/design/jobscheduler-design.md:53` | komira-ai/jobscheduler#1936 |
| s1 item 1 / I20 / s13 C2 | kbf leases whole hosts from a deployed lease server and submits each action as a run via POST/GET/DELETE /v1/leases/{id}/runs; kbf-lease-client and executor module plan. | superseded: `crates/js-lease-server/src/lib.rs:471` | - |
| R1/R3/I4/s7 'New js primitives': JobSpec.upload | Additive JobSpec.upload: `Option<UploadGrant>` carrying slots with presigned PUT URLs, redacted in Debug. | no | komira-ai/jobscheduler#355 |
| R3/I4/s6.3 item 1: delivery of grants as 0400 file on per-run tmpfs; secrets field | js-agent writes per-run secret files for the task on a per-run tmpfs, never env or args. | `crates/js-agent-wire/src/command.rs:252`, `crates/js-agent-wire/src/files.rs:1-20` | - |
| R3/I4: network profile and host firewall for the task | Per-run network profile limiting task egress to store and read listener, enforced by host nftables, metadata dropped, fail-closed check before start. | partly: `crates/js-agent-wire/src/command.rs:291`, `crates/js-agent/src/egress/allow.rs:23` | - |
| I4/s5 S5/I19: kbf-tools read-only mount and the run's args | Run's args are the kbf-task path from a read-only agent root `kbf-tools` mounted at /mnt/kbf-tools. | `crates/js-agent/src/cli.rs:125-129`, `crates/js-agent-wire/src/command.rs:130-158` | - |
| s5 I4 / s7 / s13 C2: entrypoint override | An image with an ENTRYPOINT is refused until js adds an entrypoint override; run args are the tool path. | `crates/js-agent/src/crun/make.rs:90` | - |
| s7 S5 / s8 / s13 C2: per-run uid | Container runs as a distinct uid per run instead of shared 65534; agent kills the uid's processes and verifies none remain before removing the tmpfs. | partly: `crates/js-agent/src/cli_runtime/args.rs:389-392`, `crates/js-agent/src/crun/mod.rs:1-4` | - |
| s7 S3 / I3 / s14 S2: held (long-poll) heartbeat reply | A held heartbeat reply so start and result do not wait a full heartbeat; additive option. | `crates/js-lease-server/src/held_beat.rs`, `docs/design/held-beat.md` | komira-ai/jobscheduler#903 |
| R14 / s1 Memory decided | One memory setting memory_schedulable_percent (90) is the only memory knob; page says it is not in jobscheduler. | `crates/js-lease-server/src/lib.rs:177`, `crates/js-lease-server/src/config/agent_setup.rs:83-102` | - |
| s6.3 item 6 / s12 S6 / s13 C2: js-pack-format crate | kbf-segments pack format moves to jobscheduler as js-pack-format holding UploadGrant, PackReport etc.; two-PR rule. | no | komira-ai/jobscheduler#355 |
| s6.3 items 1-5 / I7-I11: task upload path (kbf-task, presigned PUT, CommitPack, minter, read listener, GC rule, Role::Ingest) | kbf-side write path: kbf-task tool, minter, CommitPack variant, read listener, cas/b/ GC. | no | komira-ai/jobscheduler#1031; komira-ai/jobscheduler#1922; komira-ai/jobscheduler#1934; komira-ai/jobscheduler#1936 |
| s6.9 item 7 / s10.2 / Q21: Mac output handover | Mac handover directory, agent-side uploader and relay conflict with the 12:30Z ruling; Mac side to resolve (Q21). | partly: `crates/js-agent/src/mac_put/mod.rs:1-25` | komira-ai/jobscheduler#1031 |
| s10.2 / D3 / s10.6: Mac sandbox runtime (sandbox-exec + per-lease user) | A Mac pool with a sandbox runtime; Macs stay on kbf-daemon until it exists. | `crates/js-agent/src/native`, `crates/js-mac-pool` | komira-ai/jobscheduler#888; komira-ai/jobscheduler#914 |
| s10.2 / s10.7 V2 / Q21 VM: Mac VM runtime | VmRuntime in js-agent, two VMs per host, no network device so cannot PUT. | `crates/js-agent/src/vm/mod.rs:1-25`, `crates/js-vmm/src/spec.rs:36` | komira-ai/jobscheduler#740 |
| s10.3: OCI bundle packaging for Mac (JobSpec.bundle, one darwin manifest digest, index refusal, trust, extraction) | Digest-pinned OCI task bundle, index refused, root-owned store, allow list, hostile-input extraction. | `crates/js-agent-wire/src/command.rs:278`, `crates/js-oci/src/index.rs:185` | komira-ai/jobscheduler#463; komira-ai/jobscheduler#992 |
| s10.3: Mac VM base artifact in a private registry; Xcode in VM; registry hosting | VM base as its own OCI artifact type, Xcode staged to a host-local derived disk; hosting is owner question C1. | partly: `crates/js-vmm/src`, `crates/js-agent/src/vm/mod.rs:5-8` | - |
| s10.2 row Network / D3: pf allow-list for a credential-bearing sandbox task | pf anchor limiting a Mac sandbox task's egress is a precondition before a sandbox lease takes a grant. | `crates/js-mac-provision`, `crates/js-mac-pool` | komira-ai/jobscheduler#407; komira-ai/jobscheduler#465 |
| s10.7 V1: VM probes P0-P9 and boot cost S7 | Probes of a VM from a launch daemon's child with nobody logged in and restore-from-snapshot. | partly: `crates/js-isolation-probe`, `crates/js-mac-probe` | komira-ai/jobscheduler#1019; komira-ai/jobscheduler#1066 |
| s11 Firecracker (Linux, later) | Same OCI image in a Firecracker microVM; needs KVM, rootfs conversion, vsock grant delivery, allow-listed tap. | no | - |
| s4.4 / s12: js-agent .deb, unit, described service in updater | Package and unit for js-agent; .deb is jobscheduler#300; kbf#1826 described services; no unit in tree. | `docs/release.md:56` | komira-ai/jobscheduler#771; komira-ai/jobscheduler#841; komira-ai/jobscheduler#898 |
| s12: js-agent --version --json | Updater needs `--version --json` from js-agent. | no | - |
| s12 One js-agent host / s11 Drain / R10 / s13 W2: cordon, wait for zero runs, Quiesce | Update waits for zero runs; agent has no Quiesce so benchmark nodes stay on daemon. | `crates/js-agent/src/worker/quiesce.rs:26`, `crates/js-agent/src/exclusive.rs` | komira-ai/jobscheduler#947; komira-ai/jobscheduler#972 |
| s12 skew rules S2, S3, S4, S7, S8: tests and image capability in Hello | Skew test newest agent vs pinned server; capability gating; image capability in Hello (S4 NEW); N-1 adoption test (S7 NEW). | partly: `crates/js-agent-wire/tests/one_job_previous_release.rs`, `crates/js-agent-wire/src/caps.rs` | - |
| s12 Signer trust D4 / K36 | O2 (kbf republishes agent under its own signer) required before production install; jobscheduler needs rulesets/branch protection. | partly: `docs/release.md:721`, `docs/ci.md:24` | komira-ai/jobscheduler#1115 |
| s17 'Done in the same PR': docs rewrites | rollout.md rewritten; decisions.md/design.md D42-D45 and 5.4/9.2-9.4, extraction steps 21a/34, risks K35/K36 marked superseded; kbf-today.md corrections. | `docs/design/decisions.md:187`, `docs/design.md:698` | komira-ai/jobscheduler#338 |
| s17 'Still to do': docs/release.md gamma step | release.md gamma step sets js-server then js-agent; asset names must match helper convention. | superseded: `docs/release.md:107-108` | - |
| s17 'Still to do': Mac design rev 2, object-store rev 7 App A, storage design, wire spec section 6, Mac packaging design | Companion docs (not in this repo) to be updated by their authors. | decision, no code | - |
| s17 In the kbf pull request: updates.md:28 and ARCHITECTURE.md:210 | kbf doc fixes. | no | - |
| s3 / s6.5-6.6 / A1-A3 / s12: tierstore, Silo, presign-only role, TLS, streaming PUT, reconciler | Finish tierstore (presign-only role, size cap, TLS or declared posture, streaming PUT, packaging); Silo hot tier; reconciler. | no | komira-ai/jobscheduler#1920; komira-ai/jobscheduler#1922 |
| s13 C0, C4, C5, C6, C7 / A4-A8 / F1-F2 | Migration steps: conformance targets, storage/daemon split on a canary, idle agent on canary, echo tasks through the full path, per-instance AC namespace, A4 canary through A8, F1/F2 pod removal. | no | - |
| s13 C1 / W1 / W3: doc changes and switching nodes | C1 land doc changes (done above); W1 switch non-benchmark nodes to --executor js-agent; W3 worker-only and elastic hosts. | partly: `crates/js-elastic` | komira-ai/jobscheduler#338 |
| s4.5 / R8 / D9 / s14 S9: elastic Lambda host join sequence and provisioner home | Scaler, provider, join routes; first live launch; throughput over tailnet measured. | `crates/js-lease-server/src/scaler`, `crates/js-scaler` | komira-ai/jobscheduler#267; komira-ai/jobscheduler#387; komira-ai/jobscheduler#388; komira-ai/jobscheduler#413 |
| R6 / Q2: js-agent config file | Does js-agent need a config file (flags only today)? | `crates/js-agent/src/flags_file.rs`, `crates/js-agent/src/lib.rs:87` | komira-ai/jobscheduler#1060 |
| Q3 ucdk Jobs sharing a host with load-placed tasks | How booked ucdk Jobs share a host with load-placed kbf tasks. | superseded: `docs/design/decisions.md:21` | - |
| Q4 / s12 image supply: pre-pull of action images by digest | Process that pre-pulls digest-pinned images (--container-pull never) is open; is kbf-pack enabled. | partly: `crates/js-agent/src/lifecycle/machine/provided.rs`, `crates/js-oci` | komira-ai/jobscheduler#486; komira-ai/jobscheduler#552 |
| Q6 layering: may js-agent-wire, js-pack-format, kbf-lease-client enter kbf's graph | Whether the lease client and wire crates may enter kbf's graph under kbf's layering test. | decision, no code | - |
| Q7 / Q8 / S7: agent loses server; slot frees at exit or ResultAck | Does an agent that loses the server kill the container; does the host slot free at exit or at ResultAck. | partly: `crates/js-agent/src/lifecycle`, `docs/design/agent-wire.md` | komira-ai/jobscheduler#1104 |
| Q9 / D6 / s13 rule 2: benchmark lock, frozen sets, canary node, S10 limit | Define and own the benchmark lock; name a canary; set the S10 ratio limit. | decision, no code | - |
| Q10 / s10.5 / S7: Mac VM concurrency, boot time, snapshot restore, slot clean, OCI 1.1 artifact types | Mac numbers to measure. | partly: `crates/js-agent/src/vm/mod.rs`, `crates/js-vmm` | komira-ai/jobscheduler#1019; komira-ai/jobscheduler#1066 |
| Q11 owner of the jobscheduler design amendment | Who amends jobscheduler's design.md sections the page marks superseded. | `docs/design/jobscheduler-design.md:45-60` | - |
| Q12-Q13, Q16, Q19-Q20: Silo If-None-Match, SSD capacity, AC rebuild from footers, reconciler prefix filter, CommitPack headroom | Storage-side open questions. | decision, no code | - |
| Q15 embedded js-server state across a frontend restart | Whether embedded js-server-core placement state survives a frontend restart. | superseded: `crates/js-lease-server/src/store` | komira-ai/jobscheduler#791 |
| Q17 live Lambda launch go and budget | Live Lambda launch go and budget. | decision, no code | - |
| Q18 wire reports blobs not output tree | The upload report lacks tree paths, exec bits, symlinks and exit code source. | superseded: `crates/js-agent-wire/src/run.rs:60-62` | - |
| Q21 / s6.9 / s8: Mac under the 12:30Z shape (VM guest cannot PUT; pf allow-list; storage restates decision 3) | No-network VM guest cannot PUT; needs route or re-ruling. | partly: `crates/js-vmm/src/spec.rs:36`, `crates/js-agent/src/mac_put/mod.rs` | komira-ai/jobscheduler#1031 |
| Q22 / Q23: Mac equivalent of kbf-tools; how the lease server is released and updated | Mac bundle or VM base carries the task tool; lease server release path on the farm. | partly: `docs/release.md:109-111`, `docs/lease-server-update.md` | - |
| s14 S0-S10 cost stages and pass rule | Benchmark stages and a fail-closed pass rule per migration step. | no | - |
| s2 R11 / D5 / C7: storage under own uid; co-location of tasks and store credentials | Test that an action cannot read the store key; own uid for storage processes. | decision, no code | - |
| s2 R13 / s6.5 decision 10: per-instance action-cache namespace | Action-cache key must be instance-scoped before untrusted tenants share a farm; gate C7. | no | - |
| s9/S5/R10: operator tail of stdout/stderr (log capture) | Agent captures stdout and stderr for operator tail (NEW; today none). | `crates/js-agent/src/logs`, `docs/design/live-logs.md` | komira-ai/jobscheduler#689; komira-ai/jobscheduler#698; komira-ai/jobscheduler#747 |

## extraction-steps.md

As read: No status header and no Designed/Planned/Open wording: a plain list of ladder steps 0-22 (Phases 0, A, B, C) plus carried gates 10.4, 13.4, 13.6, 13.7. Written for the kbf workspace era. On main the jobscheduler repo exists with the merged crates (step 22 effectively done), and the kbf-side steps run in the kbf repo, not here.

| Section | Claim | On main | Tracking |
|---|---|---|---|
| step 0 | Golden traces: hand-written trace lines, golden hashes per scenario and seed, CI sweep | `crates/js-sched/tests/golden/trace-hashes.txt`, `crates/js-sched/tests/golden_file/mod.rs:1-25` | - |
| step 1 | Golden corpus of rows and wire messages from the last released binaries, and a cross-version harness skeleton | partly: `crates/js-agent-wire/tests/fixtures/v0.1.135/`, `crates/js-jobs/tests/golden` | - |
| step 2 | Doc corrections: kbf-sched queue persistence text, stale kbf-estimator sentence, failure-classes 'doubl' lines, worker.proto memory comments | superseded: `crates/js-sizing/src/lib.rs:1-12` | - |
| step 3 | Split kbf-types into a generic module and a REAPI module | superseded: `crates/js-types` | - |
| step 4 | Introduce a Queue type around the ordered set | superseded: `crates/js-sched/src/queue.rs:1-25` | - |
| step 5 | Introduce Fit and Rank traits with a second trivial Rank to show the seam is real | superseded: `crates/js-sched/tests/sim_x_replay/engine/rules.rs:67` | - |
| step 6 | Additive durable fields class_id, payload, labels, tenant on control-log Request, with old-reader rejection test | superseded: `crates/js-jobs/src/codec/attempt_fields.rs` | - |
| step 7 | Optional fence, limits, assignments, labels on kbf.worker.v1 Start, with cross-version matrix | partly: `crates/js-agent-wire/src/command.rs:187-189`, `crates/js-agent-wire/src/limits.rs` | - |
| step 8 | Replace the LeaseKind match in has_room with ResourceClass data | partly: `crates/js-types/src/class.rs:40`, `crates/js-agent/tests/agent/jobs/quota_start.rs:317` | - |
| step 9 | RetryPolicy trait (9a) and split memory.rs under the file limit (9b) | partly: `crates/js-types`, `crates/js-sched/src/scheduler/memory.rs` | - |
| step 10 | Make Request generic over Payload and Outcome over the result blob | superseded: `crates/js-sched` | - |
| step 11 | Resources as a named vector with four dimensions; unknown extra dimension skipped by old decoder, kept by new | partly: `crates/js-types/src/resources.rs:70-110` | - |
| step 12 | Capacity-provider spike: test-only crate driving draft traits and two-level lease against a fake aws, with 3-way observation invariants over seeds | partly: `crates/js-elastic/src/provider.rs:129`, `crates/js-elastic-fake` | - |
| step 13 | Move Store trait, fake and conformance suite as js-state; backends stay | `crates/js-state/src/` | - |
| step 14 | Move kbf-estimator as js-sizing with SizingPolicy | partly: `crates/js-sizing` | - |
| step 15 | Move kbf-caps matching as js-caps with a schema; unknown-key policy a parameter (refuse or skip-and-report) | partly: `crates/js-caps/src/matching.rs`, `crates/js-caps/tests/fuse_keys.rs:34` | - |
| step 16a | Reconcile the kbf side of js-types with the existing level-1 vocabulary | `crates/js-types`, `crates/js-types/src/sched/work.rs` | - |
| step 16b | Move js-sched; update layering test pure set, forbidden list and clippy-deny check; js-sched no kbf dep; 99% with own tests | `crates/js-sched`, `crates/xtask/src/layering.rs:33-35` | - |
| step 16c | Server adapter translating scheduler events and effects | `crates/js-sched-placer`, `crates/js-lease-server` | - |
| step 17 | Move generic durable transitions into js-store with the scheduler's rows (memory and key floors, fate, in-flight pins, op size keys) | partly: `crates/js-store` | komira-ai/jobscheduler#96 |
| step 18 | Move simulation harness and invariants into js-sim; kbf's simulation instantiates it; same seeds same hashes | `crates/js-sim/src/`, `crates/js-sim-cell` | - |
| step 19 | Extract js-contain from kbf-driver-container with its cgroup tests | `crates/js-contain/src/` | - |
| step 20a | Define jobscheduler.agent.v1: the proto and the server adapter | partly: `crates/js-agent-wire`, `crates/js-lease-server/src/agent_api.rs` | - |
| step 20b | Agent suite and its broken providers on the fake agent in js-conformance | `crates/js-conformance/src/agent/` | - |
| step 20c | The kbf daemon passes the suite through the adapter | superseded: `crates/js-conformance/tests/js_agent_suite.rs` | - |
| step 21a | Split the daemon into js-agent, kbf-executor and kbf extras (page marks superseded by kbf-on-js-agent.md) | superseded: `docs/design/kbf-on-js-agent.md:30` | - |
| step 21b | Switch the daemon to agent v1 using negotiation of 10.3, shipping after step 20's release is the minimum server; downgrade matrix and soak | superseded: `crates/js-agent-wire/src/hello.rs` | - |
| step 22 | Move extracted crates in with history; layering test as cross-repo check; crates enter coverage-baseline at zero; public-repo lint | `crates/xtask/tests/layering.rs`, `docs/design/decisions.md:8` | - |
| gate 10.4 #1-#4 | Each PR leaves kbf the sole implementer of any new trait; byte-identical golden traces and corpus; cross-version matrix (crates/kbf-integration-test/tests/cross_version.rs); rollback is a revert | superseded: kbf-integration-test is a kbf crate not in this repo. Equivalent gates in this repo: golden hashes (js-sched tests/golden), previous-release wire test (js-agent | - |
| 13.4 | Request-history replay: kbf-estimator-replay becomes a generic trace and pluggable arm run through the real scheduler in virtual time with null/floor/candidate arms and Wilson intervals | partly: `crates/js-sched/tests/sim_x_replay/`, `crates/js-cpu-replay` | - |
| 13.6 | Coverage: new crates at zero uncovered; generic counted by best instantiation; js-sched reaches zero with its own tests | `crates/js-coverage` | - |
| 13.7 | Persisted and wire compatibility: corpus from last released binaries; rolling-upgrade recovery test both ways; lease file gains pool name and acquire records additively | partly: `crates/js-agent-wire/tests/one_job_previous_release.rs`, `docs/lease-server-update.md` | komira-ai/jobscheduler#1038 |

## extraction-steps-d.md

As read: No status header. A list of extraction steps 23 to 64 (Phases D to K), each with a "Proves:" line. Only steps 26 and 34 carry "Superseded". Most of the page is stale: the lease API, js-lease and the Pool port were deleted (komira-ai/jobscheduler#804, komira-ai/jobscheduler#831), and the job model, js-elastic and js-sched replaced them.

| Section | Claim | On main | Tracking |
|---|---|---|---|
| step 23 | Rename Provider to Pool and ProviderId to PoolId, delete Flavour, add CapacityProvider with list/AcquireAck/ReleaseAck/Instance, add the Stop effect | superseded: `crates/js-provider/src/lib.rs:1-27` | komira-ai/jobscheduler#727; komira-ai/jobscheduler#831 |
| step 24 | Usage record per lease and labels on every fake resource | `crates/js-usage/src`, `crates/js-elastic/src/provider.rs:71-90` | komira-ai/jobscheduler#797; komira-ai/jobscheduler#874 |
| step 24b | Task-failure retry axis with a portable max_retries budget | superseded: `docs/design/job-retry-and-completion-callback.md` | komira-ai/jobscheduler#1120 |
| step 25 | Schedule triggers, run records and the launcher API | superseded: `docs/design/ucdk.md:21-26` | - |
| step 26 | ucdk side: launcher wiring and provider crate | superseded: `docs/design/ucdk.md:28-35` | komira-ai/jobscheduler#28 |
| step 27 | js-config schema of 5.7, loader and validation; --pool-config as an implicit static provider; refuse static without --allow-test-providers | superseded: `crates/js-lease-server/src/file/` | komira-ai/jobscheduler#840 |
| step 28 | Fake capacity provider and capacity suite with broken providers; js-conformance re-keyed | `crates/js-elastic-fake/src/cloud.rs`, `crates/js-elastic-fake/tests/conformance.rs` | komira-ai/jobscheduler#753; komira-ai/jobscheduler#769; komira-ai/jobscheduler#799 |
| step 29 | Lease table with an acquiring state, the acquire record before the call, acquire off the table lock, pool name in the lease file, share leases under Book | superseded: `crates/js-api-gateway/tests/gateway.rs:139-140` | komira-ai/jobscheduler#804; komira-ai/jobscheduler#831 |
| step 30 | Agent registration: hello with identity and shape, admission, join credentials, deny file, RegisteredAgentSource | `crates/js-registry/src/config.rs`, `crates/js-registry/src/capacity.rs:200` | komira-ai/jobscheduler#906; komira-ai/jobscheduler#922 |
| step 30b | Reuse and integrity: post-job removal, isolation field, taint report and durable tainted set, js-cli host clear-taint | `crates/js-agent/src/clear_taint/mod.rs`, `crates/js-agent/src/lifecycle/taint.rs` | - |
| step 30c | The default provider: capable-host check, no-capable-host refusal, regain_grace, pool-full | `crates/js-agent-wire/src/refusal.rs`, `crates/js-lease-server/src/agent_pool.rs` | komira-ai/jobscheduler#1059 |
| step 31 | Pool scaler in tick: max_machines, max_step, cooldown, idle and boot timeouts, boot-failure brake, releasing, replacement of tainted hosts | `crates/js-scaler/src/settings.rs:53-75`, `crates/js-scaler/src/brake.rs` | komira-ai/jobscheduler#753; komira-ai/jobscheduler#799 |
| step 31c | Ordered providers per pool, free-host-first placement, provider_max_machines, booting machine kept when a host frees | `crates/js-lease-server/src/file/platform.rs:1-22`, `crates/js-lease-server/src/file/limits.rs:5-28` | komira-ai/jobscheduler#724; komira-ai/jobscheduler#838 |
| step 31b | Placement reuse: a freed slot places queued work before any acquire | `crates/js-sched-placer`, `crates/js-lease-server/src/scaler/jobs.rs` | - |
| step 32 | js-provider-aws against a fake client, disabled by default: acquire, registration by instance identity, re-adoption by tag, the IAM scope of 12.4 | partly: `crates/js-provider-ec2/src/provider.rs:657`, `docs/design/aws-iam.md` | komira-ai/jobscheduler#1167 (new) |
| step 33 | Consumer pool selection: class and label selectors in the lease request, --pool/--class/--label in js-cli | superseded: `crates/js-proto/proto/jobscheduler/v1/job.proto` | komira-ai/jobscheduler#804 |
| step 34 | kbf-executor beside js-agent with host mounts | superseded: `docs/design/kbf-on-js-agent.md` | komira-ai/jobscheduler#899 |
| step 34b | kbf as a consumer: standing machine lease per daemon host, lease demand follows kbf's queue, golden traces unchanged | superseded: `docs/design/kbf-on-js-agent.md` | komira-ai/jobscheduler#804; komira-ai/jobscheduler#888 |
| step 35 | The first real aws pool, disabled by default, needing the go in R7 | `crates/js-provider-ec2/src/provider.rs` | komira-ai/jobscheduler#727; komira-ai/jobscheduler#769 |
| step 36 | js-caps: typed model, version order, schema, constraint evaluator with host and guest scope, refusal codes | no | komira-ai/jobscheduler#1168 (new) |
| step 37 | Wire: Hello.facts, Heartbeat.facts_rev/facts, HeartbeatReply.facts_ack, mirror into the legacy map | no | komira-ai/jobscheduler#1169 (new) |
| step 38 | Facts in js-agent: os_version, xcode, sim.* probes, --cap, --facts-interval, revision bump | partly: `crates/js-agent/src/xcode.rs`, `crates/js-agent/src/host.rs:66-80` | komira-ai/jobscheduler#1170 (new) |
| step 39 | Needs in the lease request and matching in the server; refusal codes; image in the grant; unservable wait | superseded: The lease request is gone. Jobs carry Constraints and Needs (job.proto:47-109); the typed Unservable wait exists (whole-node.md:264; closed komira-ai/jobschedul | komira-ai/jobscheduler#125; komira-ai/jobscheduler#804 |
| step 40 | kbf consumer side: translation table in kbf configuration, fail closed without features: needs | superseded: It targeted the lease request's needs, which no longer exists. The kbf side is in another repository (kbf-on-js-agent.md). | komira-ai/jobscheduler#888 |
| step 41 | Sandbox seam in the lifecycle with ContainerSandbox the only implementation | superseded: `crates/js-agent/src/lifecycle/runtime.rs:413` | komira-ai/jobscheduler#740 |
| step 41b | vms dimension in js-types as a booked and offered dimension | no | - |
| step 42 | VmRuntime, GuestChannel, fake VM runtime, js-guest-wire and its hostile-frame corpus, JobSpec runtime and vm fields | partly: `crates/js-agent/src/vm/runtime.rs`, `crates/js-guest-wire/src/lib.rs` | komira-ai/jobscheduler#740 |
| step 43 | VM lifecycle in js-agent (clone, boot, run, destroy, sweep), base_images and image_profiles as facts | partly: `crates/js-vm-lease/src/lib.rs:1-10`, `crates/js-agent/src/vm/helper.rs` | komira-ai/jobscheduler#740 |
| step 44 | Mixed-host accounting: VM bookings in full, containers checked against total less VM memory, offer rules of 5.15 | partly: `crates/js-types/src/sched/work.rs:92` | komira-ai/jobscheduler#129 |
| step 45 | Several runs per lease, each in its own VM | superseded: `crates/js-agent/src/vm/mod.rs:1-3` | komira-ai/jobscheduler#804 |
| step 46 | Whole-node class: Whole admission, exclusive and exclusive_ready fields, withheld token, preparing, prepare_timeout, exclusive_refused report, readiness hook, restore | `crates/js-agent/src/exclusive.rs:13`, `crates/js-agent-wire/src/heartbeat.rs` | komira-ai/jobscheduler#906; komira-ai/jobscheduler#1126 |
| step 47 | First real VM driver, disabled by default, passing the VM conformance suite on an operator host | `crates/js-vmm/src`, `crates/js-agent/src/vm/helper.rs` | komira-ai/jobscheduler#740 |
| step 48 | /readyz beside /healthz: ready after load and first tick, not ready while draining | `crates/js-lease-server/src/ready.rs`, `crates/js-lease-server/src/config.rs:109` | komira-ai/jobscheduler#844; komira-ai/jobscheduler#876 |
| step 49 | Restart grace: --boot-grace, no held lease ends before it | superseded: `docs/design/multi-instance.md` | komira-ai/jobscheduler#873; komira-ai/jobscheduler#878 |
| step 50 | --verify-state and --term-floor for js-lease-server | superseded: git grep verify-state and term-floor find nothing in js-lease-server. State is one Postgres store, no lease or binding files, no writer lock (komira-ai/jobsched | komira-ai/jobscheduler#878 |
| step 51 | deploy/kubernetes manifests with a manifest-asserting test, plus a container image of js-lease-server | partly: the js-lease-server container image is built (komira-ai/jobscheduler#844, an OCI image per Linux target); there is no deploy/ or kubernetes/ directory and no manifest test | komira-ai/jobscheduler#844 |
| step 52 | Client side: clients reach the service by TLS, retry an outage with back-off, re-read the lease on a new term, refuse plain HTTP that is not loopback | partly: `crates/js-client/src/server_url.rs:19` | - |
| step 53 | First in-cluster deployment and the move from the single host | decision, no code | komira-ai/jobscheduler#858 |
| step 54 | GPU machines register through js-agent; the operator-listed --pool-config pool is retired | `crates/js-lease-server/src/file/agentless.rs:22-23` | komira-ai/jobscheduler#840 |
| step 55 | Compatibility checks: server refuses an unsupported protocol major; client reads server version and features and fails closed; cross-version matrix | partly: `crates/js-lease-server/src/agent_api/refused.rs:108-199`, `crates/js-agent-wire/src/protocol.rs:33-50` | komira-ai/jobscheduler#1172 (new) |
| step 56 | Reservation and drain for whole-node requests: cost order, reserved mark, reserved_for, drain deadline kept, reserve_backoff, restart keeps the reservation | `crates/js-sched/src/scheduler/whole.rs`, `crates/js-sched-placer/src/whole_jobs.rs`, `crates/js-sched/src/scheduler.rs` (`Scheduler::place`) | komira-ai/jobscheduler#582; komira-ai/jobscheduler#1127; komira-ai/jobscheduler#1129; komira-ai/jobscheduler#1131; komira-ai/jobscheduler#1133 |
| step 57 | Caps and the host-job rule: max_whole_node, max_draining, per-consumer cap, keep_for_ordinary, allow_last_capable, host_jobs only in a whole-node lease | `crates/js-whole/src/caps.rs:41-158`, `crates/js-registry/src/config.rs`; owner-worker actions and blocked heads outside the caps, one hold per pool: `crates/js-sched/src/scheduler.rs` (`Scheduler::place`) | komira-ai/jobscheduler#1132 |
| step 58 | Elastic machines for whole-node requests: the acquire carries the request id and the machine is committed to it before any other placement | `crates/js-scaler/src/pass.rs:39`, `crates/js-scaler/src/input.rs:21-26` | - |
| step 59 | Packing and demand: bookings placed by fit and FirstFit, scaler demand is the empty hosts the unplaced bookings need; ucdk Job classes admitted Book | partly: `docs/design/placement-by-load.md`, `crates/js-sched-placer` | komira-ai/jobscheduler#960 |
| step 60 | Host facts for VMs: virt and lifecycle keys, launch options with declared facts, probe that lowers vms, VM request as demand only from a declaring option | no | komira-ai/jobscheduler#1173 (new) |
| step 61 | Launch options, price and capacity as data: launch_options and an option on AcquireSpec; choice by price; a refusal cools the option down | `crates/js-sched/src/acquire.rs:160-186`, `crates/js-elastic/src/cache.rs:196` | - |
| step 62 | Spot hosts: Interruption event, reclaim taint, host_leaving and leaves_at on the host list, requeue of unstarted runs, give-up at the deadline, brake exclusion | partly: `crates/js-elastic/src/provider.rs:98`, `crates/js-lease-server/src/scaler/events.rs:1-30` | komira-ai/jobscheduler#1174 (new) |
| step 63 | Tiers and further kinds: nested providers entry, price-unit check, per-tier fall-through, gcp, azure and on-prem kinds each with a fake | partly: `crates/js-provider-gce`, `crates/js-lease-server/src/file/platform.rs` | komira-ai/jobscheduler#1175 (new) |
| step 64 | Cap-time swap and scale-down: swap_window, scale_down_window, victim choice, launch before drain, evict taint, evicted outcome, walk down the catalog, retire scale-down | no | komira-ai/jobscheduler#1150 (new) |

## agent-wire.md

As read: Decision paper (step 20), recommends staying on JSON polling (A); no overall status header. Decision is followed in code, but the design doc's step 20 and section 3.6 still do not record it.

| Section | Claim | On main | Tracking |
|---|---|---|---|
| §2 live-log fields, line 335 | The agent's results_budget is to count the per-result logs fields and the beat's log_grant_state before it fits log tails | `crates/js-agent/src/lifecycle/machine/tails.rs:28-30`, `crates/js-agent/src/session.rs:694` | komira-ai/jobscheduler#691 |
| §4 follow-up (A) 1 | Record the decision (stay on JSON polling) in section 3.6 and step 20 of jobscheduler-design.md | partly: `docs/design/jobscheduler-design.md:301` | komira-ai/jobscheduler#1193 (new) |
| §4 follow-up (A) 2 | Build the server side of the per-job set (step 28): job_start/job_stop/acks, and answer an older agent's heartbeat as a responder (cancel per held run, unknown-lease ack per result) without refusing a hello lacking per-job, with a test against the previous release's heartbeat fixture | `crates/js-lease-server/src/agent_api.rs`, `crates/js-registry/src/registry/beat.rs` | komira-ai/jobscheduler#664; komira-ai/jobscheduler#804 |
| §4 follow-up (A) 2 / §5 point 3 | Decide how and when the run messages (Command::Start/Stop/Cancel, RunResult, ResultAck) leave major 1 / the wire (steps 28, 30) | no | komira-ai/jobscheduler#1156 (new) |
| §4 follow-up (A) 3 / §5 point 7 | Move the previous-release fixtures to the first release whose agent sends the per-job lists, so both-direction tests cover per-job bodies | `crates/js-agent-wire/tests/fixtures/v0.1.208/`, `crates/js-agent-wire/tests/previous_release_v0_1_208.rs` | - |
| §4 follow-up (A) 4 / §5 point 2 | Bound what a heartbeat lists besides tails so a beat whose non-tail content passes the 64 KiB body limit cannot be built (today it is refused 413 and the agent exits 2); also count 413s and cut tails to see how near real beats come | partly: `crates/js-agent/src/lifecycle/machine/per_job.rs:30-36`, `crates/js-agent/src/lifecycle/machine/tails.rs:28` | komira-ai/jobscheduler#1177 (new) |
| §4 follow-up (B) 1-4 | If the decision were B: a .proto for the agent wire, server reading protobuf by Content-Type, a hello capability and welcome field, then a major 2 without JSON | not a gap: decision A (stay on JSON polling) was taken | - |
| §4 follow-up (C) | If the decision were C: a gRPC agent service, long-lived stream limits, max_connections above hosts plus clients, session resume, held commands resent | not a gap: decision A (stay on JSON polling) was taken | - |
| §5 point 1 | Measure the HTTP head and TLS handshake bytes per exchange with an in-process js-client test against a loopback lease server | no | - |
| §5 point 4 | Which fields need proto3 optional so zero and absent stay distinct under (B) | superseded: Only matters under option B, which was not chosen | - |
| §5 point 5 | Derive the protobuf size of a NodeLoad at every cap | superseded: Only matters for the protobuf comparison; JSON stays | - |
| §5 point 6 | Whether a reconnect inside the grace keeps epoch and session or says hello again under (C) | superseded: Only matters for a stream (C), not chosen | - |
| §5 point 8 | Whether a server rollback across a protobuf release is supported and the agent resets encoding per hello | superseded: Only matters under (B); no protobuf release | - |
| §3 (B) Live update, line 424 | The design does not decide server rollback behaviour across a protobuf release | superseded: Same as point 8 | - |
| §3 (C) Latency, line 457 | The design states no goal for start/stop delivery latency | superseded: `docs/design/held-beat.md` | komira-ai/jobscheduler#903 |
| §4 'When the recommendation changes' (handshake cost) | If a per-exchange handshake cost is measured, add connection reuse in js-client for the agent | no | komira-ai/jobscheduler#1146 (new) |

## configuration.md

As read: No status header and no Designed/Open/What-is-left sections. It describes the file as it is today, with four statements of what is not built or not run.

| Section | Claim | On main | Tracking |
|---|---|---|---|
| Machine providers, platforms and pool settings | Platform kinds kubernetes, cloud_run and ecs are refused as not built; only kind = default works. | no | - |
| A pool of hand-joined agents / pool kinds | Several elastic pools, or an elastic pool beside a joined one, are parsed but the server refuses to start on them: one elastic pool alone, or any number of joined pools. | no | komira-ai/jobscheduler#1153 (new) |
| Test provider | The operator-listed test pool (provider kind static with [[host]] tables) is not built and a file that declares it is refused. | superseded: `crates/js-lease-server/src/file/agentless.rs:3` | - |
| Joined pool keys | A joined pool lacks the whole-node keys that the old agent pool file had (max_whole_node, drain_after and others). | no | - |

## deployment.md

As read: Status header: none; the page marks parts as "merged" vs "design", and says "(not built)" only for step 50. Its Kubernetes target (replicas 1, Recreate, PVC, one volume) is overtaken by docs/design/multi-instance.md (N leaderless instances on Postgres).

| Section | Claim | On main | Tracking |
|---|---|---|---|
| Tests / step 48 | /readyz beside /healthz: ready only once files are loaded and the tick has run, not ready while draining. | partly: `crates/js-lease-server/src/ready.rs:1-90`, `crates/js-lease-server/src/api.rs:220` | komira-ai/jobscheduler#844; komira-ai/jobscheduler#876 |
| Shutdown / --drain-hold, --drain-timeout | SIGTERM: readyz 503 at once, agent listener closes, no placement, no dispatch claims, serve job service for drain-hold, then drain. | `crates/js-lease-server/src/config.rs:292-297`, `crates/js-lease-server/src/lib.rs:68-69` | komira-ai/jobscheduler#876 |
| Tests / step 49 and D34 | --boot-grace: no held lease ends before a grace has passed after boot. | superseded: `crates/js-lease-server` | komira-ai/jobscheduler#711 |
| Tests / step 50, and move step 3 'No check of the copied files exists yet' | Read-only --verify-state mode plus --term-floor; a check that the copied binding file and job store agree. | no | - |
| Tests / step 51 | deploy/kubernetes/ manifests and a test that reads them; plus a container image of js-lease-server. | partly: `.github/workflows/artifacts.yml:4`, `docs/release.md:233` | komira-ai/jobscheduler#844; komira-ai/jobscheduler#858 |
| Tests / step 52 and Clients | Clients reach service by cluster name over TLS, retry outage with backoff, re-read lease on new term; kbf frontend moves to job submission (step 11). | superseded: `docs/design/kbf-on-js-agent.md` | - |
| Tests / step 53 and Moving from the single host | First in-cluster deployment and the 6-step cut-over, on a disposable cluster, not on shared CI, needs R7 go. | no | komira-ai/jobscheduler#858 |
| Options for Kubernetes / (b) later | An external store with js-state trait, a leader for tick/reaper/scaler, and a fence; for HA or a second replica. | `crates/js-lease-server/src/lib.rs:422` | komira-ai/jobscheduler#878 |
| Target shape / One process | Capacity providers, pool scaler and host registry join the same process as they are built. | `crates/js-lease-server/src/agents.rs` | komira-ai/jobscheduler#652; komira-ai/jobscheduler#727; komira-ai/jobscheduler#757 |
| The pod / hardening and the Kubernetes objects (PDB, NetworkPolicy, Secret mounts, resources, hostUsers) | All object choices (Deployment Recreate, PVC, Services, PDB, NetworkPolicy, probes over HTTPS) are a design. | no | - |
| The state store / six crash-safety rules and loss of volume | Rules 2, 4, 5 went with the lease table; rules 1, 3, 6 hold for the binding file. | `crates/js-lease-server/src/config.rs:420` | - |
| Upgrades / rollback pin test (13.6) | A test pins that the new server reads a file written before and an older server skips new keys. | partly: Not located by search of js-state/js-jobs for a previous-release fixture; unverified. | - |
| Probes: store unreachable | /healthz answers 503 store unreachable after server.store_unreachable_after. | `crates/js-lease-server/src/api.rs:102` | - |

## hosts.md

As read: No status header. Page is a design text for D56 to D65, steps 59 to 64 (Phase K of extraction-steps-d.md), with a section 2 paragraph "What the code does today, so that this file is not read as already built". Its lease-table/Pool framing is stale (lease API deleted in komira-ai/jobscheduler#831). Not superseded as a whole; jobscheduler-design.md:59 lists it as Reference. No GitHub issue or PR tracks any Phase K step.

| Section | Claim | On main | Tracking |
|---|---|---|---|
| §2 bullet 1 / step 59 | ucdk Job classes will book their declared size under Book and share a host instead of BookAll | superseded: `crates/js-lease-server/src/agent_pool.rs:335` | - |
| §2 bullet 2 / step 59 | The scaler's demand is the empty hosts the queued bookings need after packing (not machines' worth of the shape) | no | komira-ai/jobscheduler#1176 (new) |
| §2 'What the code does today' / step 59 | FirstFit rank in a scaling pool; Spread kept in a fixed pool | superseded: `crates/js-sched-placer` | komira-ai/jobscheduler#1076; komira-ai/jobscheduler#1087 |
| §2 / §3 / steps 59-60 | vms becomes a Dim of js-types | partly: `crates/js-types/src/resources.rs:19`, `crates/js-types/src/sched/work.rs:91-200` | - |
| §2 / §3 | Agent offers total less booked; host admitted at what it reports | yes: Merged PR komira-ai/jobscheduler#1100 'A host is admitted and booked with the capacity it reports: no [shape] floor or ceiling'. | komira-ai/jobscheduler#1100 |
| §2 'What the code does today' | CapacityProvider with acquire and optional events is built in js-elastic | `crates/js-elastic/src/provider.rs:172`, `crates/js-provider-lambdalabs` | - |
| §2 'What the code does today' / step 62 | Scheduler handles provider events() (reclaim notices) | no | komira-ai/jobscheduler#1174 (new) |
| §2 'What the code does today' / steps 62, 64 | Registry keeps marks in memory only; reclaim and evict taints made durable | partly: `crates/js-jobs/src/codec/host_fields.rs:41` | komira-ai/jobscheduler#1174 (new) |
| §2 / step 60 / §4 | Core host facts virt and lifecycle; provider declares virt and vms per launch option; agent probe lowers vms; probe failure reports vms=0 and alerts; guest constraint on them is constraint-bad-scope | no | komira-ai/jobscheduler#1173 (new) |
| §3 Usage attribution | Host instance-hours attributed to leases in proportion to bookings | superseded: `crates/js-usage` | - |
| §3 Load-ranked placement note | Load-ranked placement from jobscheduler PR 59 is a decision for that proposal, not made here | superseded: `docs/design/placement-by-load.md` | komira-ai/jobscheduler#1008; komira-ai/jobscheduler#1087; komira-ai/jobscheduler#1091 |
| §4 rule 1 / step 60 | Scaler counts a VM request as demand only if a launch option's declared facts meet its host constraints | no | komira-ai/jobscheduler#1173 (new) |
| §5 whole-node on a shared host | A whole-node lease books every dimension incl. vms; cap-time swap never touches a whole-node host | superseded: `crates/js-whole` | komira-ai/jobscheduler#906; komira-ai/jobscheduler#1071 |
| §6 spot step 1 | Interruption notice taints host as reclaim (durable), cordons, no new work, no brake count | no | komira-ai/jobscheduler#1174 (new) |
| §6 spot step 2 | host_leaving and leaves_at on GET /v1/hosts; unstarted runs requeued at once | no | komira-ai/jobscheduler#1174 (new) |
| §6 spot step 3 | Reschedule at the deadline: lost work rerun against platform-fault budget | partly: Lost-host retry is built generally: merged PR komira-ai/jobscheduler#1093 (js-types decide: a lost host or start failure retries after a backoff), komira-ai/jobscheduler#1104 task F loss paths | komira-ai/jobscheduler#1174 (new) |
| §6 spot step 4 / §7 | Option just interrupted is marked in capacity data so the next choice may go elsewhere | no | komira-ai/jobscheduler#1174 (new) |
| §7 / step 61 | Provider launch_options(shape) -> `Vec<LaunchOption>`, LaunchOption {id, totals, facts, price, capacity}, [[provider.option]] config | partly: `crates/js-elastic/src/provider.rs`, `crates/js-elastic/src/offer.rs` | - |
| §7 / step 61 | Pick lowest price, then best capacity signal, then lowest id; refused option cools down per option; AcquireSpec gains option | partly: `crates/js-scaler/src/pick.rs:128-139` | - |
| §8 / step 63 | Tiers: providers entry may be an inner list of equals; price-unit check; provider_max_machines map | no | komira-ai/jobscheduler#1175 (new) |
| §8 / step 63 | gcp, azure and on-prem kinds behind the trait, each with a fake; layering test finds no cloud name in the core | partly: `crates/js-provider-gce`, `crates/js-elastic-fake` | - |
| §8 / step 63 | Unreachable provider's share freezes and the walk passes it in the same tick | `crates/js-scaler/src/lib.rs:27-28`, `crates/js-scaler/src/brake.rs` | - |
| §9 invariants H1-H8 / tests | Seeded invariant and packing tests (500 small jobs, vm refused, probe fails, spot storm, tiers) | partly: `crates/js-sched-placer/src/engine_pack_tests.rs`, `crates/js-types/src/sched/work.rs:533-564` | - |
| §9 Steps | Steps 59 to 64 (Phase K of extraction-steps-d.md): 59 packing and demand | partly: Packing: js-sched-placer packs by bookings (not FirstFit); demand by empty hosts not built. ucdk part dropped. | komira-ai/jobscheduler#1176 (new) |
| §9 Steps | Step 60 host facts for VMs | no | komira-ai/jobscheduler#1173 (new) |
| §9 Steps | Step 61 launch options, price and capacity as data | partly: `crates/js-elastic/src/offer.rs`, `crates/js-scaler/src/pick.rs` | - |
| §9 Steps | Step 62 spot hosts | no | komira-ai/jobscheduler#1174 (new) |
| §9 Steps | Step 63 tiers and further kinds | partly: See §8 rows: gce/ec2 built, tiers and azure not. | komira-ai/jobscheduler#1175 (new) |
| §9 Steps / §11 | Step 64 cap-time swap and scale-down | no | komira-ai/jobscheduler#1150 (new) |
| §10 rejected | Considered and rejected options (one machine per job, VM reuse, price-ordered hierarchy, separate spot scheduler, headroom drain and consolidation) | decision, no code | - |
| §11 sizing first instance | First unserved request takes the smallest catalog option that holds it; grow out to max_machines | partly: `crates/js-scaler/src/pick.rs` | - |
| §11 swap up trigger | Per-minute pool sample; swap judged per swap_window at the cap with a growing queue | no | komira-ai/jobscheduler#1150 (new) |
| §11 swap items 1-4 | Victim choice by end bound, biggest option from catalog, launch before drain, wait one window | no | komira-ai/jobscheduler#1150 (new) |
| §11 eviction | evict taint, host_leaving, drain then terminate, evicted outcome charged to no budget, eviction record per request id, max_evictions | no | komira-ai/jobscheduler#1150 (new) |
| §11 scale down | Scale-down by queue-empty and utilization below scale_down_util for scale_down_window; retire taint; one machine per window | partly: `crates/js-scaler/src/lib.rs:127`, `crates/js-jobs/src/codec/host_fields.rs:41` | komira-ai/jobscheduler#1150 (new) |
| §11 parameter table | Pool keys swap_window, scale_down_window, scale_down_util, swap_drain_grace, max_evictions | no | komira-ai/jobscheduler#1150 (new) |
| §11 swap: whole-node interaction | Swap never victimizes a whole-node/reserved host; ceiling by host count | superseded: `crates/js-whole` | komira-ai/jobscheduler#831 |
| §11 spot during the window | reclaim takes precedence over retire and evict | no | komira-ai/jobscheduler#1174 (new) |
| §11 swap victim estimate | Predicted remaining time from sizing history or per-minute profiles 'none is built into this design today' (jobscheduler#59) | superseded: `docs/design/placement-profiles-live.md` | komira-ai/jobscheduler#1037; komira-ai/jobscheduler#1081; komira-ai/jobscheduler#1087 |
| §11 first-swap and long jobs K47, K48 | Risks K47 (long jobs cost lost work after grace), K48 stated | decision, no code | - |
| §11 tests / invariants H9-H14 | Swap and scale-down invariants and tests | no | komira-ai/jobscheduler#1150 (new) |

## placement-by-load.md

As read: No status header. Mostly built; it marks "does not yet persist" (L56), "Planned" telemetry (L379), "no flag yet" flags (L451), "once the profile admission layer supplies it" (L338), x2 and tuning "later" items, and "has not been confirmed" (L141, L445). Several of these are stale or superseded by placement-profiles-live.md and the sizing changes (komira-ai/jobscheduler#998, komira-ai/jobscheduler#1026).

| Section | Claim | On main | Tracking |
|---|---|---|---|
| The cost record in js-sched (L56) | The store does not yet persist the admitted lease cost, so a recovered lease is charged its request (jobscheduler#96). | `crates/js-types/src/sched/work.rs:392`, `crates/js-sched/src/scheduler/recover.rs:91-95` | komira-ai/jobscheduler#92; komira-ai/jobscheduler#96 |
| Scoring, Scored minutes (L338) | The score's fit looks at L = life_minutes; today fit_minutes is 1 until the profile admission layer supplies it. | partly: `crates/js-sizing/src/summary.rs:337`, `crates/js-sched/src/admission.rs:229` | - |
| Time profiles 1-2 (L198-217) | Agent per-minute sampling and heartbeat LeaseMinute/LeaseClock, key_profile ring, node_minute table. | `crates/js-agent-wire/src/load.rs:86`, `crates/js-agent/src/usage/wire.rs:34` | - |
| Time profiles 3 (L229-244) | Admission by per-minute curves (H=30 horizon, H+ check, dilation, g floor, shape guard, expected life) in the placement predicate. | partly: `crates/js-sched/src/admission/profile.rs` | komira-ai/jobscheduler#1057; komira-ai/jobscheduler#1087 |
| Where the code differs (L451): five flags with code | --placement-cpu-slack, -score-balance, -score-slack-tol-ppm, -key-share and -pack-limit have code in LoadConfig::apply_flag. | partly: `crates/js-sched/src/admission/config.rs:161-200`, `crates/js-lease-server/src/flags.rs:20-21` | komira-ai/jobscheduler#1178 (new) |
| Where the code differs (L451): flags with a LoadConfig field and no flag | --placement-load, --placement-cold-per-window, --placement-cpu-floor, --placement-flat-memory-slack, --placement-profile, --placement-profile-memory, --placement-profile-floor, --placement-s-mem and --placement-ws-file-share have a field and no flag. | no | komira-ai/jobscheduler#1178 (new) |
| Where the code differs (L451): --placement-cpu-percentile | The CPU percentile setting belongs to the estimator and is not in js-sched. | superseded: `crates/js-sizing/src/summary.rs:37-41` | komira-ai/jobscheduler#998; komira-ai/jobscheduler#1026 |
| Where the code differs (L451): --placement-load-off-pools | A per-pool emergency off lives in the server. | superseded: `crates/js-sched/src/admission/config.rs:120` | - |
| Where the code differs (L451): --profile-store-bytes | --profile-store-bytes has no flag. | `crates/js-lease-server/src/lib.rs:72` | - |
| What goes away in kbf / Plumbing kbf rows (L58-65, L310-313, L318-319) | kbf-node writes no cpu.max, Welcome.placement_load, kbf protobuf NodeLoad, node_load.rs, supervisor cpu.weight, --actions-cpu-max. | superseded: The repo has no kbf crates (ls crates). AGENTS.md R5 says jobscheduler never depends on kbf. The js-agent equivalent is in the js-agent-wire and js-agent crates | komira-ai/jobscheduler#999 |
| Plumbing, Who produces NodeLoad (L309) | A host whose agent does not yet produce NodeLoad is in the fallback. | `crates/js-agent-wire/src/load.rs`, `crates/js-agent/src/usage/wire.rs` | komira-ai/jobscheduler#1036; komira-ai/jobscheduler#1056 |
| Telemetry, What exists now (L377) | Refusals carry kind and figures and /metrics exports js_placement_refusals_total. | `crates/js-sched/src/admission.rs:102-140`, `crates/js-sched-placer/src/refusals.rs:60` | - |
| Telemetry, Planned (L379-383) | Per-worker measured/capacity, uncovered cost, k, r and k_m histograms, floor-binds share, k_c, q, shape_guard_demotions, minutes_dropped, profile_dilation_missed, keys 1/3/4 shares, hosts kept empty, p99 working set and OOM events. | partly: In-crate counters exist (admission.rs:102-160: misfit_*, demotions_*, floor_windows/binds, score_evaluations, chose_slack, readings_dropped) | komira-ai/jobscheduler#1179 (new) |
| Utilization (L18, L24) / Decided CPU percentile (L449) | The CPU percentile is tuned later from the measured per-run k distribution once the k histogram exists. | superseded: The k histogram does not exist (see Telemetry, Planned). The estimator books the median of the runs' means (summary.rs:37-41), so the P90-of-means premise is go | komira-ai/jobscheduler#1026 |
| Utilization (L25-26) | A feedback correction scaling predictions by observed mean k was considered and not proposed. | no | - |
| Admission, Memory guard (L141) and Decided (L445) | The flat-sum memory guard with d = 0 is the memory admission limit. The page says it has not been confirmed. | decision, no code | komira-ai/jobscheduler#127 |
| Starvation (L282) | A refinement was considered and not implemented: reserve the worker where the head first passes the predicate over the horizon (smallest tau). | no | komira-ai/jobscheduler#582 |
| Lying or stuck node (L294-301) | A plausibility check compares leases' summed cpu_usage_us over a span with the counter's growth, and flags an idle node reporting over half of C for 3 windows. It is tested by sim k. | partly: `crates/js-sched/src/admission/ledger.rs:20` | komira-ai/jobscheduler#1180 (new) |
| Simulation tests (L404-439) | Sims a-j, l-w, x, y, z and aa to ah. | `crates/js-sched/tests/load_sims_cpu.rs`, `crates/js-sched/tests/sim_ac_same_key_fan_out.rs`, `crates/js-sched/tests/sim_ae_head_under_best_fit.rs`, `crates/js-sched/tests/sim_ah_utilization_lever.rs` | komira-ai/jobscheduler#1181 |
| Simulation tests, sim k | k (stuck-low signal), with the plausibility check it tests. | no | komira-ai/jobscheduler#1180 |
| Scoring, Unmeasured (L370) and sim x2 (L429) | Replay on the real series after 14 days of collection, to set w, b, tol and the key share from the measured gain from curves. | partly: `crates/js-sched/examples/sim_x2.rs` | komira-ai/jobscheduler#1182 (new) |
| Time profiles 3 / bound with profiles (L230, L273, L372, L132) | Lowering g, lowering f below 1, reducing delta for short keys are later decisions from the histograms. | decision, no code | - |
| Scoring, Level (L325) | Level-1 scoring is behind a flag that defaults off, 'with js-lease-server --placement js-sched'. | superseded: `crates/js-sched-placer/src/engine.rs` | komira-ai/jobscheduler#800 |
| Order (L457-459) | The build order has the sampling and the tables first; the section lists nothing after that sentence. | superseded: `docs/design/placement-by-load.md:457-459` | - |
| Decided: No best fit at level 1 for now (L448, L325) | Best fit stays at level 2; FirstFit and scale_down_util at level 1 are unchanged, and a pool-scaling design would be needed to change this. | decision, no code | - |

## mac-lease-teardown.md

As read: No status header and no "Designed/Planned/Open question/step list" section. It is a behaviour description of the js-mac-session helper (kill-uid, pool reset, deletion). It names only two forward-looking items (the "next step" account per job, and "No tool opens that window yet"). Most of the page is stale: native runs on main no longer go through the helper.

| Section | Claim | On main | Tracking |
|---|---|---|---|
| Step 6 of the deletion (and intro) | Deleting an account per job is 'the next step, in the agent (mac-account-per-job.md)'; the pool reset is unchanged by the grant. | `crates/js-agent/src/mac_accounts/mod.rs:1-8`, `crates/js-agent/src/native/remove.rs:112` | komira-ai/jobscheduler#1014; komira-ai/jobscheduler#1023; komira-ai/jobscheduler#1106 |
| Intro and 'user-delete of a pool user: the reset' | The native runtime calls kill-uid and user-delete when a run is removed and again from its journal at start; a lease user is a permanent pool user reset in place (reset steps 1-8, 8a). | superseded: `crates/js-agent/src/native/remove.rs:23-29` | komira-ai/jobscheduler#1023 |
| After the run: the leak scan (last bullet) | The baseline is recorded only in its window, which the root tool that applied a software set or erased the Mac opens. 'No tool opens that window yet', so on a node today the check judges nothing. | no | - |
| After the run: the leak scan (checks bullets) | NativeChecks judges scan residue, host-changed notes, unreadable and no-baseline outcomes after each native run. | partly: `crates/js-agent/src/leak_scan.rs:47-100`, `crates/js-agent/src/platform/macos.rs:780` | - |
| While no agent runs | The helper holds a run's deadline (deadline_ms) and ends the lease's processes, the agent's stop goes through the helper (SIGTERM, then kill-uid), and the next agent adopts the run via the helper's watch. | superseded: `crates/js-agent/src/mac_supervisor/` | komira-ai/jobscheduler#991 |

## decisions.md

As read: No status header. Decision list D1 to D66; says "None is final until the owner answers, except where marked settled"; D26 stated OPEN. Most entries are recommendations; those with a build status are listed below.

| Section | Claim | On main | Tracking |
|---|---|---|---|
| D26 | macOS VM sizing (fixed size classes vs sizing per run) is open; size fields of step 43 wait for it | `crates/js-agent/src/vm/runtime.rs:108` | - |
| D1 | Crates extracted from kbf move in at step 22; until then they build in the kbf workspace | yes: crates/ holds js-sched, js-state, js-registry, js-agent, js-lease-server etc.; issue komira-ai/jobscheduler#1130 title says 'now kbf has no js core' | - |
| D15 | js-sched per-request placement (steps 3 to 18) is conditional on option (b); kbf keeps kbf-sched until js-sched is proven | `crates/js-sched/src`, `crates/js-sched-placer` | komira-ai/jobscheduler#1133 |
| D4 | Seen-work sizing: implement R1 as SeenRule::LastPeakPlusBuffer and replay it (option a), default after a replay; ladder stays as shipped | no | - |
| D34 | Leases across a restart: --boot-grace, off by default, no lease ends before it passes after a boot | partly: `crates/js-jobs/src/result.rs:33`, `crates/js-lease-server/src/job_expiry_tests.rs:126` | - |
| D44 | Compatibility rules and the version-pair check, step 55 | superseded: decisions.md:187-192 supersession note | - |
| D17 | No look-ahead in scaling demand until a trace shows a need | `crates/js-scaler/src/settings.rs` | - |
| D18 | Self-registration auth: join token now, a client certificate per pool added later without a wire change | `crates/js-lease-server/src/agent_api.rs`, `crates/js-agent-wire/src/credential.rs` | komira-ai/jobscheduler#1118 |
| D59 | Spot reclaim notice: taint 'reclaim', host_leaving and leaves_at flag on the host list, let work finish to the deadline, requeue unstarted, rerun at the deadline | partly: `crates/js-lease-server/src/scaler/effects.rs:45` | komira-ai/jobscheduler#1174 (new) |
| D60 | Price and capacity as sort keys among options and providers, never limits | partly: `crates/js-scaler/src/lib.rs`, `crates/js-elastic/src/cache.rs` | - |
| D61 | A tier groups equal providers in the one ordered hierarchy; a tier is passed over only when none of its members can satisfy | no | komira-ai/jobscheduler#1175 (new) |
| D39 | Optional `provider_max_machines.<name>` bounds one provider's share of max_machines | partly: `crates/js-scaler/src/lib.rs` | - |
| D62 | At the machine cap with a growing queue: launch the biggest type and retire the smallest instance (swap_window, default 10 min) | no | komira-ai/jobscheduler#1150 (new) |
| D63 | Drain before terminate with the taint 'evict', swap_drain_grace, the 'evicted' attempt end and max_evictions=3 with a per-request eviction record | no | komira-ai/jobscheduler#1150 (new) |
| D64 | Victim choice (smallest, soonest drain), never-victim set, biggest-type catalog walk | no | komira-ai/jobscheduler#1150 (new) |
| D65 | Scale down by queue empty and utilization below scale_down_util for scale_down_window (60 min); one machine per window marked 'retire' | no | komira-ai/jobscheduler#1150 (new) |
| D66 | Designed, not built: tmpfs source, image-tree source, id mapping, UDP host ports, host Unix sockets | no | - |
| D66 | How a copy-on-write mount is made on macOS is decided after a lazy provider's macOS mount is measured | decision, no code | - |
| D19 | Asynchronous acquire: the table records 'acquiring', the scaler calls outside the lock, and the result is an event | `crates/js-scaler/src/lib.rs` | - |
| D3 | Durable leases across agent restart not in v1 (an agent capability flag) | decision, no code | - |
| D36 | js-pool-static renamed js-pool-fake; --pool-config flag refused with a migration message | `crates/js-pool-fake` | - |
| D38 | min_machines removed from schema and docs | yes: git grep min_machines in crates finds nothing. | - |

## amendments.md

As read: History page (moves of design.md 1.1: what each amendment deleted and added). It has no status header and no Designed/Planned/Partly built tags. Its only open items are D26 and the warm pool; the rest are step ranges it lists as "added".

| Section | Claim | On main | Tracking |
|---|---|---|---|
| The capabilities amendment | Decision D26 (macOS VM sizing: fixed classes or per-run sizing) is listed as open. | decision, no code | - |
| The refinement that followed | A warm pool of VMs to hide the boot is left as future work. | no | - |
| The capabilities amendment | Steps 36 to 47 (capabilities, VM backend in js-agent, VM lifecycle, mixed-host accounting, several runs per lease, whole-node class, first real VM driver disabled by default). | `crates/js-caps`, `crates/js-vm-lease/src/lib.rs:1` | komira-ai/jobscheduler#129; komira-ai/jobscheduler#740 |
| The refinement that followed | Steps 48 to 53, one service in Kubernetes: 48 readiness and shutdown, 49 restart grace (D34), 50 verify-state, 51 manifests and image, 52 client TLS and retry, 53 first in-cluster deployment. | partly: `crates/js-lease-server/src/ready.rs:1`, `crates/js-lease-server` | komira-ai/jobscheduler#858 |
| The amendment after that (the owner's review) | Step 54: GPU machines register through js-agent with a default provider. | `crates/js-agent`, `crates/js-provider-lambdalabs` | komira-ai/jobscheduler#653; komira-ai/jobscheduler#717; komira-ai/jobscheduler#731 |
| The amendment after that (the owner's review) | Provider hierarchy: a per-pool ordered list that falls through only when an earlier entry cannot satisfy now (5.8), plus steps 30c and 31c. | `crates/js-sched/tests/failover.rs`, `crates/js-lease-server/tests/in_process/walk.rs` | - |
| The first amendment | Steps 27 to 35 (with 30b and 31b), including step 35, the first real aws pool, disabled by default. | `crates/js-provider-ec2`, `crates/js-elastic` | - |
| The rollout amendment | Step 55: compatibility checks of rollout.md (D44), where the server refuses a Hello whose protocol major it does not speak. | `crates/js-sim-cell/tests/skew.rs:270`, `crates/js-sim-cell/src/agent/profile.rs:50` | - |
| The whole-node amendment | Steps 56 to 58: reservation and drain, caps and the host-job rule, elastic machines for whole-node requests (the whole-node class, D46 to D55). | `crates/js-whole`, `crates/js-sched/tests/whole_node_caps.rs:505` | - |

## capabilities.md

As read: Design page, 'mostly planned' per jobscheduler-design.md:66. Only the Xcode/sim.* section and the whole-node GPU path are built; the typed-facts wire, constraint language and VM image facts are not. Its status wording is inline, not in a status header.

| Section | Claim | On main | Tracking |
|---|---|---|---|
| Xcode (para 1) | Typed `facts` are not built; xcode rides the legacy capabilities map as a comma-joined build list, one member each in js-caps | partly: `crates/js-agent-wire/src/hello.rs:311`, `crates/js-agent/src/xcode.rs` | komira-ai/jobscheduler#1169 (new) |
| Wire: Hello.facts | Optional Hello.facts {rev, host, images} with Value tagged union and new size limits | no | komira-ai/jobscheduler#1169 (new) |
| Wire: Heartbeat.facts_rev, Heartbeat.facts, HeartbeatReply.facts_ack | Heartbeat carries the newest facts snapshot, acked by facts_ack, repeated until acked | no | komira-ai/jobscheduler#1169 (new) |
| Wire: old peers mirror and conflict rule | New agent mirrors text/version facts into legacy map; a conflicting key in both forms is invalid-hello | no | komira-ai/jobscheduler#1169 (new) |
| Wire: advertised support for `needs` (features) | Server lists feature `needs` in Welcome.features and the version reply; consumer refuses non-empty needs or a vm class to a server without it | no | komira-ai/jobscheduler#1171 (new) |
| Schema (configuration of scheduler) | Scheduler schema of key, kind, note; unknown request key refused, unknown or kind-mismatched reported key skipped and logged; pool [capabilities] may use the constraint language | no | komira-ai/jobscheduler#1168 (new) |
| Model: five kinds and version grammar/order | text, version, set_text, set_version, count kinds with the page's version grammar and total order | no | komira-ai/jobscheduler#1168 (new) |
| Core keys: arch, os, os_version, xcode, sim.* | Core fact keys reported by agent | partly: `crates/js-caps/src/lib.rs`, `crates/js-agent/src/xcode.rs` | komira-ai/jobscheduler#650; komira-ai/jobscheduler#950; komira-ai/jobscheduler#983; komira-ai/jobscheduler#984 |
| Core keys: base_images, vms, virt, lifecycle | Facts base_images, vms (VM ceiling), virt (none/nested/bare-metal), lifecycle (on-demand/spot/registered) | no | komira-ai/jobscheduler#1173 (new) |
| Core keys: gpu_class | gpu_class text fact from the shape, apple-m on the Mac Studio GPU pool | superseded: `crates/js-agent-wire/src/hello.rs:93` | komira-ai/jobscheduler#665; komira-ai/jobscheduler#1100 |
| Core keys: --cap flag | Repeatable `--cap key=value` overrides a probe, parsed by the key's kind | partly: `crates/js-agent/src/cli.rs:102`, `crates/js-agent/src/host.rs:92` | - |
| Xcode: sim.* matching | Scheduler does not match sim.* yet; no job constraint names it; js-caps passes the keys over | no | komira-ai/jobscheduler#1171 (new) |
| Xcode: facts rebuilt only at agent start | Survey runs at start; an Xcode whose licence or simulator runtimes change is advertised at the next start; resume compares host facts | `crates/js-agent/src/update_hook/machine.rs:634` | komira-ai/jobscheduler#947; komira-ai/jobscheduler#950; komira-ai/jobscheduler#984 |
| Image profiles | Each primed image carries a profile in its manifest; agent advertises image_profiles: name -> map | no | komira-ai/jobscheduler#1189 (new) |
| Trust | Facts are unverified claims; taint and deny-file path on a lie; sandbox=vm cannot be checked | decision, no code | - |
| Storage and refresh | Server keeps newest facts in memory; lease records rev and image chosen; agent re-probes at start, after a job, after a VM store change and every --facts-interval; bumps rev only on change | no | komira-ai/jobscheduler#1170 (new) |
| Constraint language: needs list, ops table, scopes | Request carries `needs`: up to 32 constraints with key/op/value/scope over the five kinds (eq, in, ge, series, range, has, has_all ...) | no | komira-ai/jobscheduler#1168 (new) |
| Semantics 1 to 5 (absent unmet, independent constraints, guest scope, evaluate at grant, unknown op refused) | Matching rules of the constraint language | no | komira-ai/jobscheduler#1168 (new) |
| Refusal codes | constraint-unknown-key, -bad-op, -bad-value, -bad-version, -empty-range, -bad-scope, -too-many refused at submit with the constraint index | no | komira-ai/jobscheduler#1168 (new) |
| kbf's translation | kbf keeps a table from platform properties (OSFamily, Arch, sandbox, min-os, xcode, sim-runtime) to constraint templates | partly: `crates/js-caps/src/matching.rs:11` | - |
| GPUs (D32): shape gpu_class/gpus, GPU request only satisfiable by that pool | GPU requests match only the GPU host pool; host shape declares gpu_class apple-m, gpus 1 | superseded: `crates/js-caps/src/accel.rs` | komira-ai/jobscheduler#665; komira-ai/jobscheduler#1100 |
| GPUs: whole-node class, gpu-host alias | Class whole-node admitted Whole, one whole host per lease; gpu-host loads as whole-node | `crates/js-lease-server/src/file/joined.rs:10`, `crates/js-jobs/src/record.rs:20` | komira-ai/jobscheduler#35 |
| GPUs: registered through js-agent with a default provider; 'operator-listed pool is the stepping stone, step 54' | GPU Macs register themselves via js-agent; static pool list is a stepping stone | `crates/js-lease-server/src/file/platform.rs:7` | komira-ai/jobscheduler#793; komira-ai/jobscheduler#906 |
| GPUs: VMs get no GPU (gpu_indices empty for VM, invalid-job) | A VM job with a GPU index is refused invalid-job | no | - |
| GPUs: container GPU jobs on Linux | Not a current use; container GPU path stays in code, not extended | superseded: The page's premise is stale: crun runtime gives jobs GPUs (komira-ai/jobscheduler#653, komira-ai/jobscheduler#731) and NVIDIA GPU providers exist (komira-ai/job | komira-ai/jobscheduler#653; komira-ai/jobscheduler#717; komira-ai/jobscheduler#731 |
| Tests (13.2) | Table test per op and kind, ordering properties, refusal-code cases, guest scope, wire cross-version golden hello with facts, legacy-only hello, ack-lost repeat, limits (39 ok/41 refused), kbf table test, GPU fleet test | no | komira-ai/jobscheduler#1168 (new) |

## kbf-today.md

As read: A description of kbf at kbf 45e420656 and 89af8f2ef, not a build plan. It has no status header and no "Open" or "What is left" section, and it names two build steps (0 and 2) that belong to extraction-steps.md. Its ucdk section 4.6 is marked superseded (2026-11-24).

| Section | Claim | On main | Tracking |
|---|---|---|---|
| 4.1 Admission by kind | LeaseKind::Vm is marked planned: no front accepts it, no driver reports it, and has_room needs the full request to fit. | `crates/js-types/src/sched/lease.rs:67` | komira-ai/jobscheduler#129; komira-ai/jobscheduler#740 |
| 4.1 Queue | No quota, per-tenant fairness, aging or queue-length cap exists in the scheduler, front or server. | no | - |
| 4.1 Retries and loss | The scheduler's loss requeue is off by default and the server turns it on with with_loss_requeues. | superseded: `docs/design/retries-and-runs.md` | komira-ai/jobscheduler#1096; komira-ai/jobscheduler#1097; komira-ai/jobscheduler#1103; komira-ai/jobscheduler#1104 |
| 4.2 Server and durable record | lib.rs and scheduler.md say queue persistence is not done, while durable.rs writes queue rows. The page says the docs are fixed in step 2. | superseded: `docs/design/extraction-steps.md` | - |
| 4.3 Request history | learned.rs never books Estimate.memory_bytes for a first run, so R1 'seen work starts at last peak plus a buffer' is not what sizes first runs. The 'node or half a node' rule is not located (D4). | superseded: `docs/design/estimate-feed.md:3` | - |
| 4.4 Node side | The Offer and LeaseOffer placeholders in the worker proto change nothing, and Start.device_id is on the wire but unread by the daemon or drivers. | superseded: `crates/js-agent-wire` | - |
| 4.4 Node side | Stale proto comments (soft limit versus the hard memory.max rule, and MEMORY_KILL_OWN_LIMIT doubling) and the memory_max rule exists in three places (7.3). | superseded: `crates/js-sizing` | - |
| 4.4 Node side | The scheduler derives a fence policy that is not on the wire, so the daemon self-fences every lease. 'Leases die with the daemon; re-adoption is planned only.' | `crates/js-agent/src/lifecycle/runtime.rs`, `crates/js-agent/tests/agent/lifecycle/adopt.rs` | komira-ai/jobscheduler#671; komira-ai/jobscheduler#742; komira-ai/jobscheduler#855; komira-ai/jobscheduler#1023 |
| 4.4 Node side | The proto's strict Heartbeat contract (list every held unit before the first heartbeat) differs from the scheduler's graded give-up rule. The design keeps both. | decision, no code | - |
| 4.5 Simulation / Step 0 | No golden trace hash is stored. Step 0 will write explicit hand-written trace lines (no {:?}), cover both hash mechanisms and state whether the 64-bit FNV-1a hash is replaced. | `crates/js-sim-cell/tests/golden/trace-hashes.txt`, `crates/js-sim-cell/tests/replay.rs` | komira-ai/jobscheduler#684; komira-ai/jobscheduler#699; komira-ai/jobscheduler#705; komira-ai/jobscheduler#735 |
| 4.5 Simulation | Nothing compares a trace before and after a PR, and any Debug-format change moves the hash. | partly: The golden trace-hashes.txt gives a before-and-after comparison for js-sim-cell scenarios | - |
| 4.6 ucdk | The launcher, run verb, run records, real cloud providers and cost logic are not built. UL1-UL4 and UJ1-UJ3 are properties this design needs of a launcher. | superseded: `docs/design/ucdk.md:7` | komira-ai/jobscheduler#1082 |
| Corrections: scheduler cores | Only the legacy core runs in a kbf server. js is refused unless the test-only farm-core-js feature is built, and nothing in kbf runs js-agent or a js lease client. | decision, no code | komira-ai/jobscheduler#1130 |
| Corrections: storage and trust | A node runs one storage-and-daemon process that holds the cold-store credential beside untrusted actions. Its unit says trusted network only until a test proves an action cannot read the key. | no | - |
| Corrections: tierstore | tierstore is deployed nowhere and refuses presigned requests (kbf#1922). It goes on the storage nodes with Silo as its hot tier (decided). kbf-pack flags default off. | no | komira-ai/jobscheduler#1922 |

## machines.md

As read: No status header. The page has no Designed, not yet built, Planned, Partly built, Open question or What-is-left markers and no build or extraction steps. It is a present-tense spec, and the numbered steps in 5.10 are runtime steps of js-agent, not build steps.

| Section | Claim | On main | Tracking |
|---|---|---|---|
| 5.12 Replacement is bounded | The page says a pool raises an alert after max_taints_in_a_row replacements without a clean job between them, and stops replacing until the owner clears it. The page gives no status marker for this. | no | - |

## mac-runtime.md

As read: Status: proposed (line 3). Section 4 step 2 (usage) is done; steps 1 and 3 are not built. Step 3 (memory cap) is overtaken by komira-ai/jobscheduler#1027 and B14.

| Section | Claim | On main | Tracking |
|---|---|---|---|
| Status line / What changes | The whole page is a proposal: 'Nothing in this document changes code by itself', with section 4 the one behaviour change. | partly: `docs/design/mac-runtime.md:3` | komira-ai/jobscheduler#886 |
| §1 One runtime interface | 'A separate change makes the backend a compile-time choice by target OS.' | `crates/js-agent/src/lib.rs:308`, `crates/js-agent/src/cli.rs:12-15` | - |
| §3 table, memory row | Memory is not enforced on a Mac ('Enforced today: no'); new probe limits/as-not-enforced. | no | komira-ai/jobscheduler#1183 (new) |
| §3 table, memory row | proc/rusage-other-processes, an info probe today, becomes asserted and reads a process of another uid. | no | komira-ai/jobscheduler#1183 (new) |
| §3 table, rlimits NPROC | RLIMIT_NPROC is enforced per uid; new probe limits/nproc-per-uid. | partly: `crates/js-agent/src/mac_supervisor/spec.rs:34`, `crates/js-mac-probe` | komira-ai/jobscheduler#1183 (new) |
| §3 table, rlimits FSIZE | RLIMIT_FSIZE is enforced per process; new probe limits/fsize. | partly: `crates/js-mac-session/src/limits.rs` | komira-ai/jobscheduler#1183 (new) |
| §3 table, CPU row | No CPU quota on macOS; new probe limits/cpu-seconds for RLIMIT_CPU. | no | komira-ai/jobscheduler#1183 (new) |
| §4 Steps, step 1 | The new asserted probes of section 3, in js-mac-probe. | no | komira-ai/jobscheduler#1183 (new) |
| §4 Steps, step 2 / Today | Usage: the agent samples each run's processes at each poll, the supervisor's wait4 usage, job usage and series, heartbeat load from host counters. | `crates/js-agent/src/mac_usage/macos.rs`, `crates/js-agent/src/native` | komira-ai/jobscheduler#1056 |
| §4 Memory: a cap enforced by sampling | The helper samples each live run every 2 seconds and kills the uid over its cap. | no | - |
| §4 Memory: cap function moves | memory_max moves from js-contain/cgroup.rs to a crate both targets build. | no | - |
| §4 Memory: memory_cap_bytes wire field | A new optional memory_cap_bytes field in the helper's run request, an old_helper fallback and a golden. | no | - |
| §4 Steps, step 3 | The memory cap: memory_cap_bytes, the kill, and the oom_killed report. | superseded: `crates/js-contain/src/cgroup.rs:309-315` | komira-ai/jobscheduler#1027 |
| §4 CPU: measured, not limited | Each run's CPU time goes into the heartbeat so rightsizing from history works on Macs. | `crates/js-agent/src/mac_usage/macos.rs` | komira-ai/jobscheduler#1056 |
| §4 closing line | Each step is tested on Linux against the helper's fake host and on the beta macOS runner. | partly: `crates/js-mac-session/src/helper/tests` | - |
| §5 Policy: floor in pool file | The Macs' pool mac-build sets os_version = "26" in [capabilities]. | partly: `crates/js-agent-wire/src/refusal.rs:149`, `crates/js-lease-server/tests/server/mac_os_version.rs` | komira-ai/jobscheduler#1100 |
| §5 Check | mac_os_version.rs runs four Macs (two on 27, two on 26) and checks placement, no-capable-host and below-floor. | `crates/js-lease-server/tests/server/mac_os_version.rs` | - |
| §5 Raising the floor | Raise the floor to 27 once every Mac runs 27. | decision, no code | - |

## sizing.md

As read: No status header. Sections 7.1-7.6 are an extraction design written in the future tense ("becomes", "stays"). 7.7-7.11 are written as shipped (js-sizing and the placer). Only 7.6 has a "not yet" in it, and that is the swap-peak sentence in 7.7.

| Section | Claim | On main | Tracking |
|---|---|---|---|
| 7.2 SizingPolicy | The scattered sizing constants become one SizingPolicy value (limit_of, round, slack, n_min, mixed_ratio, raise_only, ladder, seen, mode and the rest), and SizingPolicy::kbf() keeps every estimator output unchanged (extraction step 14). | partly: `crates/js-sizing/src/run.rs:66` | - |
| 7.2 Declared mode | In Declared mode (ucdk) history only informs a warning or a suggested size and never raises a booking. | no | - |
| 7.3 One limit rule | The scheduler computes a memory limit via SizingPolicy::limit_of, sends it in Start `limits`, and the job service's limit_hits uses the same limit. | superseded: `crates/js-agent-wire/src/command.rs:46` | komira-ai/jobscheduler#999; komira-ai/jobscheduler#1027 |
| 7.4 Ladder per class | The --memory-ladder-gib ladder, server-wide today, becomes per resource class: an unseen request climbs its class's ladder. | no | - |
| 7.4 cap_of | The cap becomes an explicit core query cap_of(request): the memory of the largest live pool (cordoned or not) serving the request's kind, platform, CPU and GPU needs, or the largest shape a scaling pool can acquire. | partly: `crates/js-sched/src/scheduler/memory.rs:807`, `crates/js-sizing/src/run.rs:75` | - |
| 7.4 Ladder plugin | The ladder becomes a plugin behind RetryPolicy, because memory.rs mixes retry accounting with the ladder (1286 lines). | superseded: `crates/js-jobs/src/codec/attempt_fields.rs:35` | komira-ai/jobscheduler#1094; komira-ai/jobscheduler#1097; komira-ai/jobscheduler#1104 |
| 7.4 D4 (limit vs booking) | Open question D4 part 2: because the build-host limit is 1.5x + 512 MiB, a 12 GiB booking has an 18.5 GiB limit and the next rung (16 GiB) is below the limit that just failed. Whether to climb above the limit is undecided. | decision, no code | - |
| 7.5 SeenRule / R1 | R1 (a seen request starts at its last peak plus a buffer) versus the shipped long-window maximum with 20% + 128 MiB: the design does not decide. SeenRule would carry both; kbf stays as shipped until the owner picks (D4). | decision, no code | - |
| 7.5 Lowering a booking | Lowering a booking stays off until a replay on recorded data supports it (shadow mode first, then shadow_diff for parity). | superseded: `crates/js-sizing/src/store.rs:298` | komira-ai/jobscheduler#1026; komira-ai/jobscheduler#1035; komira-ai/jobscheduler#1090 |
| 7.6 SizeLearner | Learned::stage then requeued/finish becomes a SizeLearner with kbf types removed, learning only after the scheduler committed. | superseded: `crates/js-sched-placer/src/sizes.rs` | - |
| 7.6 SizeStore | Persistence becomes a SizeStore trait (get_all, put_batch, delete, scan), written outside the scheduler transaction, best-effort after kills and write-behind otherwise. | superseded: `crates/js-sizing/src/codec.rs` | - |
| 7.7 swap peak | A run's swap peak, which the agent reports, is not in the job record, so a run that a host short of memory squeezed into swap is not yet counted. | no | komira-ai/jobscheduler#1184 (new) |
| 7.9 / 7.7 history from before | Runs taken under a cpu.max stay what they were: held runs only raise their class and need no conversion. | yes: The page describes shipped behaviour (komira-ai/jobscheduler#1026). Listed only because it looks like a migration step; it is not an open item. | komira-ai/jobscheduler#1026 |

## retries-and-runs.md

As read: No status header or Designed/Planned/Open/What-is-left sections. Four forward-looking claims: step 24b, the Evicted/D62-D63 text, the "configurable floor" and "step 6". Page says nothing about built status.

| Section | Claim | On main | Tracking |
|---|---|---|---|
| 8.1 para 1: ucdk conformance kit ... step 24b adds the axis | The task-failure retry axis and the portable max_retries budget (UJ1/UJ2 case) are added by step 24b. | superseded: `docs/design/extraction-steps-d.md:14` | komira-ai/jobscheduler#1072; komira-ai/jobscheduler#1089; komira-ai/jobscheduler#1093; komira-ai/jobscheduler#1097; komira-ai/jobscheduler#1104 |
| 8.1 para 2: attempt ends `evicted` (D62/D63, hosts.md s11) | An attempt still running when a swap drain grace ends is requeued as a new `evicted` attempt that charges no budget; max_evictions counts it and the host is passed over. | no | komira-ai/jobscheduler#1150 (new) |
| 8.3 Run records: retention floor | Jobs need a configurable retention floor with a default of days, held by the state store. | `crates/js-jobs/src/result.rs:311`, `crates/js-lease-server/src/config.rs:547` | komira-ai/jobscheduler#663 |
| 8.3 Run records: record fields and 'written with additive fields (step 6)' | A submit returns only after a durable run record exists, with fields request id, definition ref, phase, tenant, labels, fence, attempts (provider, lease, start, end, outcome, usage), outcome and log cursor, written additively. | partly: `crates/js-jobs/src/record.rs:239`, `crates/js-jobs/src/attempt.rs:95` | komira-ai/jobscheduler#663; komira-ai/jobscheduler#1097 |
| Log tail of a contained run | Log tail (8 KiB each of stdout and stderr, truncated flag, logs-failed and log-tail-shed findings, halving to fit the heartbeat) is described as built, in past tense. | `crates/js-agent-wire/src/heartbeat.rs:88` | - |

## vm-backend.md

As read: No status header. Mixed page: a "Built so far" paragraph (line 64) records the shipped Mac-only slice (js-agent::vm, js-vm-lease, js-vmm lease_system, PR komira-ai/jobscheduler#740). Most of the rest is the earlier design (VmRuntime 7-op trait, Sandbox seam, Put/Exec/Log frames) which is not what was built. D26 is marked open.

| Section | Claim | On main | Tracking |
|---|---|---|---|
| 5.14 intro rule 2 / Future work | A warm pool of pre-booted VMs to hide boot time is future work, not designed. | no | - |
| Where it fits (trait VmRuntime / Sandbox) | Second backend trait VmRuntime (images, clone_and_boot, poll, open_channel, stop, destroy, list_clones), GuestChannel, and a Sandbox seam (ContainerSandbox/VmSandbox) in the lifecycle. | superseded: `crates/js-agent/src/vm/runtime.rs:1` | komira-ai/jobscheduler#740 |
| Agent flags table | --vms, --vm-network, --vm-boot-timeout, --vm-log-cap, --vm-transfer-cap, --vm-allow-shares, --vm-allow-writable-shares | no | komira-ai/jobscheduler#1185 (new) |
| Built so far | Live logs of a VM job (output reaches the result only as its tail) | no | komira-ai/jobscheduler#1186 (new) |
| Built so far | The `vm` object of a start (base_image, outputs, guest user) and base-image choice per start | no | komira-ai/jobscheduler#1155 (new) |
| Built so far | Server side: sending kind: vm starts and a JobSpec runtime = vm | partly: `crates/js-agent-wire/src/kind.rs:12`, `crates/js-sched/src/admission.rs:335` | komira-ai/jobscheduler#1155 (new) |
| JobSpec additive fields | runtime=vm with vm{base_image, outputs, user}; limits rounded to whole CPUs; isolation=host + vm refused invalid-job; failed_to_start details vm-clone-failed, vm-boot-timeout, vm-guest-protocol, image-unavailable | partly: `crates/js-agent-wire/src/refusal.rs:67`, `crates/js-agent/src/lifecycle/machine/vm.rs:17-45` | komira-ai/jobscheduler#1155 (new) |
| Lifecycle step 4-5, 7 | Guest ready wait with --vm-boot-timeout; inputs copied in over channel (Put); collect declared outputs and read-write mount contents | no | komira-ai/jobscheduler#1187 (new) |
| Lifecycle step 11 / Integrity and taint | VM store check (every store directory is the base or a held run's clone; unaccounted driver process is a finding) and taint on failed destroy | no | komira-ai/jobscheduler#1188 (new) |
| Restart | On start the agent lists clones, stops and destroys each, records a finding, reports no held run | `crates/js-agent/src/vm/runtime.rs:288-300`, `crates/js-agent/src/vm/split.rs:158` | komira-ai/jobscheduler#740 |
| Outcome mapping | Guest exit gives succeeded/failed, stop gives killed, wall limit timed_out, guest oom gives out_of_memory, Gone before Exited is failed and rerun | partly: `crates/js-agent/src/vm/runtime.rs:77` | komira-ai/jobscheduler#740 |
| The guest channel | js-guest-wire frames Put, Exec, Log, Signal, Get/Data, Shutdown, Error, with bounds (1 MiB, 4096 log frames/s) and a fuzzed hostile-frame corpus | superseded: `crates/js-guest-wire/src/frame.rs` | - |
| Base images and priming | Base image manifest (name, digest, js-guest version, profile), immutable names, digest verification on first appearance/change, base_images and image_profiles facts, Start pins digest, mismatch is image-unavailable, in-flight runs keep their VM | no | komira-ai/jobscheduler#1189 (new) |
| 5.15 One accounting | A VM books CPUs, memory and one vms in full on the agent; container memory checked against host total less VM memory; vms is a booked dimension; offer rises only after destroy and falls at admission | `crates/js-sched/src/admission/horizon.rs:155` | komira-ai/jobscheduler#129 |
| Whole-node lease class (VM part) | At preparing the agent confirms no VM is running (a found one is a finding and destroyed) and runs the VM store check; at the end destroys every VM | no | komira-ai/jobscheduler#1188 (new) |
| D26 macOS VM sizing question | Fixed size classes (A) or per-run size (B); vm_sizes flag or lease carries sizes | decision, no code | - |
| Tests (13.1, 13.2) | Fake VmRuntime with scripted outcomes and a conformance suite: no VM serves two runs, destroy before slot free, VMs counted in offer, container refused when VM memory leaves too little, whole-node sequence, restart sweeps every clone, host/GPU VM job refused, mounts in and out, hostile frame corpus never taints | partly: `crates/js-vm-lease/src/fake.rs`, `crates/js-agent/tests/agent/vm/` | - |

## placement.md

As read: No status header. The page has no Designed, Planned, Open question or What-is-left sections and no build steps. It only states current behaviour and two deliberate non-builds.

| Section | Claim | On main | Tracking |
|---|---|---|---|
| 6.4 (large requests) | Large non-whole requests have no reservation today. The design adds none because R1 forbids reservation admission, and scenario F1.5 keeps this as known behaviour. | no | - |
| 6.4 (device leases) | At most one device lease per agent (DEVICE_LEASES_PER_WORKER = 1). A class could raise this once its agent proves isolation. | `crates/js-sched/src/devices.rs:18` | - |
| 6.4 (GPUs) | GPUs are whole-device only. Fractional GPUs are out of scope (D10). | decision, no code | - |
| 6.3 (BookAll) | BookAll books no reservation, and no ucdk Job class uses it (D56). | `crates/js-lease-server/src/agent_pool.rs:256`, `crates/js-registry/src/config.rs:340` | - |
| 6.7 (probe rule) | admit_probe is the one rule, called by both placement engines: the scheduler round and the lease server placer. | `crates/js-sched/src/probe.rs:184`, `crates/js-sched/src/servable.rs:36` | - |
| 6.7 (probe-unsupported) | The removed static pool refused probes with probe-unsupported. The layered provider passes over a child that answers it. | partly: `crates/js-types/src/start.rs:39`, `crates/js-provider/src/book.rs:252` | - |
| 6.7 (js-server-core) | js-server-core is removed from this repository, so the queue-no-probe and unpersisted-pin concerns no longer apply. | superseded: ls crates shows no js-server-core crate. | - |

## tree-inputs.md + estimate-feed.md + ucdk.md

As read: tree-inputs: superseded by job-rootfs.md (D66), nothing to build. estimate-feed: "the feed is gone", flag is a no-op. ucdk: "Superseded (2026-11-24)", nothing built. Only live gap: one stale doc comment (komira-ai/jobscheduler#1130 related).

| Section | Claim | On main | Tracking |
|---|---|---|---|
| tree-inputs header | Whole page: design for owner review, superseded; nothing below is to be built | superseded: `docs/design/job-rootfs.md:917` | komira-ai/jobscheduler#845; komira-ai/jobscheduler#847 |
| 9 build item 1 | Wire types (InputRef::Tree, TreeUnavailable, input_failure, tierstore-trees key, plus A1-A4 fields) built as komira-ai/jobscheduler#842 | `crates/js-agent-wire/src/native.rs:82` | komira-ai/jobscheduler#842; komira-ai/jobscheduler#843; komira-ai/jobscheduler#847 |
| 9 build items 2-6 (server, agent+crun, macOS, e2e run) | Server tree placement, crun overlay attach, macOS Seatbelt tree policy, e2e runbook are not built | superseded: job-rootfs.md:929 and :1256 retire the wire; no hit for avoid_hosts / tree_namespace in crates; macOS trees now via mount provider, mergedprs komira-ai/jobsched | komira-ai/jobscheduler#1028; komira-ai/jobscheduler#1034 |
| 10 decisions TI1-TI24 (incl. A1-A5) | Decisions recorded for owner to overturn | superseded: tree-inputs.md:3-5 retired by job-rootfs.md section 6; no avoid_hosts in crates | - |
| 11 Q1 writable tree on macOS | Open question: copy, link per file, or tierstore views | superseded: macOS now uses read-only link from a mount provider (komira-ai/jobscheduler#1034); tree wire retired | komira-ai/jobscheduler#1034 |
| 11 Q2 macOS frontend mount user | Open: which user mounts the macOS frontend | superseded: tree-inputs.md:3-5; no tierstore fs frontend in js-agent | - |
| 11 Q3 macOS agent starting before mount | Open: plist condition vs re-probe | superseded: tree-inputs.md:3-5; tierstore-trees capability key removed per job-rootfs.md:1118 | - |
| 12 asks of tierstore | Control-socket asks (prefix detach-all-except, prefetched, namespace, reserve) | superseded: tree-inputs.md:3-5; jobscheduler names no node-local cache | - |
| estimate-feed header | The feed is gone; flag and key accepted but do nothing | `crates/js-lease-server/src/config/estimate_feed.rs:33` | komira-ai/jobscheduler#800; komira-ai/jobscheduler#831 |
| 5 default: pool estimate_feed key (design step 8) | Per-pool estimate_feed key to be added with step 8 | superseded: estimate_feed.rs:33 'no lease is costed by an estimate any more'; no pool key in crates | - |
| 5 follow-ups: read path for ended rows, counters stored (step 3), flip default | GetUsage estimated-end field, stored counters, default flip | superseded: `crates/js-usage/tests/tail_cases` | komira-ai/jobscheduler#806; komira-ai/jobscheduler#831 |
| 5 option (b): compute estimate with feed off | Registry computes estimate for the row with feed off | superseded: registry has no lives or history modules | - |
| 6 Q1 several runs on one lease | Open: bias of keyed runs on a lease | superseded: leases gone (komira-ai/jobscheduler#831); lives::estimate_end absent | komira-ai/jobscheduler#831 |
| 6 Q2 read path / Q3 pool on row / Q4 key dominating / Q5 estimate kept on row / Q7 reservation deadline | Open questions about estimate-feed evidence | superseded: estimate-feed.md:3 'the feed is gone'; hosted_row/history.rs absent | - |
| 6 Q6 stale comment on DEFAULT_BACKFILL_MARGIN_MS | Doc comment in js-whole cost.rs says nothing uses the constant yet, but js_sched::backfill uses it | `crates/js-whole/src/cost.rs` | komira-ai/jobscheduler#1134 |
| ucdk.md: Superseded 2026-11-24 | Plan for jobscheduler as second realization of ucdk Jobs (sections 11.1-11.3) dropped; nothing built | superseded: `crates/js-lease-server/src/kinds.rs` | komira-ai/jobscheduler#28; komira-ai/jobscheduler#1082 |

## 14 other pages

absence.md, agent-egress.md, agent-privilege.md, agent-suite.md, aws-iam.md, generalising.md, how-a-job-runs.md, invariants.md, mac-hostkeeper-package.md, providers.md, queue-status.md, registration.md, spawn-disclaim.md, taint-self-clearing.md. Each row's section names its page.

As read: Pages with no open status wording (all built / descriptive), listed with no rows: absence.md (no status claims), queue-status.md ("Status: built"; code at crates/js-sched/src/queue_status.rs:88, proto job.proto:324/359), taint-self-clearing.md ("Status: built"; its "Left out on purpose" items are deliberate non-goals), invariants.md (spec lists only, one tags follow-up row below). agent-egress.md says "implemented". The other pages are listed row by row. Keyword search found few hits, so each page was read in full. Several pages carry stale "next step / later PR / not decided" text that main has already overtaken.

| Section | Claim | On main | Tracking |
|---|---|---|---|
| aws-iam | The aws role's IAM scope (short session and external id, RunInstances only from the configured template and subnets, TerminateInstances only by the scheduler's tag, CreateTags only with CreateAction=RunInstances, read-only describes) is a precondition of step 32, and a test checks the policy document the provider documents against (b) to (d). | partly: `crates/js-provider-ec2/src/provider.rs`, `crates/js-provider-ec2` | komira-ai/jobscheduler#1167 (new) |
| how-a-job-runs (last bullet) | A Linux job is never placed on a Mac; a Mac that runs Linux jobs is a later design. | no | komira-ai/jobscheduler#1066 |
| invariants (13.2 capacity-provider suite) | Tags are present on every created instance (the follow-up named in the js-conformance crate doc). | partly: `crates/js-conformance/src/lib.rs`, `crates/js-elastic/src/conformance.rs:117-132` | - |
| agent-suite: Still prose (now "Outside the agent suite") | Every agent behaviour of the list that the lifecycle can show is a wire-neutral AgentHarness scenario, each with a FakeAgent defect that fails it and passed by the js-agent adapter: result verdicts, usage on failure and on kill, the start-contract and GPU refusals, cleanup before the slot, host-job consent and taint, the tainting checks, the taint kept across acknowledgement and restart, the first heartbeat after a restart, the whole-node wait. The page's "Outside the agent suite" section names where the rest is tested (the scheduler's half; the session, worker and VM runtime) and the VM rules not built yet. | `crates/js-conformance/src/agent/scenarios.rs:81`, `crates/js-conformance/src/agent/scenarios/host.rs`, `crates/js-conformance/tests/js_agent_suite.rs` | komira-ai/jobscheduler#1190 |
| agent-suite: What exists now (js-agent adapter) | The js-agent adapter drives the real lifecycle over an in-memory runtime and a moved clock, but does not go through the session or transport. The fake cancels without a result and does not model grace periods or a cap on retired ids. | `crates/js-conformance/tests/js_agent_suite.rs` | - |
| generalising: Capabilities | The capability comparators become a schema a provider registers, with namespaced keys; a request key unknown to the schema is refused at submit. | partly: `crates/js-caps/src/matching.rs:271`, `crates/js-caps/tests/job_keys.rs:157` | - |
| generalising: Phases | The generic phase set is the wire's. An unknown phase reads as quarantined (fail closed). A host takes new leases only in serving. | `crates/js-agent-wire/src/hello.rs:25-56` | - |
| generalising: Failure taxonomy and Usage | The failure classes map at one point to the wire (InputUnavailable named generically, OutOfMemory, BusyNode); usage carries a basis and censoring signals (limit hit, swap peak, OOM kill count, node pressure at start) and is reported on failure too. | partly: `crates/js-types/src/retry.rs:77`, `crates/js-agent-wire/src/heartbeat.rs:58-61` | - |
| providers: Kinds at a glance | Later elastic kinds (azure, k8s, vsphere, proxmox) are named only and are not built. | no | - |
| providers: Elastic providers step 2 / registration (b) | A registered elastic host is admitted by verify_instance on the cloud's signed instance identity; today the server does not call verify_instance, so the signed identity does not replace the join token. | partly: `crates/js-agent-wire/src/hello.rs:330`, `crates/js-provider-ec2` | komira-ai/jobscheduler#1191 (new) |
| registration: Join credential | A client certificate per pool is the alternative to a shared join token (D18). | superseded: `crates/js-lease-server/src/agent_api/host_cert` | komira-ai/jobscheduler#936; komira-ai/jobscheduler#1118 |
| mac-hostkeeper-package: Where things stand / Release | The signed macOS package, pkg-upload, release desired --pkg-url and hostkeeper-desired set-desired-mac steps. | `.github/workflows/js-agent-macos-pkg.yml`, `.github/workflows/hostkeeper-desired.yml:150` | komira-ai/jobscheduler#913 |
| mac-hostkeeper-package: Order step 5 | Step 5 of Order: kbf asks for os=macos (kbf's executor still asks every job for os=linux; macOS is out of scope in kbf). | no | komira-ai/jobscheduler#888 |
| mac-hostkeeper-package: The pool's users stay for good | Deleting an account per job in the agent is 'the next step' (mac-account-per-job.md); the helper still cannot delete a pre-pool lease user. | `crates/js-agent/src/native/accounts.rs:1-20`, `crates/js-agent/src/mac_accounts/` | komira-ai/jobscheduler#1014; komira-ai/jobscheduler#1023; komira-ai/jobscheduler#1121 |
| spawn-disclaim: The fence, measured / Options | Which way the three TCC-only places (TCC dir, TCC.db, TimeMachine plist) are closed to a job holding js-agent's Full Disk Access is 'not decided' (options a, b, c); option (a) also wants a walker that lists every path read in one start and refused in the other, run per macOS release, with the list committed beside the profile and a check that fails on a place the profile does not deny. | partly: `crates/js-agent/src/native/fence.rs:19-27`, `crates/js-agent/src/native/settings.rs:12` | tracked: komira-ai/jobscheduler#1025 and komira-ai/jobscheduler#1020 |
| spawn-disclaim: Which program keeps its responsibility (VM launcher, GUI-session path) | For the VM launcher (vm/helper.rs) and the GUI-session path (macos/gui.rs, launchctl asuser then disclaim::exec) whether to keep the responsibility is 'not decided'; both stay disclaimed. | decision, no code | - |
| spawn-disclaim: table row (sandbox-exec self-test) and 'Not measured' | The sandbox-exec self-test (native/profile.rs) goes away with the helper path. What macOS does with the responsible process when the agent exits while a supervisor runs is not measured. | no | komira-ai/jobscheduler#991 |
| agent-privilege: top status / section 6 Hardening path | Option A (a non-root agent plus a root helper) is kept as the hardening path for later; buy it by moving the five duties behind helper interfaces one at a time, with traits that both forms implement. | no | - |
| agent-privilege: section 4 (end) / Open items for B | Run image pulls as the job user (the image-pull verb started through the same hook) rather than in the root process. | partly: `crates/js-agent/src/image/pull.rs:1-14`, `crates/js-agent/src/hidden_verbs.rs:10` | komira-ai/jobscheduler#1192 (new) |
| agent-privilege: section 5 PR table (P12a, P14) and Gate | The crun runtime core, the packaging (one unit, a job-user account with a subordinate id block, cgroup chown) and choosing the Searchable layout with the runtime are for later PRs; 'Nothing selects Searchable yet'. | `crates/js-agent/src/lifecycle/run_files.rs:75-77`, `packaging/js-agent/js-agent-crun.service` | - |
| agent-privilege: section 4 duty 7 / Open items (farm AppArmor) | The farm needs the distribution's crun AppArmor profile loaded, or a minimal profile, installed by P14's held host set-up. | `packaging/js-agent/crun_host_setup.sh:18`, `docs/runbooks/crun-host-setup.md:24` | - |
| agent-privilege: section 7 UNVERIFIED | Unverified and left to later PRs or the canary host: an unpack of a real image under the root daemon; the farm's AppArmor policy; unit Delegate= and AmbientCapabilities= (A); a hidepid process table; the egress set-up; the --scratch-quota flag; the farm's kernel. | partly: `.github/workflows/ci.yml:637` | - |
| agent-privilege: Residual risk (one identity for all jobs) | A per-job user or per-job subordinate range would close the same-host-identity risk, and is not part of B. | no | - |
| agent-privilege: section 8 FUSE (what is proven) | In CI the kernel's FUSE mount in the job's namespace, FUSE_INIT, NoNewPrivs and lo-only are unverified, and are shown on the farm by running the smoke test on a node. | `crates/js-agent/tests/crun_runtime/fuse.rs:87`, `crates/js-agent/src/contract.rs:74` | - |
| agent-egress: The held canary (not run here) | A production-host canary of the crun runtime with an allow-list (the endpoint is reached and the metadata address is refused, links and members are gone after a run, egress survives an agent update, and the crun AppArmor profile is loaded) is held and is not run by any pull request. | no | - |
| agent-egress: What is and is not wired (Docker forward policy and farm kernel) | Hosts must not drop forwarded packets in another chain (Docker's forward policy does); the CI job stops Docker. The farm's kernel and AppArmor policy are not exercised in CI. | no | - |
| mac-hostkeeper-package: Where things stand (first bullet) / sign | Signing needs the pkg-signing environment's secrets; without them 'sign' skips and the release has no signed package. Notarization is not required. | `.github/workflows/js-agent-macos-pkg.yml` | komira-ai/jobscheduler#913 |
