# The agent wire: stay on JSON polling, or move to the protobuf toolchain

Decision paper for step 20 of `jobscheduler-design.md` (section 12.1): "Decide whether `hello` and heartbeat move from JSON
polling to the protobuf toolchain of step 1". Section numbers refer to `jobscheduler-design.md`. This note changes no code. It
describes the wire as the code on main has it, gives the sizes, weighs three options and recommends one.

Every number is marked **measured** (read from a named file or golden text, with the command or count that produced it),
**derived** (computed here, with the arithmetic) or **chosen** (an assumption this note makes so that a number can be derived).
The durations quoted (intervals, timeouts, caps) are constants read from the code. No latency, CPU or deployment figure appears:
none was measured, and none is derived. The limits and constants in section 1 are read from the line that defines each, in the file
named beside it.

## 1. What the wire is today

**Routes and authentication.** The lease server's agent router (`crates/js-lease-server/src/agent_api.rs`, `router`) serves
`POST /v1/agent/hello`, `POST /v1/agent/heartbeat`, and `GET` and `POST /v1/hosts/{target}` (the routes a host calls about
itself: cordon, uncordon, show). Every request carries a bearer in `Authorization`. `authenticate` accepts one of the pool's join
tokens, a one-time join token or a host credential, and answers anything else with a bare 401. The hello is held to a per-credential
rate limit before its body is read (`limit_hello`: 429 `rate-limited` with `Retry-After`; `DEFAULT_HELLO_BURST` 60,
`DEFAULT_HELLO_INTERVAL` 1 s in `crates/js-agent-wire/src/lib.rs`). The session itself is proven in the body, not in a header:
a `Heartbeat` carries the `epoch` and the `session` secret the `Welcome` gave, and `Registry::heartbeat`
(`crates/js-registry/src/registry.rs`) refuses another as `stale-epoch` (409) or `session-mismatch` (403). A body that does not
decode is answered with the route's `invalid-*` code and never with the decoder's message (`decode`, `read`).

**Transport.** One HTTP/1.1 POST of a JSON body, one answer. The agent's `Transport::post` takes a `serde_json::Value`
(`crates/js-agent/src/transport.rs`). `js-client` sends `content_type("application/json")` and opens a new connection for every
request (`max_idle_connections(0)`, `crates/js-client/src/client.rs`), so every heartbeat is a fresh TCP and TLS connection. The
server's TLS offers only `http/1.1` in ALPN (`crates/js-lease-server/src/tls.rs`). The server reads the body with
`serde_json::from_slice` whatever the content type says.

**Messages** (`crates/js-agent-wire`, protocol name `jobscheduler.agent.v1`, `PROTOCOL_MAJOR` 1):

- `Hello`: `protocol` (a `ProtocolRange` of majors), `host_id`, `pool`, `agent_version`, `instance`, `shape` (`WireShape`),
  `capabilities`, `gpu_devices`, `labels`, `phase`, `held` (the runs held), `accepts_host_credential`.
- `Welcome`: `protocol`, `epoch`, `heartbeat_interval_ms`, `grace_ms`, `pool`, `session`, `host_credential`. Over HTTP the welcome is
  bare (200) and a refusal is the API's `{"error", "supported", "field"}` body; `HelloReply` is the transport-neutral form.
- `Heartbeat`: `host_id`, `epoch`, `session`, `phase`, `held` (`HeldRun`), `results` (`RunResult`), `taint`, `exclusive_ready`,
  `exclusive_refused`, `load` (`NodeLoad`, telemetry: a malformed one reads as absent), `disk_gib`, and the per-job lists `jobs`
  (`HeldJob`, each with its `JobResult`) and `retired_jobs`.
- `HeartbeatReply`: `commands`, `result_acks` (`ResultAck`), `taint_ack`, `exclusive`, and the per-job lists `job_acks`
  (`JobResultAck`) and `retired_acks`.
- `Command`, tagged by `type`: `start` (`JobSpec`, keyed by run and lease), `stop` and `cancel` (by `run_id`), and the per-job
  `job_start` (`JobStartSpec`) and `job_stop` (by `job_id`). A tag this version does not know reads as `Command::Unknown`.

The per-job messages ride in major 1 beside the lease and run messages. An agent announces them with the hello capability
`per-job` (`PER_JOB_CAPABILITY`), and the wire crate's rule is that a server sends `job_start` and `job_stop` only to an agent that
reported it; an agent lists a job a job start began only under `jobs`, never in `held` or `results`
(`crates/js-agent-wire/src/per_job.rs`). The agent side is built: the agent reports `per-job` (`crates/js-agent/src/contract.rs`)
and carries out both commands. The server side is not: the lease server on main sends no `job_start` or `job_stop` and answers
with empty `job_acks` and `retired_acks` (`crates/js-registry/src/runs.rs`). The capabilities
`one-job` (`ONE_JOB_CAPABILITY`) and `run-files` gate start fields an older agent would silently ignore.

