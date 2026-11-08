# HTTP gateway

The job service (`jobscheduler.v1.JobService`, crate `js-proto`) is defined once, in
protobuf, and gRPC is its native transport. Scripts and browsers that cannot speak gRPC use
the HTTP/JSON gateway, crate `js-api-gateway`: routes under `/v1/` generated from the
`google.api.http` option each method carries in `service.proto`. No route and no JSON field
is written by hand, so a method appears over HTTP when it is annotated, and a method without
the option has no route.

The gateway is a library: a server mounts its router (`js_api_gateway::router`) beside its
own routes and puts its bearer layer in front. The router matches the whole request path, so
it is merged at the root, not nested under a prefix. The gateway itself
authenticates nothing; an unauthenticated request is the mounting server's 401 with an empty
body. The lease routes (`/v1/leases`, `/v1/agent`, `/healthz`, `/status`) are not part of it,
and its build fails if an option names a path under `/v1/leases` or `/v1/agent`.

Today's lease server has routes of its own under the same resources, with other bodies and
other authentication: `GET /v1/hosts`, `GET` and `POST /v1/hosts/{target}` (which serves
`POST /v1/hosts/{id}:cordon` and `:uncordon`), `GET /v1/pools`,
`POST /v1/pools/{pool}/brake/reset`, `GET /v1/usage/nodes/{id}` and `GET /v1/usage/keys/{key}`.
The gateway's router holds `/v1/hosts`, `/v1/pools` and `/v1/usage` and every path below each
(`/v1/hosts/{*rest}` and so on), so the two routers cannot be merged as they are: axum refuses
the overlapping routes. The server that mounts the gateway beside the lease routes has to
decide which one answers those paths. The lease server therefore does not mount the gateway beside its lease routes: it serves
the gateway and the gRPC service together on a port of their own, `--job-listen`, behind the bearer tokens of its clients
([configuration.md](design/configuration.md)).

## Routes

The table is `js_api_gateway::ROUTES`, in the order of the service definition, which the
gateway's build reads from the `google.api.http` options. `cargo xtask gen-job-surface --out .`
writes it between the markers below, and `cargo xtask gen-job-surface --check --out .` (run by
`cargo test -p xtask --test job_surface`) fails naming this page when it differs.

<!-- gen-job-surface routes begin -->
| route | method of `JobService` | success |
|---|---|---|
| `POST /v1/jobs` | `SubmitJob` | 201 |
| `GET /v1/jobs/{job_id}` | `GetJob` | 200 |
| `GET /v1/jobs` | `ListJobs` | 200 |
| `POST /v1/jobs/{job_id}:cancel` | `CancelJob` | 200 |
| `GET /v1/jobs/{job_id}/result` | `GetResult` | 200 |
| `GET /v1/jobs/{job_id}/logs` | `GetLogs` | 200 |
| `GET /v1/hosts` | `ListHosts` | 200 |
| `POST /v1/hosts/{host_id}:cordon` | `CordonHost` | 200 |
| `POST /v1/hosts/{host_id}:uncordon` | `UncordonHost` | 200 |
| `GET /v1/pools` | `ListPools` | 200 |
| `GET /v1/providers` | `ListProviders` | 200 |
| `GET /v1/usage` | `GetUsage` | 200 |
<!-- gen-job-surface routes end -->

A call that succeeds answers its response with the status of the last column: 200, or the
status the method's `http_success_status` option names. A submit answers 201, also when it is
a repeat that the service answers from the job already made, with the same body, so a client
needs no branch for a first and a repeated submit.

A call that changes state is a POST and a read is a GET. Cancel is
`POST /v1/jobs/{job_id}:cancel`, not `DELETE /v1/jobs/{job_id}`: a cancel keeps the job's
record and its result, where a `DELETE` would read as removal. It is idempotent on the job, so
a repeat answers 200 with the same final state. `GetLogs` without a `stream` answers the job's
state and its result's tails. With a stream it reads one page of that stream from the log
store. Its query parameters are `stream` (`LOG_STREAM_STDOUT` or `LOG_STREAM_STDERR`, the
names of the `LogStream` enum), `fromOffset`, `maxBytes` and `follow`. In the answer `data` is
base64 of the bytes, `offset` and `nextOffset` are decimal strings, `gaps` is a list of
`{"offset", "length"}` objects with decimal strings, and `firstByteAtMs` is a decimal string
when present. The mapping is the generic one of the other calls.

