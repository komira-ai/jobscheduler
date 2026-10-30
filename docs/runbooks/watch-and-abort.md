# The watcher and the abort (`js-watch`)

The lease server tells an operator about leaks: the scaler's alerts are on `/v1/pools` and
`/status`, and (with `server.alert_webhook_url`) are POSTed to a webhook. A server that is down or
wedged tells nobody, which is when machines leak. `js-watch` is a separate program for a different
host (or at least a different process and credential) that does not run inside the server.

## What each part does

| part | does | holds |
|---|---|---|
| `server.alert_webhook_url` | every alert key of the scaler is POSTed `{source, key, text}` once while it holds, and again if it clears and returns; a refused POST is sent again at the next tick | a URL, and (optional) a bearer token read from an owner-only file |
| `js-watch run` | each tick: compares the cloud's list of the scheduler's instances with the machines on the server's `GET /v1/pools`; checks that `last_tick_ms` still moves; every `sweep_every_secs` lists leftovers; alerts through the same webhook | read-only handles only (no launch, no terminate) |
| `js-watch abort` | stops every instance that carries one scheduler id; dry run unless `--apply` | the cloud client that can terminate, built only for this command |

No mail sink exists yet: the workspace holds no SMTP client. A mail sink is one more implementation
of `js_alert::AlertSink`.

## Alerts the watcher raises (keys)

- `heartbeat`: the server's `last_tick_ms` has not changed for `stale_after_secs` (or the status page
  could not be read that long). The comparison is on the watcher's own clock, so a skew between the
  two hosts does not matter.
- `unexplained:<cloud>:<id>`: the cloud lists an instance of the scheduler that the server's machines
  (by provider id or host name) do not show, for `confirm_ticks` ticks in a row. An orphan the server
  itself knows as an orphan is reported here too: orphans are counted and reported, never released.
- `list:<cloud>`, `sweep:<cloud>`: the cloud could not be listed or swept. A failed list is never an
  empty list.
- `stray:<kind>:<region>:<id>`: a leftover that carries the scheduler's marks and is not a running
  instance. Lambda: instances with some of the marks, not all (they bill). Not yet modelled for Lambda:
  persistent file systems and `/audit-events`. The kinds a later aws or gcp provider fills are volumes,
  network interfaces, addresses, disks and queued resources.

## Configuration

One TOML file; a secret is the path of an owner-only file.

```toml
scheduler_id = "js"
cloud = "lambda"
status_url = "http://10.0.0.5:7300/v1/pools"   # served to private peers only
interval_secs = 60          # optional
stale_after_secs = 300      # optional
confirm_ticks = 2           # optional
sweep_every_secs = 1800     # optional

[lambda]
api_key_file = "/run/credentials/js-watch/lambda-key"

[alerts]
webhook_url = "https://hooks.example.org/..."
webhook_token_file = "/run/credentials/js-watch/webhook-token"   # optional
```

`js-watch run --config FILE` runs until killed; `--ticks 1` makes one pass (for a timer).

## Abort

```sh
js-watch abort --config FILE --cloud lambda --scheduler-id <id>            # prints, stops nothing
js-watch abort --config FILE --cloud lambda --scheduler-id <id> --apply    # stops them
```

It prints each instance it would stop with its type, region and hourly cost, the total, and the
instances with partial marks (listed, never touched). `--apply` stops each through the provider's own
release, which terminates only an instance whose tags and name are both the scheduler's and reads it
back; a stop that is not confirmed is printed as FAILED and the exit status is 1. There is no force.
Run it again to see them gone.
