# The watcher and the abort (`js-watch`)

The lease server tells an operator about leaks: the scaler's alerts are on `/v1/pools` and
`/status`, and (with `server.alert_webhook_url`) are POSTed to a webhook. A server that is down or
wedged tells nobody, which is when machines leak. `js-watch` is a separate program that does not run
inside the server; run it on a different host.

It reads the same Lambda API key file as the server, and that key can launch and terminate. The
watcher is read-only by type, not by credential: it holds only a handle that can list. A separate
read-only key, if the cloud offers one, would make it true of the credential too.

## What each part does

| part | does | holds |
|---|---|---|
| `server.alert_webhook_url` | every alert key of the scaler is POSTed `{source, key, text}` when it begins to hold, again every `server.alert_repeat_after` (default 1 h) while it holds, and again if it clears and returns. Delivery is on a thread of its own with a bounded queue (64; the oldest is dropped and counted), so a receiver that refuses or hangs never delays a scaler tick. A refused POST is retried after 5 s, doubling to 10 min, and stays pending until delivered once even if the alert clears meanwhile | a URL, and (optional) a bearer token read from an owner-only file |
| `js-watch run` | each tick: compares the cloud's list of the scheduler's instances with the machines on the server's `GET /v1/pools`; checks that `last_tick_ms` still moves; every `sweep_every_secs` lists leftovers; alerts through the same webhook | read-only handles only (no launch, no terminate) |
| `js-watch abort` | stops every instance that carries one scheduler id; dry run unless `--apply` | the cloud client that can terminate, built only for this command |

No mail sink exists yet: the workspace holds no SMTP client. A mail sink is one more implementation
of `js_alert::AlertSink`.

## Alerts the watcher raises (keys)

- `heartbeat:<pool>`: that pool's `last_tick_ms` has not changed for `stale_after_secs`. It is per
  pool, so one wedged pool alarms whatever the others do. `heartbeat`: the status page could not be
  read that long. The comparison is on the watcher's own clock, so a skew between the two hosts does
  not matter. A pool that has a scaler (`scaler: true` on `/v1/pools`), or that has ever shown a
  tick time to this watcher, must tick: a null that lasts `stale_after_secs` alarms too (a scaler
  wedged on its first tick after a restart shows nothing else). A pool with no scaler has a null
  `last_tick_ms` for good and never alarms. The watcher is for servers that launch machines. A tick that
  cannot write the scaler's records (disk full) does nothing else and does not finish, so
  `last_tick_ms` stops moving then and the alarm is meant.
- `unexplained:<cloud>:<id>`: the cloud lists an instance of the scheduler that the server's machines
  do not show, for `confirm_ticks` ticks in a row. A machine is matched by its provider id when its
  record has one; a machine with no id yet is matched by name, and a name explains ONE instance, so a
  second instance with the same name (a launch answer lost and the launch repeated) is reported. An
  orphan the server itself knows as an orphan is reported here too: orphans are counted and reported,
  never released.
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
repeat_after_secs = 3600    # optional: a holding alert is sent again this often

[lambda]
api_key_file = "/run/credentials/js-watch/lambda-key"

[alerts]
webhook_url = "https://hooks.example.org/..."
webhook_token_file = "/run/credentials/js-watch/webhook-token"   # optional
```

`js-watch run --config FILE` runs until killed; `--ticks 1` makes one pass (for a timer).
The dedupe is per process: under a timer with `--ticks 1` every run starts with no memory, so every
holding alert is sent again at every run and `repeat_after_secs` does not apply there. Run it as one
long process if that is not wanted.

A holding alert is sent again after `repeat_after`, plus up to one tick, plus the sink's latency (the
repeat is decided at a tick, and its delivery completes after the receiver answers).

A webhook configured on a server with no elastic pool starts, with a warning: nothing raises an alert there.

## Abort

Stop or disable the lease server (or its elastic pool) first. While the scaler runs it can launch
again what the abort stopped.

```sh
js-watch abort --config FILE --cloud lambda --scheduler-id <id>            # prints, stops nothing
js-watch abort --config FILE --cloud lambda --scheduler-id <id> --apply    # stops them
```

It prints each instance it would stop with its type, region and hourly cost, the total, and the
instances with partial marks (listed, never touched). `--apply` stops each through the provider's own
release, which terminates only an instance whose tags and name are both the scheduler's and reads it
back; a stop that is not confirmed is printed as FAILED and the exit status is 1. There is no force.
Run it again to see them gone.