## Requests

- A path variable (`{job_id}`, `{host_id}`) sets the request field it names, percent-decoded
  exactly once (`/v1/jobs/%61` is the job `a`; a malformed escape or text that is not UTF-8
  is a 400 naming the field). An empty path segment (`/v1/jobs/`, `/v1/jobs//result`) is an
  empty value. Literal segments and custom verbs are matched as written.
- The path decides before the method: of the routes whose path matches, the most specific
  count (a custom verb before none, then more literal segments), so `GET /v1/jobs/x:cancel`
  is a 405 of the cancel route allowing POST, not a read of the job `x:cancel`.
- A route whose option has `body: "*"` (every POST here) takes the request message as its JSON
  body (an empty body reads as `{}`) and no query string. A body or query field the path sets (`{"jobId": ...}` on a cancel, `?jobId=` on a
  read) is refused, whatever it holds, `null` and the default included.
- Any other route (every GET here) takes no body. Its query string sets the other fields: `name=value` for a singular
  field (once), the same name repeated for a list, and `name[key]=value` for each entry of a
  map (`labels[]` is the empty key; brackets inside a key are kept), names and values
  percent-decoded once, with `+` read as a space; a malformed escape (`%`, `%4`, `%zz`) is a
  400. A parameter without `=` has an empty value. For example
  `GET /v1/jobs?states=JOB_STATE_QUEUED&states=JOB_STATE_RUNNING&labels[team]=ml&pageSize=50`.
- A field may be named by its JSON name (`pageSize`) or its proto name (`page_size`), not by
  both in one request. A 400 names it by its JSON name either way, and a map entry's
  parameter by the map field alone (`labels`, not `labels[team]`), since the key is a value
  of the request.
- A `job_id` (1 to 63 bytes of letters, digits, dot, underscore and dash) is checked before
  the call, in the path, the body and the query. An empty one in a body or a query is left to
  the service: an empty job id in a submit asks the service to mint one. Any other path
  variable, such as a `host_id`, must not be empty; the service judges the rest.

## JSON mapping

Bodies are the proto3 JSON mapping of the messages:

- fields by their lowerCamelCase JSON names (`jobId`, `wallLimitSeconds`); a request may also
  use the proto names;
- enums by their value names (`"JOB_STATE_RUNNING"`);
- 64-bit integers as decimal strings (`"queuedAtMs": "1800000000000"`), so that no JSON number
  rounds a value above 2^53; 32-bit integers as numbers;
- bytes (the log tails) as base64;
- a field at its default value (0, `false`, empty) left out of a response, but an optional
  field that is present written even at zero;
- an enum value this version does not know written as its number;
- the keys of every object in order, so the same message is always the same bytes.

In a request, `null` leaves a field absent, an object that gives a member twice is refused,
and an integer field takes a JSON number only as an exact integer: a fraction, an exponent or
a number past the 64-bit range (which a JSON parser would round) is a 400, while a decimal
string is read exactly.

A request with a field its message does not have is refused with a 400 that names it, so an
older server tells a newer client that it does not know the field instead of dropping it.

## Limits

A request body over 64 KiB (65536 bytes, the lease server's body limit) is a 413
`request-too-large` before anything is decoded. The gateway adds no buffer of its own for the
request head; the listener in front of it bounds the head, query string included.

## Errors

An answer that is not the call's response is `{"error": "<code>"}`, plus `"field"` (the first
offending field, by its JSON-name path: `spec.jobId` inside the submitted job,
`spec.ports[1].hostPort` in a list, a map field without its key) and `"reason"` (a
fixed text) on a 400. Neither ever repeats a value of the request, and a 500 never says its
cause.

