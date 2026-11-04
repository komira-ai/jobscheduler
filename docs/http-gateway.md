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

- A path variable (`{job_id}`, `{host_id}`) sets the request field it names. An empty path
  segment is an empty value; path text is not percent-decoded.
- A POST takes the request message as its JSON body (an empty body reads as `{}`) and no query
  string. A body field the path sets (`{"jobId": ...}` on a cancel) is refused.
- A GET takes no body. Its query string sets the other fields: `name=value` for a singular
  field (once), the same name repeated for a list, and `name[key]=value` for each entry of a
  map, percent-encoded, with `+` read as a space. For example
  `GET /v1/jobs?states=JOB_STATE_QUEUED&states=JOB_STATE_RUNNING&labels[team]=ml&pageSize=50`.
- A field may be named by its JSON name (`pageSize`) or its proto name (`page_size`).
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
- a field at its default value (0, `false`, empty) left out of a response.

A request with a field its message does not have is refused with a 400 that names it, so an
older server tells a newer client that it does not know the field instead of dropping it.

## Limits

A request body over 64 KiB (65536 bytes, the lease server's body limit) is a 413
`request-too-large` before anything is decoded. The gateway adds no buffer of its own for the
request head; the listener in front of it bounds the head, query string included.

## Errors

An answer that is not the call's response is `{"error": "<code>"}`, plus `"field"` (the first
offending field, by its JSON name: `spec.jobId` inside the submitted job) and `"reason"` (a
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
| any other failure | 500 | `internal` |
| unauthenticated (the mounting server's bearer layer) | 401 | empty body |

How the service's answer selects a row: a gRPC status that carries a `Refusal` in its details
is answered by the refusal's code. Any other status is answered by its gRPC code and its
message, which the service sets to the code of the table: `NOT_FOUND` with `unknown-job` (404)
or `expired` (410), `FAILED_PRECONDITION` with `not-finished` (409), `INTERNAL` with
`state-write-failed` (500 `state-write-failed`), `UNIMPLEMENTED` (501), and `INVALID_ARGUMENT`
(400, where a message `field: <name>` gives the `field`). Every other status, and a refusal
code this version does not know, is a 500 `internal`. The crate's goldens
(`crates/js-api-gateway/tests/golden/`) hold the whole mapping.