**Cadence.** The welcome sets the heartbeat interval and the grace; the defaults are `DEFAULT_HEARTBEAT_INTERVAL_MS` 5,000 and
`DEFAULT_GRACE_MS` 15,000, and a grace must exceed two intervals (`grace_covers_heartbeats`). `Welcome::validate` holds the interval
between `MIN_HEARTBEAT_INTERVAL_MS` 250 and `MAX_HEARTBEAT_INTERVAL_MS` 3,600,000 and the grace to at most `MAX_GRACE_MS`
86,400,000. A start rides on the reply to the host's next heartbeat and is resent on every reply until the host lists it (section
5.7, step 3). On any 409 to a heartbeat (the server sends `stale-epoch` or `unknown-host`) or a 403 `session-mismatch`, the agent
says hello again at once if its last heartbeat was accepted (`verdict` in `crates/js-agent/src/session.rs`). Every other 4xx except
403 `pool-full`, 408 and 429 is fatal: 401, 413 `body-too-large`, 400 `invalid-hello` or `invalid-heartbeat`, `unsupported-protocol`
and the other 403s end the session and the agent exits with status 2, because the same message would be refused again (the test
`refused_hello_is_fatal_per_code` holds each code to its outcome). The retryable answers (403 `pool-full`, 408, 429, every 5xx) and a
server that cannot be reached wait a jittered time between half and all of a ceiling that doubles from 1 s (`BASE`) to 60 s (`CAP`),
raised to a server's `Retry-After` but never past `RETRY_AFTER_CAP` 120 s (`Backoff::next`, `crates/js-agent/src/backoff.rs`; the
constant is in `crates/js-agent-wire/src/lib.rs`).

**Limits** (`crates/js-agent-wire/src/limits.rs` unless named):

| Limit | Value | What it bounds |
|---|---|---|
| `MAX_HELD` | 4,096 | held runs, jobs, retired job ids, retired acks in one heartbeat or reply |
| `MAX_RESULTS` | 4,096 | results in a heartbeat; run and job acks in a reply |
| `MAX_COMMANDS` | 1,024 | commands in one reply |
| `MAX_ID_LEN` | 128 bytes | the ids of a message (run, lease, job, allocation, host, pool, instance); `session` has `MAX_SESSION_LEN` 128 and a lease id inside a load `MAX_LEASE_REF_LEN` 41 |
| `MAX_DETAIL_LEN`, `MAX_LOG_STREAM_LEN` | 1,024 bytes; 8 KiB | a result's `detail`; each log stream of a result's tail |
| `MAX_LOAD_BYTES` | 28 KiB | a `NodeLoad` at every cap, serialised |
| `HEARTBEAT_BODY_BUDGET` | 65,536 bytes | the heartbeat body; "the server's own limit must equal it" |
| `BODY_LIMIT` (`js-lease-server/src/api.rs`) | 64 KiB | the request bodies of the agent routes; for hello and heartbeat `read` in `agent_api.rs` answers 413 `body-too-large` |
| `REPLY_LIMIT` (`js-client/src/client.rs`) | 1 MiB | the largest answer the client reads |
| `max_connections` (`js-lease-server/src/config.rs`) | 256 | connections the server holds at once (section 11) |
| `MAX_LIVE_RUNS`, `MAX_FINISHED_RUNS` (`js-registry/src/runs.rs`) | 256, 256 | runs per host, as built (section 11) |

`MAX_HELD` and `MAX_RESULTS` allow far more than a 64 KiB body can carry; the doc of `HEARTBEAT_BODY_BUDGET` says so. The agent
lists at most `RETIRED_LISTED` 96 retired job ids (`crates/js-agent/src/lifecycle/machine/per_job.rs`) and fits its results to what
the body leaves after the other lists, a reserve of `MAX_LOAD_BYTES` and a 4 KiB envelope (`results_budget`), by halving and then
dropping log tails (`Lifecycle::fit_log_tails` in `crates/js-agent/src/lifecycle/machine/tails.rs`). That fitting is not a
guarantee: when no tail is left to cut it stops, whatever the size. The held list, the job list without results, and results without
tails are never cut, so a beat whose non-tail content passes the body limit is sent anyway, refused 413 `body-too-large`, and the
agent exits 2. Section 2 sizes where that happens.

