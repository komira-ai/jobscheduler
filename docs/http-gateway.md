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
decide which one answers those paths; the gateway is not mounted anywhere yet.

## Routes

The table is `js_api_gateway::ROUTES`, in the order of the service definition; a test of the
crate fails when this page differs from it.

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

A call that succeeds answers its response with the status of the last column: 200, or the
status the method's `http_success_status` option names. A submit answers 201, also when it is
a repeat that the service answers from the job already made, with the same body, so a client
needs no branch for a first and a repeated submit.

A call that changes state is a POST and a read is a GET. Cancel is
`POST /v1/jobs/{job_id}:cancel`, not `DELETE /v1/jobs/{job_id}`: a cancel keeps the job's
record and its result, where a `DELETE` would read as removal. It is idempotent on the job, so
a repeat answers 200 with the same final state. `GetLogs` answers the tail held at the time of
the call; there is no offset, limit or stream.

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
| an argument the service refuses | 400 | `invalid-argument`, with `field` when the service names one |
| a method the service does not implement | 501 | `unimplemented` |
| a store write that failed | 500 | `state-write-failed` |
| any other failure, a response that cannot be written | 500 | `internal` |
| unauthenticated (the mounting server's bearer layer) | 401 | empty body |

How the service's answer selects a row: a gRPC status that carries a `Refusal` in its details
is answered by the refusal's code. Any other status is answered by its gRPC code and its
message, which the service sets to the code of the table: `NOT_FOUND` with `unknown-job` (404)
or `expired` (410), `FAILED_PRECONDITION` with `not-finished` (409), `INTERNAL` with
`state-write-failed` (500 `state-write-failed`), `UNIMPLEMENTED` (501), and `INVALID_ARGUMENT`
(400, where a message `field: <name>` gives the `field`). Every other status, and a refusal
code this version does not know, is a 500 `internal`. The crate's goldens
(`crates/js-api-gateway/tests/golden/`) hold the whole mapping.

## The lease server's job service

The lease server holds the job service in `crates/js-lease-server/src/job_api/`: one service
object that both transports call, gRPC (the generated `JobService` server) and this gateway,
with one bearer layer in front of both. A request that does not present
`Authorization: Bearer <token>` with the server's token reaches no call: over gRPC it is
answered `UNAUTHENTICATED`, over the gateway 401 with an empty body. The token is the one the
lease routes take, compared by the same code.

Nothing mounts the two transports yet, so no port serves them. Every call answers
`UNIMPLEMENTED` (over the gateway, 501 `unimplemented`) until the file of its group in that
directory serves it (the fleet calls, until the server gives their group its handles); the
read calls are served (below):

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
and start failure, its host once placed, whether a cancel was asked for, and its times
(queued, placed, started, ended). The host silence is not set yet.

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

**`ListJobs`** keeps the jobs in any of the `states` (a state this server does not know
matches none), carrying every one of the `labels` with the same value, and queued in the
half-open window from `queuedAfterMs` (in) to `queuedBeforeMs` (out). Jobs come in the order
of their ids. A page holds 100 jobs when the request asks for 0, and at most 1000: a larger
`pageSize` is cut to 1000, not refused. A call reads the table 1000 rows at a time and stops
when its page is full, at the end of the table, or after 10,000 rows; so a page is short only
at the end of the table, where it carries no `nextPageToken`, or at that bound, where it
carries one even if it holds few jobs or none. The page token resumes after the last job or
row the page read; a job submitted between pages appears on a later page if its id sorts
after the token, and no job is listed twice.

### The fleet calls

The service's calls that are not about one job are an operator's view of the fleet. They
take the same bearer token as the job calls, and no other gate.

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
  status (the next launch's plan, the alerts, the orphans) is carried. A server with no elastic
  pool answers empty lists.
- `GetUsage` reads the ended bookings of the usage store (see `usage-store.md`), oldest end
  first, from `from_ms` included to `to_ms` excluded, of one host or one job when asked. A page
  holds `page_size` records (100 when 0, at most 1000); the filters apply before the page is
  cut. `next_page_token` is the key of the page's last record: the next page starts right after
  it, so no record is read twice or skipped while records are written; it is empty on the last
  page. A token no answer gave is `INVALID_ARGUMENT` `invalid-page-token`. A server started
  with `--usage-off` answers an empty page.

None of the reads takes a write lock, writes a row or counts anything. The hand-written
`GET /v1/hosts` and `GET /v1/pools` routes stay beside these calls with their own bodies; the
removal of the lease routes does not remove them.