| answer | status | `error` |
|---|---|---|
| a request the gateway cannot decode: malformed JSON, an unknown field, a value of the wrong kind, a bad id | 400 | `invalid-argument`, with `field` when one is to blame |
| a body over 64 KiB | 413 | `request-too-large` |
| no route matches the path under a resource of the gateway | 404 | `not-found` |
| a route matches the path for another method only (with an `Allow` header) | 405 | `method-not-allowed` |
| refusal `job-id-conflict`: the same id with a different job | 409 | `job-id-conflict` |
| refusal `no-capable-host` at admission | 422 | `no-capable-host` |
| refusal `wall-time-not-positive` | 400 | `wall-time-not-positive`, `field` `spec.wallLimitSeconds` |
| refusal `wall-time-above-ceiling` | 400 | `wall-time-above-ceiling`, `field` `spec.wallLimitSeconds` |
| an unknown job | 404 | `unknown-job` |
| a result or logs past retention | 410 | `expired` |
| the result of a job that has not ended | 409 | `not-finished` |
| logs asked of a server whose log sink is `none` | 409 | `no-log-sink` |
| live logs asked of a job that ran without them | 409 | `no-live-logs` |
| a log offset past the end of the stream | 400 | `out-of-range`, `field` `fromOffset` |
| too many follows open at once | 429 | `too-many-follows` |
| the log store cannot be read | 503 | `log-store-unavailable` |
| a fleet call of a client that is no operator (`PERMISSION_DENIED`, `operator-only`) | 403 | `operator-only` |
| an argument the service refuses | 400 | `invalid-argument`, with `field` when the service names one |
| a method the service does not implement | 501 | `unimplemented` |
| a store write that failed | 500 | `state-write-failed` |
| any other failure, a response that cannot be written | 500 | `internal` |
| unauthenticated (the mounting server's bearer layer) | 401 | empty body |

How the service's answer selects a row: a gRPC status that carries a `Refusal` in its details
is answered by the refusal's code. Any other status is answered by its gRPC code and its
message, which the service sets to the code of the table: `NOT_FOUND` with `unknown-job` (404)
or `expired` (410), `FAILED_PRECONDITION` with `not-finished`, `no-log-sink` or `no-live-logs` (409),
`OUT_OF_RANGE` with `field: fromOffset` (400 `out-of-range`), `RESOURCE_EXHAUSTED` with
`too-many-follows` (429), `UNAVAILABLE` with `log-store-unavailable` (503),
`PERMISSION_DENIED` with `operator-only` (403), `INTERNAL` with
`state-write-failed` (500 `state-write-failed`), `UNIMPLEMENTED` (501), and `INVALID_ARGUMENT`
(400, where a message `field: <name>` gives the `field`). Every other status, and a refusal
code this version does not know, is a 500 `internal`. The crate's goldens
(`crates/js-api-gateway/tests/golden/`) hold the whole mapping.

## The lease server's job service

The lease server holds the job service in `crates/js-lease-server/src/job_api/`: one service
object that both transports call, gRPC (the generated `JobService` server) and this gateway,
with one bearer layer in front of both. A request that does not present
`Authorization: Bearer <token>` with the token of one of the server's clients reaches no call:
over gRPC it is answered `UNAUTHENTICATED`, over the gateway 401 with an empty body. The
clients are `default`, whose token is the one the lease routes take (`--token-file`), and each
`--client-token` of the server; a presented token is compared with every client's. A request
that passes goes to its call as that client: `SubmitJob` records the client's name on the job,
and every answer that carries a job shows it as `client` (absent on a job recorded before
clients were named). The name and the role (below) come from the token, never from the
request.

### Clients, roles and token reload

**Roles.** Each client has a role, `client` or `operator`:

| call | `client` | `operator` |
|---|---|---|
| `SubmitJob` | yes | yes |
| `GetJob`, `GetResult`, `GetLogs`, `CancelJob` | its own jobs only | every job |
| `ListJobs` | its own jobs only, whatever the filter | every job |
| `ListHosts`, `CordonHost`, `UncordonHost`, `ListPools`, `ListProviders`, `GetUsage` | `PERMISSION_DENIED` `operator-only` (403 `operator-only`) | yes |

A client's own jobs are the ones recorded with its name. A job recorded with no client
(submitted before clients were named) belongs to no client, so only an operator can read or
cancel it. When a client asks for another client's job, the answer is the same as for an id
with no row: `NOT_FOUND` `unknown-job` (404). The client cannot tell another client's job from
a job that does not exist. Job ids are still one namespace. An operator retrying any job gets
the stored job back. A client retrying an id that another client's job holds gets
`job-id-conflict` (409), whatever the job, so it never reads a job it does not own.

**Token entries and the default role.** A named client gets its role from its entry:

```toml
[[client]]
name = "console"
token_file = "/run/credentials/js/console-token"
role = "operator"            # optional: "client" (the default) or "operator"
```

With flags, the entry is `--client-token NAME=PATH` (role `client`) or
`--client-token NAME:ROLE=PATH`. Any other role is refused at start, naming the entry and never
repeating the value. **An entry with no role is a `client`**, the least privilege. So after an
upgrade, every named client that calls the fleet calls or cancels other clients' jobs (an
operator's console or admin tooling, for example) must be given `role = "operator"` in the
deployment's configuration. Clients that only submit and follow their own jobs (CI, a build
farm) keep the default. The client `default`, whose token is the one in `--token-file`
(`server.client_token_file`), is always an `operator`. That token already opens the lease
routes, the join-token routes and the brake reset, so a narrower role on the job service would
restrict nothing its holder cannot already do.