**The rule for change** (`crates/js-agent-wire/src/protocol.rs` and the crate's "Compatibility rule"): servers upgrade before
agents. Within a major, fields are only added; a receiver ignores fields it does not know; a field a sender may leave out has a
default on decode. An unknown value of `Outcome`, `Ack`, `HeldState` or `Command` reads as that enum's `Unknown` variant. The other
enums do not, each on purpose: an unknown `Phase` reads as `Quarantined` (it takes no work; test `unknown_phase_reads_quarantined`);
an unknown exclusive-refusal code reads as `RefusalReason::SetupFailed` (`refusal_codes_round_trip_and_unknown_reads_setup_failed`);
and an unknown `Isolation`, `os` or `arch`, and an unknown value of the run and native bodies' `NetworkLevel`, `Placement`,
`OutputKind` or `InputRef` type (`crates/js-agent-wire/src/native.rs`, none of which has a catch-all variant), fails the decode of
that one start, which then reads as `Command::Unknown` while the commands beside it arrive
(`unknown_isolation_is_typed_error_not_contained` and `unknown_enum_values_leave_known_entries_intact` in `tests/decode.rs`,
`an_unknown_confinement_value_reads_as_an_unknown_command` in `tests/native_job.rs`, `an_unknown_platform_reads_as_an_unknown_command`
in `tests/one_job.rs`). A set of messages is added within a major by new `Command` variants, list fields absent when empty, and a hello
capability, not by a new major, because `ProtocolRange::negotiate` would drop every agent of the older major from a server that left
it. A change that cannot be made that way is a new major.

## 2. Sizes

### How the protobuf sizes are derived

No `.proto` file for the agent wire exists, so the protobuf sizes below rest on a schema this note **chooses**: proto3; each message's
fields numbered from 1 in the order the Rust struct declares them; each enum with `UNSPECIFIED = 0` and its values from 1 in the
order the Rust enum declares them (the convention of `crates/js-proto`); `map<string, string>` for the maps; `Command` a message
whose `oneof` holds one sub-message per variant; repeated integers packed; a zero, empty or absent field not written.

Encoding rules used: a field's tag is 1 byte for numbers 1 to 15 and 2 bytes for 16 to 2,047; a varint takes one byte per started 7
bits of its value (1 byte up to 127, 2 up to 16,383, 3 up to 2,097,151; 2^30 needs 5, 2^34 needs 5, 2^36 needs 6); a string or
sub-message is its tag, its length as a varint, and its bytes; a map entry is a sub-message with key 1 and value 2; a negative
`int32` is written as a 10-byte varint (a `sint32` is not).

JSON sizes are of the compact form `serde_json` writes. For the files under `crates/js-agent-wire/tests/fixtures/v0.1.135/` they are
**measured** with `jq -c . <file> | tr -d '\n' | wc -c` (the files themselves are indented: 568, 375 and 755 bytes by `wc -c`). For
golden text in a test they are **measured** by counting the characters of the literal, or of the compact form of a `json!` value.

### Worked example: the hello (`fixtures/v0.1.135/hello.json`)

| Field (number) | Content | Bytes, derived |
|---|---|---|
| `protocol` (1) | sub-message: `min` 1 (1+1), `max` 1 (1+1) = 4 | 1+1+4 = 6 |
| `host_id` (2) | 6-byte string | 1+1+6 = 8 |
| `pool` (3) | 8-byte string | 1+1+8 = 10 |
| `agent_version` (4) | 5-byte string | 1+1+5 = 7 |
| `instance` (5) | `provider` 3 bytes (5), `instance_id` 6 bytes (8) = 13 | 1+1+13 = 15 |
| `shape` (6) | `cpu_millis` 8,000 (1+2), `memory_bytes` 2^34 (1+5), `gpus` 1 (1+1), `gpu_class` 11 bytes (1+1+11), `max_leases` 4 (1+1) = 26 | 1+1+26 = 28 |
| `capabilities` (7) | one entry: key 2 bytes (4), value 5 bytes (7) = 11 | 1+1+11 = 13 |
| `gpu_devices` (8) | `index` 0 (not written), `uuid` 5 bytes (7), `model` 4 bytes (6), `memory_bytes` 2^36 (1+6) = 20 | 1+1+20 = 22 |
| `labels` (9) | one entry: key 4 bytes (6), value 2 bytes (4) = 10 | 1+1+10 = 12 |
| `phase` (10) | `SERVING` = 1 | 1+1 = 2 |
| `held` (11) | `run_id` 5 bytes (7), `state` `RUNNING` = 2 (2) = 9 | 1+1+9 = 11 |
| **Total** | 6+8+10+7+15+28+13+22+12+2+11 | **134** |

JSON: **451** bytes, measured.

### Worked example: the welcome (golden text in `welcome_and_hello_reply_wire_forms`, `tests/golden.rs`)

`protocol` 1 (1+1 = 2), `epoch` 9 (2), `heartbeat_interval_ms` 5,000 (1+2 = 3), `grace_ms` 15,000 (1+2 = 3), `pool` 8 bytes
(1+1+8 = 10), `session` 8 bytes (10): 2+2+3+3+10+10 = **30** bytes, derived. JSON: **109** bytes, measured (the compact form of
the `json!` value of the bare `Welcome`, which is what the hello route sends; the tagged `HelloReply` form, with `"type":"welcome"`,
is 126).

### Worked example: a heartbeat with a result and a taint (`fixtures/v0.1.135/heartbeat.json`)

| Field (number) | Content | Bytes, derived |
|---|---|---|
| `host_id` (1) | 6-byte string | 8 |
| `epoch` (2) | 3 | 2 |
| `session` (3) | 8-byte string | 10 |
| `phase` (4) | `DRAINING` = 5 | 2 |
| `held` (5) | `run_id` 5 bytes (7), `state` `EXITED_UNACKED` = 3 (2) = 9 | 1+1+9 = 11 |
| `results` (6) | `run_id` 5 bytes (7), `lease_id` 3 bytes (5), `outcome` `TIMED_OUT` = 4 (2), `exit_code` 137 (1+2), `detail` 10 bytes (12) = 29 | 1+1+29 = 31 |
| `taint` (7) | `reason` 14 bytes (16), `run_id` 5 bytes (7), `check` 9 bytes (11) = 34 | 1+1+34 = 36 |
| **Total** | 8+2+10+2+11+31+36 | **100** |

JSON: **304** bytes, measured.

### Worked example: an idle heartbeat and an idle reply

The golden text of `exclusive_wire_form` (`tests/golden.rs`) has a heartbeat with empty `held` and `results` and an
`exclusive_ready` of `"7-1"`: 117 bytes, measured. Without `,"exclusive_ready":"7-1"` (24 bytes) it is the idle heartbeat:
117 − 24 = **93** bytes JSON, derived. Its phase is `draining`; a serving host's is one byte shorter, 92. In protobuf only the first
four fields are written: 8+2+10+2 = **22** bytes, derived (the same for either phase).

The reply golden text of the same test, `{"commands":[],"result_acks":[],"taint_ack":false,"exclusive":"7-1"}`, is 68 bytes, measured;
without `,"exclusive":"7-1"` (18 bytes) the idle reply is 68 − 18 = **50** bytes JSON, derived. In protobuf every field of an idle
reply is empty or false and none is written: **0** bytes, derived (a `taint_ack` of true would be 2).

### Worked example: a reply with commands (`fixtures/v0.1.135/reply.json`)

The start, a `JobSpec` (`run_id` 1, `lease_id` 2, `image` 3, `run` 4, `os` 5, `arch` 6, `native` 7, `args` 8, `limits` 9,
`gpu_indices` 10, `mounts` 11, `files` 12, `isolation` 13, `labels` 14, `heartbeat_seq` 15, `valid_for_ms` 16, `bundle` 17):

| Field (number) | Content | Bytes, derived |
|---|---|---|
| `run_id` (1), `lease_id` (2) | 5 and 3 bytes | 7 + 5 = 12 |
| `image` (3) | 84 bytes (`ubuntu:24.04` 12, `@` 1, `sha256:` 7, 64 hex digits) | 1+1+84 = 86 |
| `args` (8) | two strings of 4 and 3 bytes | 6 + 5 = 11 |
| `limits` (9) | `cpu_millis` 2,000 (1+2), `memory_bytes` 2^30 (1+5), `pids` 512 (1+2), `wall_ms` 60,000 (1+3) = 16 | 1+1+16 = 18 |
| `gpu_indices` (10) | packed, one value 0 | 1+1+1 = 3 |
| `mounts` (11) | `root_name` 5 bytes (7), `rel` 8 bytes (10), `ro` true (2) = 19 | 1+1+19 = 21 |
| `isolation` (13) | `HOST` = 2 | 2 |
| `labels` (14) | key 4 bytes (6), value 5 bytes (7) = 13 | 1+1+13 = 15 |
| `heartbeat_seq` (15) | 7 | 1+1 = 2 |
| `valid_for_ms` (16) | 10,000; a 2-byte tag | 2+2 = 4 |
| **JobSpec** | 12+86+11+18+3+21+2+15+2+4 | **174** |

The `Command` around it: `start` (1) is 1+2+174 = 177 (the length 174 is a 2-byte varint), and the `commands` (1) entry of the reply
is 1+2+177 = 180. A `stop` with `run_id` `"r1"`: inner 1+1+2 = 4, in the `Command` 1+1+4 = 6, as a `commands` entry 1+1+6 = 8; a
`cancel` the same, 8. `result_acks` (2): `run_id` 2 bytes (4), `ack` `UNKNOWN_LEASE` = 4 (2) = 6, as an entry 1+1+6 = 8. `taint_ack`
(3) true: 2. Reply: 180+8+8+8+2 = **206** bytes, derived. JSON: **559** bytes, measured.

### A busy heartbeat

Two shapes, both with the envelope of the idle heartbeat (93 bytes JSON, 22 protobuf) and ids at `MAX_ID_LEN` (128 bytes):

- one held run, `{"run_id":"<128>","state":"exited_unacked"}`: JSON 11+128+11+14+2 = 166 bytes; protobuf `run_id` 1+2+128 = 131
  and `state` 2, so 133 inside and 1+2+133 = 136 as an entry. Derived.
- one result with no `exit_code`, `detail` or `log_tail`, `{"run_id":"<128>","lease_id":"<128>","outcome":"timed_out"}`: JSON
  11+128+14+128+13+9+2 = 305 bytes; protobuf 131+131+2 = 264 inside, 1+2+264 = 267 as an entry. Derived.

The shape `a_load_at_every_cap_with_96_held_and_8_results_stays_under_the_budget` (`tests/hostile_load.rs`) holds to 48 KiB, without
its load: 96 held runs and 8 results. JSON 93 + 96×166 + 95 commas + 8×305 + 7 commas = **18,571** bytes; protobuf
22 + 96×136 + 8×267 = 22 + 13,056 + 2,136 = **15,214** bytes. Derived. That test adds a `NodeLoad` at every cap, which
`MAX_LOAD_BYTES` bounds at 28 KiB of JSON; its protobuf size is not derived here (open point 5).

At the documented list maxima, `MAX_HELD` 4,096 held runs and `MAX_RESULTS` 4,096 results of the shapes above: JSON
93 + 4,096×166 + 4,095 + 4,096×305 + 4,095 = **1,937,499** bytes; protobuf 22 + 4,096×136 + 4,096×267 = **1,650,710** bytes.
Derived. They are 1,937,499 ÷ 65,536 = 29.6 and 1,650,710 ÷ 65,536 = 25.2 times the 65,536-byte body the server reads, so **the
body limit, not the list caps and not the encoding, is what bounds a heartbeat**. Within it, with the envelope, n held runs take
92 + 167n bytes of JSON (93 + 166n + n − 1 commas), so (65,536 − 92) ÷ 167 = 391 fit, and 22 + 136n of protobuf, so
(65,536 − 22) ÷ 136 = 481 fit (both rounded down): 481 ÷ 391 = 1.23 times as many. Derived. Past those counts the beat is over the
limit with no tail left to cut, the server answers 413 and the agent exits 2 (section 1), in either encoding; protobuf moves the
point, it does not remove it. The registry sends a host at most `MAX_LIVE_RUNS` 256 live runs, and 256 held runs at 128-byte ids are
92 + 167 × 256 = 42,844 bytes of JSON, inside the limit; a host that adds results, jobs and a load to that comes nearer. Derived.

Log tails are where the two differ most. JSON writes a control character that has no short escape (anything but `\b`, `\t`,
`\n`, `\f`, `\r`, which take 2 bytes) as `\u00XX`, six bytes for one, and `tails.rs` says so; a protobuf `string` holds the raw
byte. In the worst case two streams of 8 KiB of such characters are 2×8,192×6 = 98,304 bytes of JSON, more than the whole body (the
agent cuts them to fit, `results_budget`), and 2×(1+2+8,192) = 16,390 bytes of protobuf for the two strings, 1+3+16,390 = 16,394 with
the `LogTail` sub-message around them. Derived.

### Summary

| Message | JSON bytes | Protobuf bytes (derived) | Protobuf ÷ JSON |
|---|---|---|---|
| hello (`hello.json`) | 451 measured | 134 | 0.30 |
| welcome (golden text) | 109 measured | 30 | 0.28 |
| heartbeat with a result and a taint (`heartbeat.json`) | 304 measured | 100 | 0.33 |
| idle heartbeat | 93 derived from golden text | 22 | 0.24 |
| idle reply | 50 derived from golden text | 0 | 0 |
| reply with a start, a stop, a cancel, an ack, a taint ack (`reply.json`) | 559 measured | 206 | 0.37 |
| heartbeat, 96 held and 8 results, 128-byte ids, no load | 18,571 derived | 15,214 | 0.82 |
| heartbeat at `MAX_HELD` and `MAX_RESULTS`, 128-byte ids | 1,937,499 derived | 1,650,710 | 0.85 |

Small messages shrink to a third, because protobuf drops the field names and quotes; messages made of long ids shrink by a sixth,
because the ids are the same bytes in both.

### Traffic at 256 hosts

At the default interval of 5,000 ms a host makes 60 ÷ 5 = 12 exchanges a minute and 86,400 ÷ 5 = 17,280 a day. The fleet of 256
hosts is **chosen**: it is the connection limit of section 11, as the question for this note asks. Body bytes only (derived):

| Case | One exchange (beat + reply) | One host a day | 256 hosts, per second | 256 hosts a day |
|---|---|---|---|---|
| idle, JSON | 93 + 50 = 143 | 143 × 17,280 = 2,471,040 | 143 × 256 ÷ 5 = 7,321.6 | 632,586,240 |
| idle, protobuf | 22 + 0 = 22 | 380,160 | 1,126.4 | 97,320,960 |
| busy (96/8 beat and the `reply.json` reply), JSON | 18,571 + 559 = 19,130 | 330,566,400 | 979,456 | — |
| busy, protobuf | 15,214 + 206 = 15,420 | 266,457,600 | 789,504 | — |
| every beat at the 65,536-byte body limit, either encoding, beats only | 65,536 | 65,536 × 17,280 = 1,132,462,080 | 65,536 × 256 ÷ 5 = 3,355,443.2 | — |

At the shortest interval a welcome may set (250 ms) every per-second figure is 20 times larger. Not counted, and the same for
options A and B: HTTP request and response heads, TLS records, and the TCP and TLS handshake that every exchange makes because
`js-client` opens a new connection per request. Their size is not derived here (open point 1).

## 3. What a change would buy and cost

**(A) Stay JSON polling.** Nothing changes.

- *Correctness.* Both ends are Rust in this repository and link the same `js-agent-wire` types, so there is already one schema for
  both ends. What is hand-kept is each field's serde attributes (`default`, `skip_serializing_if`, `serde(other)`): a missing
  `default` on a new field would make every older peer's body unreadable. The goldens (`tests/golden.rs`, `tests/per_job.rs`) pin
  each name and tag. `tests/previous_release.rs` decodes the bodies kept in `tests/fixtures/v0.1.135` with the current types and
  the current types' bodies with a copy of that release's structs; `tests/one_job_previous_release.rs` does both ways for starts,
  from `tests/data/previous_release_starts.jsonl`; `tests/native_old_agent.rs` checks with a frozen copy that an agent from before
  the `native` body refuses a native start.
- *Compatibility.* The capability rule above, used for `run-files`, `one-job` and `per-job` within major 1. The per-job set is
  tested both ways against the previous release's bodies; its server side is not built yet (section 1).
- *Size and CPU.* The JSON column of section 2. The busy case is 979,456 bytes a second, about 1 MB/s, for 256 hosts at the
  default interval, and the ceiling is set by the body limit in either encoding.
- *Tooling.* None added.
- *Debuggability.* A body is text, and the previous-release fixtures are readable in review. Production code does not print bodies:
  `Answer`'s `Debug` leaves the body out (`transport.rs`), the session secret goes into the heartbeat body and nothing else
  (`session.rs`), and the server never repeats a decode error (`agent_api.rs`). Readability helps in tests, fixtures and a debugging
  proxy, not in today's logs.
- *Live update.* Unchanged: servers first, then agents, each release additive.

**(B) Protobuf messages over the same HTTP polling routes.**

- *Correctness.* A `.proto` file becomes the schema and prost generates the structs. The trust-boundary checks do not go away:
  every `validate()` (`lib.rs`: "the server and the agent must call it on every message they receive") and every unknown-value rule
  of section 1 (`Unknown` variants; `Phase` to `Quarantined`; `RefusalReason` to `SetupFailed`; an unknown `Isolation`, `os`, `arch`,
  `NetworkLevel`, `Placement`, `OutputKind` or `InputRef` refusing its one start) must be kept by hand over the generated types,
  because proto3 enums are open (an unknown value is kept as its number) and a field left at zero is indistinguishable from a field
  not sent. The decode tests named in section 1 and the previous-release tests would have to pass against the protobuf mapping too. `Option<i32>` (`exit_code`) and every `Option` field would need
  `optional`, and a negative `exit_code` costs 10 bytes as `int32` unless it is `sint32`. Either the domain code moves onto the
  generated types or a hand-written mapping layer joins them to today's types; both are new code to get wrong.
- *Compatibility.* The server reads every body as JSON today, so a new agent that sends protobuf to an older server is answered
  400 `invalid-hello` or `invalid-heartbeat`, which the agent treats as fatal and exits 2 (`session.rs`). An agent must therefore
  not switch until it knows the server reads protobuf: a hello capability and a new optional welcome field, or a second route.
  JSON stays served for as long as any agent of an earlier release runs. Dropping JSON later removes a shipped encoding, which the
  rule of section 1 allows only as a new major, and `negotiate` then drops every major-1 agent from that server.
- *Size and CPU.* The protobuf column of section 2: a third of the bytes for small messages, five sixths for id-heavy lists, 1.23
  times as many held runs in the body limit, and log tails that are never inflated by escaping. Encode and decode work is not
  figured here.
- *Tooling.* The workspace already has prost 0.14, tonic-prost-build and protox, a compiler written in Rust that `crates/js-proto`
  runs from its build script, so no compiler binary is installed. `js-proto` cannot hold the agent messages: it depends on `tonic`,
  which a pure crate must not reach (`FORBIDDEN` in `crates/xtask/src/layering.rs`), and `js-agent-wire` is in the pure layer of
  `layers.toml`. prost is not in `FORBIDDEN`, and the closure check walks normal dependencies only ("not dev, not build"), so a
  second generated crate in the pure layer, or a build script in `js-agent-wire`, is allowed. It adds a `.proto` file, a build script,
  a schema test like `crates/js-proto/tests/schema.rs`, a second codec in `Transport` (which takes a `serde_json::Value` today) and in
  `js-client` (JSON only), and a content-type branch in both agent routes.
- *Debuggability.* A body needs the schema to read; fixtures become binary or protobuf text format.
- *Live update.* Server learns protobuf first and answers each request in the encoding it was sent in; agents switch only after the
  server says it reads protobuf; both encodings are served across every rolling update until the last JSON agent is gone. A server
  rolled back to a release without protobuf is the hard case: an agent that already switched sends its next heartbeat in protobuf,
  the older server reads it as JSON and answers 400 `invalid-heartbeat`, and the agent exits 2 before any welcome could renegotiate.
  So the choice of encoding would have to be per session (reset to JSON at every hello, chosen again from each welcome), and the
  agent would have to answer a 400 to a protobuf heartbeat by saying hello again in JSON instead of ending; or a server rollback
  across that release would be declared unsupported. The design does not decide this (open point 8).

**(C) A gRPC stream per agent (push instead of poll).**

- *Correctness.* As (B) for the messages, plus a second session model. An open stream is not proof that the agent's session loop
  runs: an HTTP/2 ping is answered by the HTTP/2 stack whether or not it does, while today a heartbeat is sent only by that loop,
  which ends the session when its worker has ended or stalled (`Worker::stalled`, `session.rs`). Presence must therefore still come
  from application heartbeats inside the stream.
- *Delivery.* Polling never relies on one reply arriving: the server resends a start on every reply until the host lists it and a
  stop until it stops listing it (section 5.7, step 3), and the agent may drop a whole reply when its worker's queue (`QUEUE` 16 in
  `crates/js-agent/src/worker.rs`) is full, noted as `Note::Busy`, because the next reply carries it again (`Session::accept`). A
  stream that sends each command once would lose this: a write that succeeded before the stream broke, or a command that met a full
  queue, does not show that the worker took it. A stream would have to keep the same rule, holding every start and stop until the
  host's listing shows it taken and sending it again after a reconnect, and the agent would still rely on starts being idempotent on
  the job id. Those are the compatibility cases a stream must be tested on: a break after a command is written, a command that meets
  a full queue, and a repeated start after a reconnect.
