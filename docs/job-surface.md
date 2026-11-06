# The generated job commands and client

The job service is one protobuf `service`, `jobscheduler.v1.JobService`, in
`crates/js-proto/proto/jobscheduler/v1/` (design section 3.6). Two pieces of client code are
generated from it, so a call added to the definition appears in both with no hand-written
code:

- `crates/js-client/src/jobs/generated.rs`: one method of `js_client::jobs::JobClient` per
  call, taking and returning the messages of `js-proto`.
- `crates/js-cli/src/jobs/generated.rs`: `SURFACE`, the table of the `js` job commands (the
  words of each command, one flag per request field, the call), and `dispatch`, which sends a
  request through the client.

Both are rendered by `cargo xtask gen-job-surface` (`crates/xtask/src/job_surface`) from the
descriptor set js-proto builds, and both are checked in: a build needs no protobuf compiler,
and the generated code is reviewed with the change that makes it. The hand-written parts are
the connection and the classification of failures in js-client (`jobs/mod.rs`, `jobs/wire.rs`,
`jobs/error.rs`) and the interpreter in js-cli (`jobs/mod.rs`, `jobs/text.rs`), which builds
the commands from the table and names no call.

Three commands are written by hand because each is a loop over several calls, not one call:
`js job wait` (reads the job until it has ended and exits with a status that says how),
`js job follow` (writes a stream of the job's output to its end) and `js job run` (submits,
follows both streams, waits, and cancels on a signal). Their loops are js-client's
(`jobs/loops.rs`: `wait_for_terminal`, `follow_logs`, `submit_retrying`, over the `JobCalls`
trait and an injected `Waiter`); js-cli's `jobs/wait.rs`, `jobs/follow.rs` and `jobs/run.rs`
read their flags, print, and map the job's end to the exit status. `job run` takes the
flags of `job submit` from the table, so a field added to `SubmitJobRequest` is a flag of
both. Their help is held to goldens beside the generated commands'.

## Adding a call

1. Add the `rpc` and its messages to the `.proto` files, under the evolution rule in the
   header of `service.proto`, with a comment on the call and on every field: the comments
   become the client's documentation and the commands' help.
2. Render the files again from the repository root:

   ```text
   cargo xtask gen-job-surface --out .
   ```

3. Move the goldens the change moves (the help texts under
   `crates/js-cli/tests/cli/goldens/job_help/` and `job_surface.json`), and add the call's
   server side. When the server starts serving a call, add it to
   `js_lease_server::SERVED_CALLS` (`crates/js-lease-server/src/job_api/wiring.rs`, whose test
   fails when the list and what the server serves differ) and render the files again.

The same verb writes two blocks of `docs/http-gateway.md`, each between its marker lines
(`<!-- gen-job-surface routes begin -->` and `... end -->`, and the same for `served`): the
route table, from `js_api_gateway::ROUTES` (which the gateway's build reads from the
`google.api.http` options), and the paragraph that says which calls the server serves, from
`SERVED_CALLS`. The rest of the page is hand-written.

`cargo test -p xtask --test job_surface` runs `cargo xtask gen-job-surface --check --out .`,
which fails naming each checked-in file that is not what the definition renders (the page
too, when a block differs or its markers are missing), so a definition changed without
step 2 is red in CI.

`cargo test -p xtask --test doc_refs` checks the other direction for the operator docs: every
`js` command, with its flags, in `docs/runbooks/*.md`, and every `/v1/` route there and in
`docs/http-gateway.md`, must exist in the command tree of `js` (`js_cli::command()`) or among
the gateway's and the lease server's routes; it fails naming the file, line and word.

## The rules

How a call becomes a command and a field a flag is in the crate docs of js-cli ("Job
commands") and of `xtask::job_surface::cli`. In short: a call is a verb and a noun
(`ListHosts` is `js host list`); a request's one singular scalar field, when it has exactly
one, is its subject and its flag is required (`GetJob` is `js job get --job-id ID`); every leaf
field is a flag named by its path (`--spec.needs.cpu-millis`); a field the flags cannot spell is
read from a file of proto3 JSON (`--spec.ports-file`).

The subject rule has one limit: a request whose one singular scalar field is really optional
gets a required flag. The fix is to mark the field `optional` in the definition, which is
additive.

One entry is held by the generator and not read from the definition:
`xtask::job_surface::surface::RETRY_KEYS` says that `SubmitJob` is idempotent on
`spec.job_id` (design section 5.3 says so in words; the definition has no option for it). Its
flag is `--id`, and `js` draws a random id when it is not given, so that a submit whose answer
was lost can be run again as it was. Reading the job service fails when the call or the field
is gone, so the entry cannot fall out of date unnoticed; an additive option in the definition
would replace it.