**Reload.** At every tick the server reads each named client's token file again (the same
tick that reads the agent join tokens again). The bearer layer reads the set in force on every
request. So when a token file is rewritten with a new token, the old token is refused (401,
`UNAUTHENTICATED`) from the first request after the next tick, and the new one is accepted.
This is how a leaked token is revoked without a restart. A reload is all or nothing. If any
file cannot be read, holds no valid token, holds another client's token, or holds a join token
of the agent side, the whole set in force is kept and one warning line names the entry (never
the token). A good set is never replaced by an empty or partial one. A reload that changes a
token logs one info line. Names, roles and paths come from the configuration and are read once
at start, like every other key: adding or removing a client, or changing its role, takes a
restart. The token of `default` is the lease routes' token and is read once at start along with
them.

**Audit.** Each call writes exactly one info line, `job service call`, with these fields:

- `call`: the method.
- `client` and `role`: the caller, from its token.
- `target`: the job id, or the host for `CordonHost` and `UncordonHost`; empty for a listing.
- `job`: the job alone, as before.
- `outcome`: `ok`, `denied` (a client's fleet call, or another client's job, logged as
  `denied` although the answer is `unknown-job`), `not-found`, `refused` (an argument, a
  conflict or a precondition), or `failed` (the server could not answer).
- `code`: the gRPC code.

This includes the state-changing calls `SubmitJob`, `CancelJob`, `CordonHost` and
`UncordonHost`. No line holds a token or its digest. A request whose token is no client's
reaches no call and writes no call line. The job record does not take a `cancelled_by` field:
the line is the record of who cancelled.

Both transports are served on the port of `--job-listen` (off by default). The paragraph
between the markers below is written by `cargo xtask gen-job-surface` from the server's list
of the calls it serves, which a test of the server holds to what its service answers:

<!-- gen-job-surface served begin -->
The server serves every call of the service (`js_lease_server::SERVED_CALLS`).
<!-- gen-job-surface served end -->

A request carries at most 64 KiB over either transport: a larger gateway body is 413
`request-too-large`, and a larger gRPC message is not decoded (`OUT_OF_RANGE`).

| calls | file |
|---|---|
| `GetJob`, `ListJobs`, `GetResult`, `GetLogs` | `read.rs` |
| `SubmitJob` | `submit.rs` (its checks in `validate.rs`) |
| `CancelJob` | `cancel.rs` |
| `ListHosts`, `CordonHost`, `UncordonHost`, `ListPools`, `ListProviders`, `GetUsage` | `fleet.rs` |