- *Compatibility.* A new service beside the polling routes. Old agents keep polling, so the server runs both session models for as
  long as old agents run.
- *Size and CPU.* Message bytes as (B). The handshake per exchange goes away: one connection per agent instead of one per beat.
- *Tooling.* tonic is already a dependency of `js-proto` (codegen only in its normal dependencies; its tests enable the transport
  feature). The server's TLS offers only `http/1.1` in
  ALPN, and the listener's limits are written for HTTP/1.1 requests (`listener.rs`: `header_timeout` 10 s, `read_timeout` 30 s,
  `max_requests` 100); they would need a rule for a stream that stays idle between beats, which may be up to an hour apart. The
  agent's `Session` is a synchronous state machine over a `Transport` (implemented with `ureq` in `js-client`) that "never sleeps
  and reads no clock"; a stream would need an async client in the agent (the agent already links tokio) and a new session loop.
- *Connections.* Section 11's limit is 256 connections and `max_connections` defaults to 256. With one standing stream per agent,
  256 hosts would take every slot, and clients, status pages and the hosts' own `/v1/hosts/{id}` calls queue in the kernel. Today an
  exchange holds a connection only while it runs.
- *Debuggability.* As (B), and a stream's history is not one request and one answer.
- *Latency.* Today a start or stop waits for the reply to the next beat. When exchanges succeed that wait is at most one interval
  plus the exchange (5 s at the default; derived from section 5.7, step 3, `DEFAULT_HEARTBEAT_INTERVAL_MS`, and `Session::beat_done`,
  which waits the interval after an exchange ends); a failed exchange adds its backoff and a request may take up to `REQUEST_LIMIT`
  10 s (`transport.rs`), so one interval is not a bound. A stream removes the polling wait; it does not promise delivery at once,
  since transport delay, a broken stream and a full worker queue remain. The design states no goal for this delay.

