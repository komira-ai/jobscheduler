# First live launch on Lambda Cloud

This is the checklist for the first time a scheduler spends money on a Lambda Cloud GPU
instance. A lead and the owner of the account run it together. The design is
`docs/design/pools-providers-network.md`; sections are cited as `D§n` and open questions as `Qn`.

Each item is marked:

- **CI** a test in this repository proves it against fakes. It says nothing about Lambda,
  Tailscale or the stock image.
- **LIVE** only a person looking at the real systems can prove it. Nothing in CI covers it.

It is written against `main` after the scaler's shell (#287), the Tailscale DNS and socket wait (#309),
digest-pinned images (#311), the pool's provider checks (#312), the one config file (#302), the status
pages (#286), the Lambda launch inputs (#267), the host credential (#266, #274) and revoke on terminate
(#196). Key names below are the ones the config reader accepts today. The reader refuses a key it does not
know, so a mistyped key stops the start rather than being ignored. The design's `memory_schedulable_percent`
is not a key on `main`; nothing here sets it.

The service is started with `--config <file>` and nothing else: a flag beside `--config` is refused (#302).

Stop at the first failed pass criterion. Do not go on to a later step to see what happens.
Section 7 says how to stop spend from any step.

Write down each step's output (with every secret masked) in the launch record: the date, the
scheduler version, the head of this repository, and who ran it.

## Must land before the first live launch

Each item is marked **blocking** (do not set `launch = "live"` until it has merged), **accepted risk**
(the run goes ahead with the figure given) or **lead decides** (not decided; the recommendation is stated).
Re-read the state of each issue and pull request on the day; this list was written from the repository
as it stood then.

The live launch itself needs the CEO's explicit go. It spends money on a real account, so no one
sets `launch = "live"` on their own judgment, whatever the state of this list.

The lead has decided every item below. The marks are the decisions.

| item | what is missing, and what it costs | mark |
|---|---|---|
| #329 (open pull request): price-0 cap bypass | The step accepts any offer that has a price, and a price of 0 counts as one (`price_cents_per_hour.is_some()`). A zero-priced offer is launched, stored at 0, and adds 0 to the hour and day sums, so `max_spend_per_hour` and `max_spend_per_day` do not bind on it; an orphan whose offer the catalog prices at 0 is counted at 0 too. The pool check only names an offer with no price at all (`catalog: <offer> has no price`). #329 refuses a zero price and counts such an orphan at the whole hourly cap. This is money. | **blocking** (unchanged) |
| #329, the same change: persisted counters | The times a host was first seen silent and a machine first seen missing are held in memory. Each restart starts the count again, so a scaler restarted more often than `unknown_hold` (30 min) never reaches the hold and a silent machine is not released. #329 writes them to the records file (version 2) and does not count the scaler's downtime. Until it lands, the run does not restart the service while an instance is up, except to abort (section 7). After it lands, a build without it cannot read the version 2 records file, so a rollback keeps the file's build in mind. | **blocking** (it ships with the price fix) |
| #329, the same change: boot hold | A booting machine past `boot_timeout` is released at the first complete registry view, with no hold, so a restart can release a machine whose agent is about to say hello. #329 holds for a third of `boot_timeout` (5 min by default) first. This costs a launch, not an overspend. | **blocking** (it ships with the price fix) |
| #387: `heard_since_hello` | The shell passes `true` for every host, which turns off the lost-Welcome rule. A host whose hello was admitted but whose Welcome was lost never heartbeats and cannot register again (the token is retired); it is released only after `unknown_hold` of silence, not at `boot_timeout`. The bound for one such machine is the time to its hello, plus the 15 s heartbeat grace, plus 30 min (`unknown_hold`, not a config key), at the offer's hourly price: at `max_instances = 1` that is about half an hour of one instance, and the pool launches nothing else meanwhile. This path starts no boot-failure cooldown as far as the design describes it, so a repeat is bounded by `max_spend_per_day`, not by time. | **accepted for the first run** (the first live run only), with `max_instances = 1`, a small day cap (a few hours of one instance), and the watch rule of step 4 (terminate by hand a registered host that has not heartbeated within `boot_timeout`). It stays a named follow-up. |
| #388: persist the unremoved hosts | A host whose registry removal was refused (a lease booked on it) is retried each tick, but the list is in memory. After a restart it leaves the registry only by `forget_lost`, up to 24 h later, and keeps its pool slot. No machine is running and nothing is billed; with `max_instances = 1` the pool cannot launch again until then. | **accepted for the first run** (no spend). It stays a named follow-up. |
| Metadata-address block (design 7.3 step 8, F12b), branch `bootstrap-metadata-block` | The design says the bootstrap blocks containers from the provider's metadata address. The rendered bootstrap on `main` has no such step: it prepares the image, writes the token and instance line, runs the network steps, fetches the agent, installs the GPU hook and the unit. No open issue names it; the branch above is the work. A job on the host can read the instance's user data (the agent URL, and the Tailscale key, which is single-use and 10 min old at most; the join token is retired at the first hello). | **blocking.** It is being built on the branch `bootstrap-metadata-block`; do not set `launch = "live"` until it has merged and the deployed build contains it. Step 5.1 passes only once it has landed. |
| #345: the bootstrap's agent unit | It lacks the engine ordering, stop timeout, 30 s restart pace and mount condition of the packaged unit (`RestartSec=5` here). A misconfigured agent says hello every 5 s (the hello limiter applies). No spend effect on a one-instance run. | **accepted for the first run**. It stays a named follow-up. |
| Human checks the design puts before the go | Q10 (no inbound under Lambda's account-wide rules, 3.1), Q2 (launch field names and the `user_data` limit, 3.2), Q5 (the stock image: driver, runtime, toolkit, scratch device, cloud-init file list, 5.2), and the live checklist of design 13.4 (sections 3 to 6). They are steps of this runbook, not code. | **blocking**, each as its step says. |
| Host credential (design 7.2, PR 7b) | Merged (#266, #274). The switch `require_host_credential` is off by default; with it off, a hello that does not ask for a credential keeps the join token as the host's credential for up to 24 h. Step 1.11 turns it on. | merged; the setting is decided: **on** for launched pools (`server.require_host_credential = true`, in the example and step 1.11). |

## 1. Prerequisites

The config is one file. This is the shape the reader accepts for a Lambda pool (placeholders in angle
brackets; every path is a file owned by the service user, mode 0600 or 0400, except the TLS certificate):

```toml
[server]
listen            = "<tailnet address>:7300"
agent_listen      = "<tailnet address>:7301"    # required with an elastic pool; differs from listen
advertise_url     = "https://js.example.ts.net:7301"   # required when a pool is live; https off the loopback
scheduler_id      = "js"                        # default "js"
state_file        = "/var/lib/jobscheduler/state"
boot              = "resume"                    # "initialise" for the first live start only (step 4)
client_token_file = "<path>"
tls_cert          = "<path>"
tls_key           = "<path>"
require_host_credential = true
max_lease_wall    = "1h"                        # required with a pool with GPUs: no lease may ask for more

[[pool]]
name               = "gpu"
class              = "whole-node"
launch             = "plan"                     # the one line the go changes
max_instances      = 1
max_spend_per_hour = "<dollars, e.g. 2.00>"     # a string: a decimal with at most two places
max_spend_per_day  = "<dollars>"                # not below the hour cap
idle               = "2m"                       # short for this run; required of a pool with GPUs
max_age            = "3h"                       # required of a pool with GPUs: the host is retired at this age, busy or not
min_host_lifetime  = "15m"                      # required of a pool with GPUs: idle never releases a younger host
boot_timeout       = "15m"                      # the default
agent   = { x86_64 = { url = "https://<host>/js-agent", sha256 = "<64 lower-case hex>" } }   # one entry per arch
network = { kind = "tailscale", oauth_client_file = "<path>", tag = "tag:js-agent",
            binary = { url = "https://<host>/tailscale.tgz", sha256 = "<64 lower-case hex>" } }
provider = { kind = "lambdalabs", api_key_file = "<path>",
             types = [{ name = "<instance type>", gpu = "<gpu class>", gpus = 1 }] }
             # optional in provider: regions, ssh_key, api_url
```

The reader refuses an unknown key, a `launch` other than `plan` or `live`, a day cap below the hour cap,
`max_instances` outside 1 to 255, and an `agent` or `network.binary` pin that is not an `https://` url with a
64-digit lower-case sha256. With `launch = "live"` it also requires `agent` (at least one architecture),
`network.binary` for a Tailscale network, and `server.advertise_url`. **CI** (`config_launch.rs`,
`config_pool_refusals.rs`).

| # | item | pass criterion | mark |
|---|---|---|---|
| 1.0 | Every **blocking** item of "Must land" has merged, and the build you deploy contains it. | `git log` of the deployed build names each pull request. | LIVE (a person reads it) |
| 1.1 | A Lambda Cloud account used only for this scheduler, or one where every instance is ours to touch. `server.scheduler_id` (default `js`) is unique on the account (D§5.2, F16). | The Instances page lists nothing that is not yours. | LIVE |
| 1.2 | An API key file (`provider.api_key_file`), mode 0600 or 0400, owned by the service user. | `stat -c '%a %U' <file>` prints `600 <service user>`. A file others can read stops the start. | CI (the reader refuses), LIVE (the key is valid) |
| 1.3 | The ssh key rule. The key is never used to log in; Lambda needs a name on the launch. With `provider.ssh_key` set, that key must be on the account. Otherwise the launch holds the key named `js-<scheduler id>`, else the account's only key; on an account with no key the launch creates `js-<scheduler id>`; with two or more keys and none named the pool is unavailable. | Either name one in `provider.ssh_key`, or have exactly one key (or none). `provider_checks` shows `ssh_key` ok. With no key it carries the note `account has 0 keys; a launch will create js-<id>`; with several, the reason is `provider.ssh_key: account has N keys; name one`; with a name that is not on the account, `provider.ssh_key: key <name> is not on the account`. | CI (`pools_provider_checks.rs`), LIVE (Lambda accepts the name) |
| 1.4 | A Tailscale OAuth client with only the scopes `auth_keys` (write) and `devices:core` (write), limited to the one tag, and a tailnet policy that lets that tag reach the scheduler's `agent_listen` port and nothing else (D§8.3). The client file (`network.oauth_client_file`) is TOML with `client_id` and `client_secret`, mode 0600. | The admin console shows the client with those two scopes and one tag. The policy has one rule for the tag, to the agent port only. | LIVE. The service cannot see the client's scopes or the policy (Q22: the policy warning is not built). |
| 1.5 | The scheduler joined to the tailnet (its own `tailscaled`), a TLS certificate for its tailnet name, `advertise_url = "https://js.example.ts.net:<agent port>"`, `agent_listen` the tailnet address. Start-up refuses a non-loopback `http://` `advertise_url` (D§9, Q13) and a non-loopback listener without `tls_cert` and `tls_key`. | `tailscale status` on the scheduler host shows the node. `agent_listen` is the tailnet address, not a wildcard (a wildcard bind shows as `server: warning: binds all interfaces`). | CI (the refusals), LIVE (the certificate and route) |
| 1.6 | One agent pin per architecture the pool's offers have: `agent.<arch>.url` and `.sha256`. An offer whose architecture has no pin is never launched, and the pool is unavailable with `catalog: <offer> is for the architecture <arch>, which has no agent pin (agent.<arch>)`. | For each arch: download the url from a clean machine, `sha256sum` it, compare with the config. The file is the release you mean to run: `js-agent --version`. | CI (the shape, the requirement with `live`, the catalog note), LIVE (the digest is of the right file) |
| 1.7 | The pinned Tailscale static build (`network.binary`): url and sha256. | Same check as 1.6 against the url in the config. | CI (shape, required with `live`), LIVE (the digest matches the file) |
| 1.8 | Caps. `max_instances = 1`; `max_spend_per_hour` set to the smallest GPU type's price for one instance for one hour, and `max_spend_per_day` to about twice that. | The config has all three. `/v1/pools` shows `instances.max` 1 and `instances.max_spend_cents_per_hour` equal to your figure in cents. Read the type's price on Lambda's page and confirm it is above zero (#329: the cap does not bind on a zero price until it lands). | CI (they are required and the step enforces them), LIVE (the prices the catalog returns are the ones you budgeted) |
| 1.9 | `launch = "plan"` in the pool. A second lock: the provider is built disabled and in dry run unless `launch = "live"`. | `grep launch <config>` prints `plan`; `/v1/pools` shows `launch: plan`. | CI (`a_plan_pool_launches_nothing`) |
| 1.10 | A client token file for the lease routes, and the `js` CLI on a machine that can reach `listen`. | `js --server https://js.example.ts.net:<port> --token-file <file> --ca-cert <pem> lease list` prints an empty list. | CI (CLI), LIVE (the route) |
| 1.11 | `server.require_host_credential = true`: the first hello of a one-time join token must ask for a host credential, or it is refused (`join-token-credential-required`, 403) and the token stays unused. The pinned agent asks. | The line is in the config. A launched host's token is dead after its first hello (5.3). | CI (`the_switch_refuses_an_unasked_first_hello_of_a_one_time_token`), LIVE (the pinned agent asks) |
| 1.12 | Where the records file will be: `<state_file>.scaler`, beside the lease file, in a directory only the service user can write. Back it up with the lease file. | `ls -l` of the directory. Nothing in the directory is deleted by hand during the run. | LIVE |

On fail: fix the item. Nothing has been spent.

## 2. Plan mode

Start the service with the config from step 1 and `launch = "plan"`. A plan pool does not need the
records file. From a peer on the private or tailnet network (not from a launched host, and not from the
open internet):

```sh
curl -sS https://js.example.ts.net:<port>/v1/pools
curl -sS https://js.example.ts.net:<port>/status      # the same data as a page
curl -sS https://js.example.ts.net:<port>/healthz
```

`/healthz` answers 200 `ok` while the tick loop runs, and 503 once the table's tick loop or the
scaler's task has stopped (a panic ends it). **CI** (`a_panic_in_the_scaler_ends_its_ticks_and_health_says_so`).
The status routes need no token. They answer 404 on a `listen` address that is not private and to a peer
that is not private (**CI**: `status_gate_tests`).

For the pool in `/v1/pools` (`pools[]`), look at:

- `launch` is `plan`. `state` is `available` (it begins `checking`; wait one pass: 60 s at most, 10 s then
  doubling to 5 min while it fails).
- `credentials.ok` and `network_check.ok` are true, `reasons` is empty.
- `provider_checks` has three entries, `types`, `ssh_key` and `firewall`, each `ok: true`. A `note` is
  not a failure: `ssh_key` may say a launch will create a key, `firewall` may say
  `js-<scheduler id>-noinbound will be created in <regions>`. The checks only list; they create nothing.
  A `types` row that matches no catalog entry, or whose GPU count differs, fails with
  `catalog: ...`; a ruleset of our name that holds rules fails with
  `firewall: the ruleset <name> holds <n> rules; it must hold none`.
- The top-level `warnings` is empty (no `server: warning: binds all interfaces`).
- `instances.n` is 0, `instances.max` is 1; `blocked` is null (it is `unavailable` whenever `state` is not
  `available`; it carries no other value); `no_offer` is null; `machines`, `orphans` and `alerts` are
  empty arrays.
- `next_launch` holds the offer, the zone, a name, and a `plan` in which `join_token` and
  `network.tailscale.authkey` print as `***`, with the agent and Tailscale urls and digests you pinned,
  `--host-id`, `--container-pull missing` and the GPU toolkit hook. `plan_error` is null.

Pass: every check green, `next_launch` as above, no secret in either response
(`curl ... | grep -c` for the first characters of the join-token file and of the OAuth secret
prints 0).

| claim | mark |
|---|---|
| The pool becomes `available` only after one full pass; a plan pool launches nothing | CI |
| The page cannot show a plan the launch would not follow; the secret slots print `***` and a test greps the serialised status for known secrets | CI |
| The provider checks (ssh key, firewall, `types` rows) make the pool unavailable before any acquire and create nothing | CI (`pools_provider_checks.rs`) |
| The credentials and the OAuth client are accepted by the real APIs; the catalog has the GPU type and its price; Lambda's list calls answer as the fake does | LIVE |
| No process on the scheduler host or in its logs holds a secret in clear | LIVE (read the log once) |

On fail: read `reasons` and the log, fix the config, restart. A refused key is
`credentials: failed (<code>)`; a refused OAuth client is `network: credentials failed`. Do not go on.
`check()` does not mint a Tailscale key, so a green network check does not prove the mint works
(D§8.5); step 4 does.

## 3. Pre-spend human checks

These are things the code cannot prove. Do both before the flip. Record the answers; if either fails,
stop and do not set `live`.

### 3.1 No inbound, including port 22 (D§9, Q10)

Every launch attaches a firewall ruleset named `js-<scheduler id>-noinbound` that holds no rules. The
ruleset is looked up again on every launch, and made in the region if it is absent; one of that name that
holds rules refuses the launch (**CI**), and the pool check fails first (step 2). Lambda also has
account-wide rules, and they may still allow port 22.

1. In the Lambda console, open Firewall. Read the account-wide (global) rules. List each inbound rule and
   its port, in the launch record.
2. Confirm the ruleset `js-<scheduler id>-noinbound` of the region exists with zero rules, or does not
   exist yet (the first launch creates it; `provider_checks` shows `firewall` ok with the note that it
   will be created, or fails if the ruleset holds rules, **CI**).
3. If a global rule allows 22 or any other inbound port, remove it or restrict it, or stop. The attached
   ruleset does not override a global rule unless Lambda's documentation says so; read that page and
   record the answer.

Pass: you can state, from Lambda's own pages, that no inbound port is open to an instance launched
with the ruleset. **LIVE.** Step 5 tests the same thing from outside as far as a person can, but
that test cannot prove a negative; this reading is the check. Until then the docs do not say "no
inbound" (Q10).

### 3.2 The user_data field names and size limit (Q2)

The crate has fixtures for launch, instances and instance types only. The names of `user_data` and the
firewall ruleset fields come from the API description, and the limit is "1MB" with no unit
(`crates/js-provider-lambdalabs/src/provider.rs`, `PAYLOAD_LIMIT` = 10^6 bytes, the smaller reading).
The fields the launch sends are `ssh_key_names`, `user_data` and `firewall_rulesets`, beside the type,
region, name and tags (`crates/js-provider-lambdalabs/src/wire.rs`).

1. Open Lambda's current API reference. Confirm the launch call's field names: `user_data`,
   `ssh_key_names`, `firewall_rulesets`, `tags`, `name`, as sent in that file.
2. Read the stated limit and the unit. Record it.
3. Print the masked plan size: the rendered bootstrap for the pool is in `next_launch.plan`; a plan
   over the limit is a `Shape` refusal before any call (**CI**).

Pass: the names match and the limit is at least 10^6 bytes, with the rendered plan well under it.
**LIVE** (documents). A launch rejected for a field name or size shows as a refusal in `last_launch`
and is not a launch; it costs nothing.

### 3.3 Optional: the provider alone

`crates/js-provider-lambdalabs/examples/first_launch.rs` launches one instance through the provider with
hard boot, wall-clock and spend limits and no scaler or bootstrap. It runs under its own scheduler id, `first-launch`, so what it launches carries that id and not the pool's.
It takes no ssh flag and checks nothing on the instance: nothing can connect, so it waits for the API to
report the instance active. Its dry run (the default) prints a plan with a
hash; `--go --plan-hash <hash>` launches only that plan; `--teardown-only <state file>` terminates what
it launched. Use it only if you want the field names and limit proven before involving the scheduler.
It spends money, so it needs the same go as step 4. **LIVE.**

## 4. The go

Both people agree, aloud, in the launch record: the caps, the abort steps of section 7, who watches
the Lambda console during the run, and the decisions of "Must land" that are not blocking, and that the CEO has given the go for this live launch.

1. Set `launch = "live"` in the pool. This is the one change in the pool. Because the pool is live, it does
   not start without its records file, and in plan mode none was written (the file is written when a
   step changes a record). Set `boot = "initialise"` for this start only: the service otherwise refuses
   with `<state file>.scaler is missing, and a pool with launch = "live" does not start without its scaler
   records ...`. **CI** (`a_live_pool_with_no_records_file_starts_only_at_a_first_start`). `initialise`
   creates the lease file if it is missing and takes an existing one as it is; do not use it later to
   get past a missing records file, which would forget the machines in flight and the day's spend.
   Check the diff: these two lines are the only changes. Restart the service.
2. Read `/v1/pools` again: `launch` is `live`, `state` is `available`. After a restart the scaler does not
   release a host or a machine as missing or silent until the registry view is complete: at least
   5 minutes after the listeners bind, and until every known host has been heard or 10 minutes have
   passed (**CI**, `scaler/complete.rs`). Launching needs the checks to pass.
3. Request one lease:

   ```sh
   js --server https://js.example.ts.net:<port> --token-file <client token> --ca-cert <pem> \
      lease acquire --class whole-node --gpus 1 --gpu-class <class from types> --max-wall 30m
   ```

   Use the class and `gpu` spelling from the pool's `types` row. The answer is 202 `pending`: the machine
   does not exist yet. (`acquire` waits `--wait-pending`, 60 s by default, for a start to settle; a boot
   takes longer, so poll `lease list`.)
4. Watch `/v1/pools` every 10 s. `machines[].state` goes `booting`, then `registered` (the record
   begins `planned`/`launching` for the length of a tick). Also watch the Lambda console list.
5. Leave `boot = "initialise"` until the run is over. When the machine is gone (section 6), check that
   `<state file>.scaler` exists, then set `boot = "resume"` and restart. With `resume` and no records
   file a live pool refuses to start, by design; the file appears with the first launch record.

| what you see | pass |
|---|---|
| `last_launch.outcome` is `accepted` (the provider gave an id; `pending` before), `host` set; the instance in Lambda's list carries our name and tags `jobscheduler:scheduler`, `jobscheduler:pool`, `jobscheduler:acquire`, and the firewall ruleset | the tags and name are as above (**LIVE**) |
| `booting` within one tick of the request | **CI** for the order (record written before the call, one launch per request: `a_waiting_request_is_launched_once_with_its_records_written_first`, `a_live_pool_launches_once_and_not_again_after_a_restart`) |
| A tailnet device named like the host appears, then `registered` once the host says hello, within `boot_timeout` (default 15 min). The unit starts the agent with `--host-id <name>`, which is how the scaler finds the host of the record. | **LIVE.** Time it: the Tailscale key lives 10 minutes (Q11); the join token lives `boot_timeout` plus 5 minutes. |
| `instances.n` is 1 and `spend_cents_per_hour` is the catalog price | **CI** (arithmetic), **LIVE** (price is Lambda's) |
| The lease moves from 202 `pending` to held on `js lease list` | **CI** (flow), **LIVE** (a real host) |

The watch rule: a machine `registered` for longer than `boot_timeout` whose host does not appear in
`/v1/hosts` as `state: live` with a recent heartbeat is the lost-Welcome case (#387). Terminate it by hand
(section 7); the scaler would hold it for `unknown_hold`.

On fail:

- `booting` longer than `boot_timeout` with no hello: the scaler releases the machine at the first
  complete registry view past `boot_timeout` and starts `boot_fail_cooldown` (10 min). Watch it do that;
  if the instance is still in Lambda's list 5 min after `boot_timeout`, use section 7. Reasons to read:
  the Lambda console's output for the instance, `tailscale status` for the device, the scheduler's log.
- No launch: `/v1/pools` carries `blocked` only as `unavailable` (a failing check, with `reasons`); it
  does not say that a cap held a launch back. Compare `instances.n` and `max`, and
  `spend_cents_per_hour` with `max_spend_cents_per_hour`, then read the log. A launch the provider
  refused leaves no record: its reason is in the log, and no instance exists.
- `network: key mint failed: <status>`: no launch was made. Fix the OAuth client's scopes or tag.
- A host refused `host-contract` (no container runtime or GPU toolkit): the stock image lacks something
  the host-prep steps do not install (Q5). Those steps check `nvidia-smi`, the container program, and make
  the scratch file system; they install nothing. Expect this on the first run. Record what was missing
  and tear down (section 6).
- `alerts` on `/v1/pools` or `/status` is not empty: read the text. The alerts are an orphan (a machine of
  ours that no record knows: `a machine of ours that no record knows: ...; release it at the provider`), a
  record of a pool the server does not have, a host `silent past twice unknown_hold, books nothing and is
  not marked releasing`, a join token `that cannot be revoked, so its machine is not terminated and keeps
  running`, a host `of a machine that is gone` that the registry will not remove (#388), and a machine
  `whose release the provider does not confirm`. Each alert means a machine may still be billing; go to
  section 7 for any of them. **CI** (`the_alerts_are_on_the_status`, `a_revoke_that_keeps_failing_is_said_once`).

## 5. On the live host, while it runs

You cannot ssh in: that is the point. Run each of these as a container job on the leased host (the
run routes, or `js lease run`), or from outside, as stated. Do them in order,
while the lease is held. Keep the `max_wall` short.

### 5.1 Containers cannot reach the metadata address (D§7.3 step 8, F12b)

Run, as a job in a container on the host, a request to the provider's metadata address
(the link-local address for the cloud; look it up in Lambda's documentation and record it) with a 5 s
timeout, for the instance's user data.

Pass: the request times out or is refused; no user data is returned. A job's container cannot reach
the metadata address. **LIVE.** This passes only once the block has landed (branch `bootstrap-metadata-block`;
see "Must land", where it is blocking). The bootstrap on `main` renders no such step, so a build
without the branch returns the user data here, and that is a failed pass, not an accepted one.
On fail: stop, tear down (section 6), and do not run another job on any launched host until
the block exists. The token is retired at the first hello, which limits what a reader of user data gets
(5.3), but the Tailscale key (single-use, 10 minutes) and the agent url are in user data.

### 5.2 Cloud-init copies of user data: fill the cleanup list (D§7.3 step 7, Q5)

On the host, through a job with the root filesystem visible (or the agent's host-visible mount), and
before the host is released:

```sh
find / -xdev -newer /etc/jobscheduler/instance -type f 2>/dev/null | head -200
grep -rIl 'tskey-' / --exclude-dir=proc --exclude-dir=sys 2>/dev/null
grep -rIl 'jobscheduler/token' / --exclude-dir=proc --exclude-dir=sys 2>/dev/null
```

The `tskey-` pattern finds a Tailscale auth key; the second finds the rendered script, which writes the token. Compare the list with `CLOUD_INIT_TRACES` in
`crates/js-lease-server/src/bootstrap/mod.rs`: `/var/lib/cloud/instance`, `/var/lib/cloud/instances/*`,
`/var/lib/cloud/data/*` and `/var/log/cloud-init*.log`, which the script's `EXIT` trap removes together with
the token file and the Tailscale key file.

Pass: every path that holds the user data, the token or the Tailscale key is in that list or was
removed by the trap, and neither `grep` finds a file outside `/proc` and `/sys`. Record any path that is not in
the list; it goes into the list in a follow-up change, and the stock image is re-checked whenever
it changes. **LIVE.** Removal with `rm` does not erase blocks and nothing here claims it does.
On fail (a path holds a secret and is not in the list): the token is dead after hello (5.3); the
Tailscale key is single-use and ten minutes old at most. Record it and add the path before a second launch.

### 5.3 The join token is retired after the first hello (D§7.2)

The scheduler keeps only a digest of the token, so an operator cannot replay it from the scheduler
host. The live replay is from the host itself, and only if the token file survived the trap:

```sh
test -f /etc/jobscheduler/token && curl -sS -o /dev/null -w '%{http_code}\n' -X POST \
  -H "Authorization: Bearer $(cat /etc/jobscheduler/token)" \
  https://js.example.ts.net:<agent port>/v1/agent/hello -d '{}'
```

Pass: the file is gone (the trap ran), or the answer is 401 with an empty body. **CI**: a retired
token is unknown and gets the same 401, and two concurrent first hellos make one credential
(`agent_api_race_tests`). **LIVE**: that the real agent sent the first hello and was given the
credential: `/v1/hosts` lists the host with `source: registered`, and the agent keeps heart-beating.

### 5.4 The tailnet and port separation (D§8.3, F21)

From the launched host (a job in a container that tries `curl`): 

- connect to another node of the tailnet: must fail;
- connect to the scheduler's `listen` port (`/v1/pools`, `/v1/leases`): must fail;
- the agent port (heartbeats, `/v1/agent/hello` with no token answering 401): must succeed.

Pass: only the agent port answers. **LIVE.** This is the only test that the operator's policy and the
route split work together (they are separate in the config; **CI** proves the routes are not served
on the wrong port).
On fail: stop and tear down. Fix the policy before another launch.

### 5.5 Inbound from outside (D§9)

From a machine outside the tailnet and from the scheduler host, try the instance's public address on
22 and one other port, 5 s timeout.

Pass: both time out or are refused. **LIVE.** This supports 3.1; it does not replace it.

### 5.6 The lease ran

Run a short job on the lease (`js lease run ... -- nvidia-smi` or a container job as the pool allows).

Pass: exit status 0 and the GPU listed; the host in `/v1/hosts` shows the lease and `state: live`.
**LIVE** (the GPU, the state root, the loop-mounted device the agent needs; Q5). **CI** covers the
lease flow against a fake agent.
On fail: read the agent unit's output through the job, or the console output in Lambda.

## 6. Teardown

1. Release the lease (`js lease release <id>`) or let `max_wall` end.
2. The scaler holds the host while it works and drains it once it has been idle for the pool's `idle` time
   (set it short for this run). `machines[].state` goes `registered`, then
   `draining`, then `releasing`. **CI**: a host is released only while it has no lease, run or stream
   (`a_busy_host_is_kept_and_a_drained_host_is_taken_back_for_a_request`,
   `a_machine_is_launched_registered_idle_drained_released_and_removed`).
3. Order, as the log and the status show it: the host's token or credential is revoked **before** the
   release is sent. If the revoke fails, the release waits and the revoke is retried each tick.
   **CI** (`a_revoke_that_fails_holds_the_release_back`,
   `a_restart_marks_what_was_being_released_before_anything_else`; the host credential ends with the
   revoke: `a_revoke_ends_the_credential`). A revoke that keeps failing raises the alert
   `the join token of host <name> cannot be revoked, so its machine is not terminated`: the machine
   is billing, so go to section 7.
4. The release is read back until the instance is gone. The status shows the machine
   (state `releasing`) until Lambda's list does not hold it; a release the provider does not confirm
   within `boot_timeout` plus 30 min raises an alert (`a machine whose release the provider does not confirm`). **LIVE** that Lambda's list agrees (open the console).
5. The tailnet device is deleted: `tailscale status` and the admin console no longer list the host name.
   A failed delete is logged as a warning and does not block the release; delete it by hand if so. **CI** (the
   delete, fake API), **LIVE** (the real one).
6. The join token and the host credential are revoked: the agent routes answer 401 to both. **CI**
   (revoke), **LIVE** (against the running service).
7. The Tailscale OAuth client and the Lambda API key remain; revoke or rotate them if the run showed any
   secret in a log or on a host path (5.2).
8. Return the pool to `launch = "plan"`, set `boot = "resume"` (check `<state file>.scaler` exists first),
   and restart, so the next spend is a new go. Keep the records file: a live pool does not start without
   it, and it holds the day's spend.

Pass for the run: from Lambda's list and status: our tags and name; a hello arrived; the lease ran; the
instance terminated and read back gone; the device gone from the tailnet; the token revoked;
`instances.n` is 0; no spend after the release. **LIVE.**
Open the Lambda billing page after an hour and compare the charge with `max_spend_per_hour`.

On fail: a machine not gone after the second tick, or any item above not true: section 7.

## 7. Abort and rollback

Stop spend at the first doubt. Do these in order; the first that works is enough, then verify with the
last.

| situation | action |
|---|---|
| Before the flip (steps 1 to 3) | Nothing was launched. Leave `launch = "plan"`. |
| Any time after the flip, before a launch | Set `launch = "plan"` in the config and restart. Nothing launches. Also release the lease request (`js lease release`). |
| An instance exists and the scheduler is healthy | Release the lease; let the teardown of section 6 run. Watch it. |
| A registered host that has never heartbeated past `boot_timeout` (a lost Welcome, #387), or an `alerts` entry about a machine | Terminate the instance in the Lambda console (or `first_launch --teardown-only` if it launched it). Instances are found by the tags `jobscheduler:scheduler=<id>`, `jobscheduler:pool=<pool>` and the name. Then delete the tailnet device in the admin console and restart the scheduler: it lists the account, finds no instance, and its record for the machine ends. Restart with `boot = "resume"` and the records file in place; never delete the records file or start with `initialise` to clear a record. |
| An instance exists and the scheduler is wedged, unreachable or restarted badly | The same action. `/healthz` answers 503 once the tick loop or the scaler task has stopped; the supervisor must restart the process, which reads its records back, and launches nothing twice (**CI**). While it is down nothing releases the instance; the lifetime the launch carries (`max_age` plus `max_lease_wall` plus the waits of a retire) is for a cloud that can enforce one, and Lambda cannot, so on Lambda only `max_age` with a live scheduler bounds the cost, and with the scheduler down the bound is the supervisor's restart and your watch. Outside the lifetime sum, to fit in its 10 minute margin: the lease and regain grace before a silent host's lease is given up, the release retry loop, the gap between the recorded launch time and the machine's creation, a tick, and (after a registry restart, until the next tick) an aged host being bookable again. A run the host keeps listing after its lease ended keeps `release_host` refused with no deadline: a lifetime-enforcing cloud cuts it at the lifetime, Lambda does not, so there you terminate by hand. |
| A secret may have leaked (5.1 or 5.2 failed) | Terminate first. Then delete the Tailscale OAuth client or the key's tag permission, rotate the Lambda API key, and remove the token file entries for the pool. |
| The abort cannot reach Lambda | Rotate the API key in the Lambda console; the scheduler stops being able to launch, and you terminate from the console. |

Notes the abort relies on:

- `launch = "plan"` stops new launches. Whether a pool put back to `plan` also releases the machines
  it holds is not verified here. Treat a running instance as yours to terminate until the log shows the
  release.
- Stopping the service does not terminate instances. They keep billing until they are terminated.
- Spend is bounded five ways (D§9): `max_instances`, the hour and day caps, the provider's own
  `max_machines` and launch interval, and `plan` with its second lock. **CI** proves the caps in the step,
  except for an offer priced at zero (#329). **LIVE** only that the figures you set are the ones the
  catalog prices.
- After an abort, verify by hand: Lambda's list has no instance with our tags, `tailscale status` has
  no device with the pool's tag, `/v1/pools` shows `instances.n` 0, and `orphans` and `alerts` are empty.

## What this run does not prove

The run covers one machine of one type in one region. It does not prove a second type, a second
region, the arm64 pin, behaviour under a restart of the scheduler while the instance is up (restart and
re-adopt: **CI** only, with fakes; and see #329), the lost-Welcome release at `boot_timeout` (#387 is open), or a stock image other than the one used. Re-run the live steps
when the stock image, the Tailscale pin or the agent pin changes.