### Serving the read calls

`GetJob`, `ListJobs`, `GetResult` and `GetLogs` are served from the job tables (`read.rs`).
They read the store and nothing else: no call changes any state, none takes a lock of the
server, and the only time they read is the service's clock, to tell an ended job past its
retention end.

| case | gRPC | gateway |
|---|---|---|
| a `job_id` that is not a job id | `INVALID_ARGUMENT`, `field: jobId` | 400 `invalid-argument`, `field` `jobId` |
| no job with the id: never submitted, or swept after its retention window | `NOT_FOUND`, `unknown-job` | 404 `unknown-job` |
| `GetResult` or `GetLogs` of a job past its retention end and not yet swept | `NOT_FOUND`, `expired` | 410 `expired` |
| `GetResult` of a job that has not ended | `FAILED_PRECONDITION`, `not-finished` | 409 `not-finished` |
| a `page_token` no listing gave | `INVALID_ARGUMENT`, `field: pageToken` | 400 `invalid-argument`, `field` `pageToken` |
| a read of the store that failed | `INTERNAL`, `internal` | 500 `internal` |

**`GetJob`** answers the job: its spec as submitted (a file's contents left empty), its state
and start failure, its host once placed, whether a cancel was asked for, its times
(queued, placed, started, ended), and why it waits: its queue status (`queueStatus`, a code, a
detail and `sinceMs`), its placement attempts (`attempts`), its newest events (`events`, at most
32) and how many older ones were dropped (`droppedEvents`), as
[design/queue-status.md](design/queue-status.md) describes. `ListJobs` shows each job the same
way. The host silence is not set yet.

**The `expired` window.** An ended job keeps its record and result until its retention end
(the end plus the server's retention window, seven days by default). From that instant
`GetResult` and `GetLogs` answer `expired`, until the sweep, which runs on the server's tick,
deletes the row; then every call answers `unknown-job`, as for a job that never existed (no
tombstone is kept). `GetJob` and `ListJobs` show the job until it is swept.

**`GetResult`** of an ended job answers its stored result: the state it ended in, the exit
code, the reason, the last 8 KiB of standard output and of standard error with whether either
was cut, the times and the resource usage the agent reported. A job that ended without a
result (lost, or failed to start) answers its state and times alone.

**`GetLogs` without a stream** answers the job's state and the result's tails: for an ended
job the same tails as `GetResult` (at most 8 KiB a stream, with `truncated`), and for a job
that has not ended (queued, placed, starting or running) empty tails and `truncated` false,
never an error. Live output is read with a stream.

### Reading logs

`GetLogs` has two forms. Without `stream` (or with `LOG_STREAM_UNSPECIFIED`) it is the call
as it was before streams were added and answers the tails above, with every field below
empty. With `stream` set to `LOG_STREAM_STDOUT` or `LOG_STREAM_STDERR` it answers the job's
`state` and one page of that stream, read from the log store the server's `[logs]` table
names (`docs/design/live-logs.md`, sections 3.6 and 8), with empty tails. The job's row is
read first, by the rules of the table above, so an unknown, swept or expired job answers
as `GetJob` and `GetResult` do, whatever the store still holds (`logs.rs`).

The bytes pass through the service: each page is read from the store with the server's read
key for the answer and kept nowhere, neither in the job tables nor in the server's memory.
A client needs no store credential.

**Offsets.** A stream is addressed by byte offset from its first byte. The request gives
`fromOffset` (absent: 0) and `maxBytes` (absent: 262,144; above 1,048,576 it is cut to
1,048,576; 0 is refused). The answer gives `data`, contiguous bytes exactly as the job wrote
them (no line parsing, no re-encoding), at most `maxBytes` of them; `offset`, the offset of
the first byte of `data`; `nextOffset`, `offset` plus the length of `data`; `gaps`; `eof`;
and `firstByteAtMs`, the time the host's capture first saw the first byte of `data`, from
the chunk it came from (absent for an empty page and for a page read from a stream's final
file). To read a stream exactly once, call again with `fromOffset` set to the `nextOffset`
of the last answer, until `eof`.

**Gaps.** Bytes that are not in the store and never will be (the host dropped them past its
local cap, or they were never uploaded) are reported, not invented: `gaps` holds the range
from `fromOffset` to `offset` when the page skipped one, and `data` begins after it. A range
no object covers yet may still be uploaded; it is waited for (an empty page at
`fromOffset`) until 30 seconds after the object after it was written or the job ended, and
reported as a gap after that. A page ends before a gap, so the next call reports it.

**The end.** `eof` is set once the job has ended and `nextOffset` is the stream's length,
which the agent reported with the result; for a job lost without a result, once 30 seconds
have passed since it ended, where its stored bytes end. A queued job answers an empty page.

**Follow.** With `follow=true`, a page with nothing new (no gap, no data, not at the end) of
a job that has not ended is a long poll: the server reads the store again every second and
answers as soon as there is something or the job ends, and after 20 seconds answers the empty
page; the client calls again from the same `nextOffset`. A job that has ended answers at
once. At most `max_follow` (the `[logs]` key, 256 by default) follows wait at once; one more
is refused with 429 `too-many-follows`, which a client retries after a backoff. A follow whose
client goes away gives its place back at once. The call stays unary: a long poll, not a
stream.

| case | gRPC | gateway |
|---|---|---|
| the server's `[logs]` table has `sink = "none"` | `FAILED_PRECONDITION`, `no-log-sink` | 409 `no-log-sink` |
| a job without live logs: its id has no log prefix, or it ended with a result that reports no stream lengths | `FAILED_PRECONDITION`, `no-live-logs` | 409 `no-live-logs` |
| `maxBytes` of 0 | `INVALID_ARGUMENT`, `field: maxBytes` | 400 `invalid-argument`, `field` `maxBytes` |
| `fromOffset` past the length of an ended job's stream | `OUT_OF_RANGE`, `field: fromOffset` | 400 `out-of-range`, `field` `fromOffset` |
| more than `max_follow` follows waiting | `RESOURCE_EXHAUSTED`, `too-many-follows` | 429 `too-many-follows` |
| the store unreachable, slow, failing, or refusing the read key | `UNAVAILABLE`, `log-store-unavailable` | 503 `log-store-unavailable` |

A store failure never carries the store's words, a URL or a key. The server's log line says
only whether the store refused the read key (an error: check `read_key_file` and `url`) or
could not be read (a warning). The call changes nothing, so a client retries it.

For example, `GET /v1/jobs/build-42/logs?stream=LOG_STREAM_STDOUT&fromOffset=0&follow=true`
answers, while the job runs and has printed `hello\n`:

```json
{"state": "JOB_STATE_RUNNING", "data": "aGVsbG8K", "nextOffset": "6", "firstByteAtMs": "1800000000123"}
```

(`offset` is 0, a default, so the answer leaves it out.)

**`ListJobs`** keeps the jobs in any of the `states` (a state this server does not know
matches none), carrying every one of the `labels` with the same value, submitted by the
`client` of that name when it is given (a job recorded with no client matches none), and
queued in the half-open window from `queuedAfterMs` (in) to `queuedBeforeMs` (out). Jobs come in the order
of their ids. A page holds 100 jobs when the request asks for 0, and at most 1000: a larger
`pageSize` is cut to 1000, not refused. A call reads the table 1000 rows at a time and stops
when its page is full, at the end of the table, or after 10,000 rows; so a page is short only
at the end of the table, where it carries no `nextPageToken`, or at that bound, where it
carries one even if it holds few jobs or none. The page token resumes after the last job or
row the page read; a job submitted between pages appears on a later page if its id sorts
after the token, and no job is listed twice.

### Serving the submit and cancel calls

`SubmitJob` (`submit.rs`, its checks in `validate.rs`) and `CancelJob` (`cancel.rs`) write the
job tables. Each answers only after its one store transaction has committed, and builds its
answer from what the commit returned; neither holds anything of its own outside the store.

| case | gRPC | gateway |
|---|---|---|
| a field of the job that is not valid (the first, in the message's order) | `INVALID_ARGUMENT`, `field: <name>` | 400 `invalid-argument`, `field` `<name>` |
| a wall limit that is absent or 0 | `INVALID_ARGUMENT` with the refusal | 400 `wall-time-not-positive`, `field` `spec.wallLimitSeconds` |
| a wall limit above the server's ceiling | `INVALID_ARGUMENT` with the refusal | 400 `wall-time-above-ceiling`, `field` `spec.wallLimitSeconds` |
| a job id that names a different job | `ALREADY_EXISTS` with the refusal | 409 `job-id-conflict` |
| a job nothing could ever serve | `FAILED_PRECONDITION` with the refusal | 422 `no-capable-host` |
| `CancelJob` of an id with no job, or of another client's job by a `client` | `NOT_FOUND`, `unknown-job` | 404 `unknown-job` |
| `SubmitJob` by a `client` of an id another client's job holds | `ALREADY_EXISTS` with the refusal | 409 `job-id-conflict` |
| a store that failed (the commit, or the read before it) | `INTERNAL`, `state-write-failed` | 500 `state-write-failed` |

**`SubmitJob`**, in order:

1. **Checks.** The image is pinned by digest (`NAME[:TAG]@sha256:<64 hex>`; a tag alone is
   refused naming `spec.image`). The needs are a share of a host or the whole host alone; a
   GPU count needs a class, from `gpuClass` or else the accelerator's model, which the alias
   table resolves (an unknown model passes through). The constraints are in the capability
   vocabulary a lease request speaks: `os` `linux` or `macos`, any `arch` (`aarch64` is
   recorded as `arm64`), an execution mode, an ISA level, an accelerator that does not
   contradict the class, and an isolation (`contained`, or unspecified, which reads as
   contained, and `host`, which is refused as `no-capable-host` unless a registered host
   reported that it runs host jobs; an unknown value is refused naming
   `spec.constraints.isolation`); the ISA level and the accelerator are recorded among the
   job's capabilities, where placement reads them. The wall limit is present, above 0 and at most
   the ceiling, `--max-job-wall` (30 days unless given). The network level is a known one;
   each port is 1 to 65535 and no host port is given twice (refused naming
   `spec.ports[<i>].hostPort` or `.containerPort`); the files are checked as a start's run
   files are; the labels as a lease's are. The job id is empty (the service mints one) or 1
   to 63 bytes of letters, digits, `.`, `_` and `-`, else refused naming `spec.jobId`. A
   refusal never repeats a value of the request. Every check runs before the id is looked
   up, so a retry of a job the ceiling now refuses is refused, not answered.
2. **Idempotency.** A job id the tables hold, with the same job (every field but the
   priority, compared on the job as recorded), is answered with that job as it is now, in
   whatever state, with its own priority, and the same answer as a new job's (OK, 201): a
   retry cannot tell. A different job is `job-id-conflict`. An id is held for as long as its
   job's row is: until the sweep on the server's tick deletes it once its retention window has
   passed (seven days after it ended, by default); after that the id is free. A submit without
   an id has one minted, so repeating it makes a second job.
3. **Admission**, only for a job that would be made: a job that no registered host (busy and
   silent hosts count, tainted and departing hosts do not) and no catalog of the pool's
   launching provider could ever serve is refused `no-capable-host`, and nothing is
   recorded. The catalogs are read from the server's catalog cache, as for a lease, so a
   submit calls no provider; a catalog the cache has not read (never fetched, or its fetch
   failed) may offer, so the job is admitted.
4. **Commit, then answer.** The job is inserted as queued in one transaction; the answer is
   the job as recorded (201 over the gateway). A failed commit is `state-write-failed` and
   nothing was recorded. The call does not wait for placement: placement finds the queued job
   on its own pass.

**`CancelJob`** commits one transaction and answers the job as it then stands. A queued job is
cancelled at once (it has no allocation), with a result of reason cancelled and its retention
end. A placed, starting or running job gets the cancel intent (`cancelRequested`) and nothing
else: from that commit on its host is never sent its start, and its next heartbeat reply
carries the stop, resent until the host stops listing the job; the job is cancelled when the
host reports it stopped, or retires an id it never held. An ended job is answered unchanged
in its state. A second cancel is the first: it commits nothing and answers the same. A failed
commit is `state-write-failed` and the job is as it was.

### The fleet calls

The service's calls that are not about one job are an operator's view of the fleet. They
take the same bearer token as the job calls, and only a client of the role `operator` may call
them: any other client is answered `PERMISSION_DENIED` `operator-only` (403). This is what
`fleet.rs` does with the server's handles; whether the server serves these calls is the
generated paragraph above.

- `ListHosts` lists every host, by id, as `GET /v1/hosts` does (one function reads both), with
  the resources booked on it in `booked`: one `HostBooking` per live allocation (booked or
  releasing, until the host confirms the job's container gone), in the order they were booked,
  then one per lease still held through the lease routes, under the lease id's text. A
  whole-host booking is the host's whole shape. `booked` is read-only: no call creates or ends
  a booking.
- `CordonHost` takes a registered host out of placement for `cordon_secs` (300 when absent,
  cut to 3600, 0 is `INVALID_ARGUMENT` `invalid-cordon`), or renews its cordon without ever
  shortening it; `already_cordoned` says it was cordoned before. The cordon lapses unless it is
  renewed. `UncordonHost` lifts it, and changes nothing on a host that is not cordoned. Both
  are the rules of the host's own `POST /v1/hosts/{id}:cordon` and `:uncordon`, and both answer
  only once the bindings are written: when they cannot be, the answer is `UNAVAILABLE`
  `cordon-unsaved` and the change holds on this server all the same. A host the registry does
  not hold (a static host, or one that never said hello) is `NOT_FOUND` `unknown-host`.
- `ListPools` and `ListProviders` read the status `GET /v1/pools` writes: per elastic pool its
  name, kind, state, reasons, memory, machine count and last tick; per pool's provider its
  credentials check and its own checks, and the machines it launched. Nothing else of the
  status (the next launch's plan, the alerts, the orphans) is carried. `ListProviders` also
  tells what keeps a job waiting on the provider, which `GET /v1/pools` does not write: the
  catalog's fetch time and offer count, the instance types passed over for quota, the offers
  declined for capacity, the pool's brake and cool-down, and the launches whose call to the
  provider is out, with their age (`design/providers.md`, "Reading why a provider launches
  nothing"). A server with no elastic
  pool answers empty lists. Both are served whatever the listen address: unlike
  `GET /v1/pools`, which takes no token and is served only on a private address, they take a
  client's token.
- `GetUsage` reads the ended bookings of leases in the usage store and of jobs in the job
  store (see `usage-store.md`) as one listing, oldest end first (then by id, a lease's record
  before a job's with the same end and id), from `from_ms` included to `to_ms` excluded, of one
  host or one job when asked; a job's record carries the job id in `job_id`. A page holds
  `page_size` records (100 when 0, at most 1000); the filters apply before the page is cut.
  `next_page_token` names the page's last record, its key and which store it came from: the
  next page starts right after it, so no record is read twice or skipped while records are
  written; it is empty on the last page. A token no answer gave is `INVALID_ARGUMENT`
  `invalid-page-token`. A server started with `--usage-off` lists the job store's records
  alone. When either store cannot be read the call is `UNAVAILABLE` `usage-unavailable`.

Over the gateway, a fleet call's `INVALID_ARGUMENT` is 400 `invalid-argument` as for any call,
but the gateway's error table does not name the fleet calls' other refusals yet: `unknown-host`
(`NOT_FOUND`), and `cordon-unsaved`, `store-unavailable` and `usage-unavailable`
(`UNAVAILABLE`), each answer 500 `internal`.

None of the reads takes a write lock, writes a row or counts anything. The hand-written
`GET /v1/hosts` and `GET /v1/pools` routes stay beside these calls with their own bodies; the
removal of the lease routes does not remove them.