**What a stream would change about section 6.3 (presence and loss).** Section 6.3 makes a host lost when it misses the grace, and
says "Nothing times out for silence alone" for jobs. With a stream, a break would be a new, earlier signal, but it must not shorten
the grace: a break followed by a reconnect inside the grace is the equivalent of today's lost heartbeat that the next one repairs,
and treating the break as loss would make a network blip end a session that polling keeps. Whether a reconnect resumes the session
(same epoch and secret) or says hello again (new epoch) is a choice the design does not make (open point 6). The grace would have to
stay counted from the last application message heard, so `grace_covers_heartbeats` would still apply.

**What a stream would change about section 6.7 (restart).** Today agents learn of a restart at their next heartbeat (409
`unknown-host`) and say hello again, so the hellos of a fleet are spread over one interval by the phase of each agent's beat. With
streams every stream would break at the same moment, and a client that reconnects without jitter would say hello at once; the
backoff's jitter (`Backoff::next`) spreads only retries after a failure, not the first reconnect. The hello rate limit admits a burst
of 60 per credential and then one a second, answering the rest 429, so hosts that share a pool join token would be admitted over
minutes (196 hosts past the burst at one a second, derived from 256 − 60), while hosts with their own credentials would all arrive at
once. The seating of hosts as `awaiting` until their hello (6.7, step 4) would be unchanged.

## 4. Recommendation

**Stay on JSON polling (A).** The reasons, from sections 2 and 3:

1. What bounds a heartbeat is the 65,536-byte body the server reads, in either encoding. Protobuf would fit 1.23 times as many
   id-keyed entries and whole log tails of binary output. In JSON, the beat of 96 held runs and 8 results is 18,571 bytes by
   derivation, and the test that adds a load at every cap holds the whole beat to 48 KiB, three quarters of the body; the agent
   already fits results to the body.
2. The traffic at 256 hosts is 7,321.6 bytes a second idle and 979,456 for every host busy at once, in body bytes; protobuf saves
   7,321.6 − 1,126.4 = 6,195.2 and 979,456 − 789,504 = 189,952 of those (derived). No limit has been measured that these figures
   press on.
3. Both ends are Rust and share one crate, so a `.proto` would add a second statement of a schema that is already single, while the
   validation layer, the enum exceptions and the compatibility tests stay hand-written in either case.
4. The live-update path of (B) and (C) serves two encodings, or two session models, for as long as older agents run, and ending JSON
   is a new major that drops every major-1 agent. (A) has a rule for adding message sets within major 1 that has been used for
   `run-files`, `one-job` and `per-job`, with the per-job set tested both ways against the previous release's bodies.

**The rule from here.** A new message set is added within major 1, not by a new major: new `Command` variants (an older agent reads
each as `Command::Unknown` and keeps the rest of the reply), new list fields absent when empty with a default on decode, and a hello
capability without which the server sends none of the set. A field an older receiver would silently ignore cannot carry something
the job needs; it is gated by a capability, as `run-files` and `one-job` are. The tests that guard it: the goldens in
`tests/golden.rs` and `tests/per_job.rs` (`job_messages_have_the_documented_wire_form`), both directions against the previous
release in `tests/previous_release.rs` (`previous_agent_wire_is_still_accepted_by_the_new_wire_types` and
`previous_agent_wire_reads_the_new_wire_types_without_error`), `tests/one_job_previous_release.rs`, `tests/native_old_agent.rs`,
`per_job_messages_ignore_what_they_do_not_know` in `tests/per_job.rs`, and the negotiation tests of `tests/protocol.rs`. The
previous-release side of those tests is a copy of that release's structs and its bodies kept as files, not the released binary.

**When the recommendation changes.**

- *A measured limit.* Real heartbeats refused 413 `body-too-large`, or results whose tails the agent cuts to fit, at a rate that
  matters. The first answers are inside JSON (a larger body limit, fewer or shorter list entries per beat); protobuf buys 1.23 times
  for ids and more only for escaped tails.
- *A second implementation of the agent* in another language, or a third party writing one. Then a `.proto` with generated types
  is the cheapest way to hold two codebases to one schema, and (B) is the move.
- *A latency goal* for delivering a start or stop below one heartbeat interval. The first answer is a shorter interval within the
  limits (down to 250 ms) and its cost in exchanges; a stream (C) only if that cost is measured to be too high.
- *A measured cost of the per-exchange handshake.* The answer is connection reuse in `js-client` for the agent (it opens a new
  connection per request today; the listener's 10 s `header_timeout` would keep a connection open across 5 s beats), not an
  encoding change.

**The follow-up changes each outcome creates.**

If the decision is (A):

1. Record the decision in section 3.6 and step 20 of the design (the design's owner edits it; this note does not).
2. Build the server side of the per-job set (step 28, which also drops the lease and run split from `js-agent-wire`) and decide how
   the run messages leave major 1 before step 30 deletes the lease routes: a server without the run path must refuse a hello that
   lacks `per-job` by a named refusal rather than admit an agent it cannot give work (open point 3), with a test of that refusal
   against the previous release's hello fixture.
3. Move the previous-release fixtures to the first release whose agent sends the per-job lists, so the both-direction tests cover
   per-job bodies as a released agent wrote them (`tests/fixtures/v0.1.135` predate them; open point 7).
4. Bound what a heartbeat lists besides tails so that a beat whose non-tail content passes the body limit cannot be built (section 1:
   it is refused 413 and the agent exits 2), in whichever encoding is kept; open point 2 asks how near real beats come.

If the decision is (B), in this order:

1. A `.proto` for the agent wire with its field numbers frozen, in the pure layer, with a schema test like
   `crates/js-proto/tests/schema.rs` and a decision on mapping versus moving the domain code onto generated types.
2. The server reads and answers protobuf on the same routes, chosen by `Content-Type`, beside JSON; a test that the previous
   release's JSON fixtures still decode and are answered in JSON.
3. A hello capability and an optional welcome field by which the server says it reads protobuf; the agent switches only after it.
   Tests: a new agent against a server of the previous release (stays on JSON, is not refused) and the previous release's agent
   against the new server.
4. When no agent of an earlier release remains, a major 2 without JSON; deleted then: the serde derives, the JSON fixtures and frozen
   copies, and the JSON branch of `Transport` and `js-client`.

If the decision is (C), in this order: (B) steps 1 and 3 for the messages; a gRPC agent service with HTTP/2 in ALPN and listener
limits for long-lived streams; `max_connections` set above hosts plus clients; a decision on session resume across a reconnect
(open point 6); application heartbeats kept in the stream for presence; commands held and resent until the host's listing shows
them, with the tests of the delivery cases above; the polling routes served beside the stream within major 1. Deleting the polling
routes removes the only transport of every released agent, so it is a major 2 like (B)'s step 4, and the server then speaks major 2
only. An older agent can only learn that through the route it calls, so `POST /v1/agent/hello` stays as a minimal legacy handler
that reads no body and answers every request 400 `unsupported-protocol` with `"supported"` naming major 2, the answer
`ProtocolRange::negotiate` gives today when the ranges do not meet. An older agent that returns later (a host restored from an old
image, say) gets that answer at its hello, which its `verdict` treats as fatal and prints with the majors the server speaks
(`Fatal`, `session.rs`), and it exits 2 until it is updated. Without the handler the hello would get a 404, also fatal to an older
agent, but with no code to say why. Deleted then: `POST /v1/agent/heartbeat`, the hello's decoding and admission behind the legacy
handler, the polling session code in the agent and the server, and the JSON fixtures and frozen copies. A test sends the previous
release's hello fixture to that handler and expects the 400 and its body.

## 5. Open points

1. **Header and handshake bytes per exchange.** How large are the HTTP heads and the TLS handshake that each exchange makes? Settled
   by an in-process test that counts the bytes `js-client` writes and reads against the lease server on a loopback listener
   (`crates/js-client/src/client.rs`, `max_idle_connections`; `crates/js-lease-server/src/tls.rs`).
2. **How near real beats come to the body limit.** Nothing counts 413 `body-too-large` answers or the tails the agent cuts. Settled
   by reading `read` in `crates/js-lease-server/src/agent_api.rs` and `results_budget` in
   `crates/js-agent/src/lifecycle/machine/tails.rs` against the hosts' own logs, which this note does not do.
3. **How the run messages leave major 1 (steps 28 and 30).** Does a server without the run path refuse a hello without `per-job`, and with
   which code? Settled in `Registry::hello` (`crates/js-registry/src/registry.rs`) and `ProtocolRange::negotiate`
   (`crates/js-agent-wire/src/protocol.rs`).
4. **Presence of zero values under (B).** Which fields need proto3 `optional` so that zero and absent stay distinct (`exit_code`,
   `disk_gib`, `epoch` 0, a GPU `index` 0)? Settled field by field against the `Option` fields and `serde(default)`s of
   `crates/js-agent-wire/src`.
5. **The protobuf size of a `NodeLoad` at every cap.** Not derived here. Settled by the same counting over
   `crates/js-agent-wire/src/load.rs` and the maximal load of `a_load_at_every_cap_with_96_held_and_8_results_stays_under_the_budget`.
6. **Session resume under (C).** Does a reconnect inside the grace keep the epoch and secret, or say hello again? Settled in the
   session rules of `Registry::heartbeat` and `Registry::hello` (`crates/js-registry/src/registry.rs`) and the hello-again rule of
   `crates/js-agent/src/session.rs`.
7. **Which release the previous-release fixtures should be.** `tests/fixtures/v0.1.135/README.md` names the release they were taken
   from; whether a release after it sends `per-job` is settled by that release's `crates/js-agent-wire/src/per_job.rs`.
8. **Server rollback under (B).** Is a server rollback across the release that adds protobuf supported, and if so does the agent reset
   its encoding at every hello and answer a 400 to a protobuf heartbeat by a JSON hello? Settled in `verdict` and the hello-again rule
   of `crates/js-agent/src/session.rs`, and by the project's rollback policy for the server (section 6.6 leaves the server's update
   path open).
