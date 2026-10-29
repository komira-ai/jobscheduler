# First live launch on Lambda Cloud

This is the checklist for the first time a scheduler spends money on a Lambda Cloud GPU
instance. A lead and the owner of the account run it together. The design is
`docs/design/pools-providers-network.md`; sections are cited as `D§n` and open questions as `Qn`.

Each item is marked:

- **CI** a test in this repository proves it against fakes. It says nothing about Lambda,
  Tailscale or the stock image.
- **LIVE** only a person looking at the real systems can prove it. Nothing in CI covers it.

It assumes these changes have landed: the scaler's shell (#287: `launch = "live"`, records file,
per-arch agent pins with sha256, `--host-id`, the GPU toolkit hook, `--container-pull missing`),
the Tailscale DNS and socket-wait change, and digest-only images. Their key names are not repeated here; use
the config reference of the version you deploy. Anything below that says "per the landed change"
was not checked against that change when this was written.

Stop at the first failed pass criterion. Do not go on to a later step to see what happens.
Section 7 says how to stop spend from any step.

Write down each step's output (with every secret masked) in the launch record: the date, the
scheduler version, the head of this repository, and who ran it.

## 1. Prerequisites

| # | item | pass criterion | mark |
|---|---|---|---|
| 1.1 | A Lambda Cloud account used only for this scheduler, or one where every instance is ours to touch. The scheduler id (`server.scheduler_id`, default `js`) is unique on the account (D§5.2, F16). | `Instances` page lists nothing that is not yours. | LIVE |
| 1.2 | An API key file for the account, mode 0600 or 0400, owned by the service user. | `stat -c '%a %U' <file>` prints `600 <service user>`. A file others can read stops the start. | CI (the reader refuses), LIVE (the key is valid) |
| 1.3 | Exactly one ssh key on the account, or `ssh_key = "<name>"` in the provider table. The key is never used to log in. | Account has one key, or the config names one. With none, the pool shows `provider.ssh_key: account has 0 keys; name one`. | CI (the rule), LIVE (Lambda accepts the name) |
| 1.4 | A Tailscale OAuth client with only the scopes `auth_keys` (write) and `devices:core` (write), limited to the one tag, and a tailnet policy that lets that tag reach the scheduler's `agent_listen` port and nothing else (D§8.3). The client file holds `client_id` and `client_secret`, mode 0600. | Admin console shows the client with those two scopes and one tag. The policy has one rule for the tag, to the agent port only. | LIVE. The service cannot see the client's scopes or the policy (Q22: the policy warning is not built). |
| 1.5 | The scheduler joined to the tailnet (its own `tailscaled`), TLS certificate for its tailnet name, `advertise_url = "https://js.example.ts.net:<agent port>"`. Start-up refuses a non-loopback `http://` for an elastic pool (D§9, Q13). | `tailscale status` on the scheduler host shows the node. `agent_listen` is the tailnet address, not a wildcard. | CI (the refusal), LIVE (the certificate and route) |
| 1.6 | One agent pin per arch the pool can launch: an `https://` url and the sha256 of that file (64 lower-case hex digits). | For each arch: download the url from a clean machine, `sha256sum` it, compare with the config. The file is the release you mean to run: `js-agent --version`. | CI (a pin must be well formed and is required with `live`), LIVE (the digest is of the right file) |
| 1.7 | The pinned Tailscale static build: url and sha256. | Same check as 1.6 against the url in the config. | CI (shape), LIVE (digest matches the file) |
| 1.8 | Caps. `max_instances = 1`; `max_spend_per_hour` and `max_spend_per_day` set to the smallest GPU type's price times one instance for one hour, and about twice that for the day. | The config has all three. `max_instances` 1 and a day cap of a few hours of one instance. | CI (they are required, and a cap is enforced by the step), LIVE (the prices the catalog returns are the ones you budgeted) |
| 1.9 | `launch = "plan"` in the pool. A second lock: the provider is built disabled and in dry run unless `launch = "live"`. | `grep launch <config>` prints `plan`. | CI (`a_plan_pool_launches_nothing`) |
| 1.10 | A client token file for the lease routes, and the `js` CLI on a machine that can reach `listen`. | `js --server https://js.example.ts.net:<port> --token-file <file> --ca-cert <pem> lease list` prints an empty list. | CI (CLI), LIVE (the route) |

On fail: fix the item. Nothing has been spent.

## 2. Plan mode

Start the service with the config from step 1 and `launch = "plan"`. From a peer on the private
or tailnet network (not from a launched host, and not from the open internet):

```sh
curl -sS https://js.example.ts.net:<port>/v1/pools
curl -sS https://js.example.ts.net:<port>/status      # the same data as a page
curl -sS https://js.example.ts.net:<port>/healthz
```

The status routes need no token. They answer 404 on a `listen` address that is not private and to a
peer that is not private (**CI**: `status_gate_tests`).

Look at, for the pool:

- `state` is `available` (it begins `checking`; wait one pass, 60 s at most, 10 s while it fails).
- `credentials.ok` is true, `network_check.ok` is true, `reasons` is empty, `warnings` is empty.
  No `server: warning: binds all interfaces`.
- `instances.n` is 0, `instances.max` is 1, `blocked` is null, `no_offer` is null.
- `next_launch` holds the offer, the zone, a name, and a plan in which `join_token` and
  `network.ts_authkey` print as `***`, with the agent and Tailscale urls and digests you pinned.

Pass: every check green, `next_launch` as above, no secret in either response
(`curl ... | grep -c` for the first characters of the join-token file and of the OAuth secret
prints 0).

| claim | mark |
|---|---|
| The pool becomes `available` only after one full pass; a plan pool launches nothing | CI |
| The page cannot show a plan the launch would not follow; the secret slots print `***` and a test greps the serialised status for known secrets | CI |
| The credentials and the OAuth client are accepted by the real APIs; the catalog has the GPU type and its price | LIVE |
| No process on the scheduler host or in its logs holds a secret in clear | LIVE (read the log once) |

On fail: read `reasons` and the log, fix the config, restart. A refused key is
`credentials: failed (<code>)`; a refused OAuth client is `network: credentials failed`. Do not go on.
`check()` does not mint a Tailscale key, so a green network check does not prove the mint works
(D§8.5); step 4 does.

## 3. Pre-spend human checks

These are things the code cannot prove. Do both before the flip. Record the answers; if either fails,
stop and do not set `live`.

### 3.1 No inbound, including port 22 (D§9, Q10)

The launch attaches a firewall ruleset named `js-<scheduler id>-noinbound` that holds no rules. Lambda
also has account-wide rules, and they may still allow port 22.

1. In the Lambda console, open Firewall. Read the account-wide (global) rules. List each inbound rule and
   its port, in the launch record.
2. Confirm the ruleset `js-<scheduler id>-noinbound` of the region exists with zero rules, or does not
   exist yet (the first launch creates it; a same-named ruleset with rules is refused,
   **CI**).
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

1. Open Lambda's current API reference. Confirm the launch call's field names: `user_data`,
   `ssh_key_names`, `firewall_rulesets`, `tags`, `name`, as sent in
   `crates/js-provider-lambdalabs/src/wire.rs`.
2. Read the stated limit and the unit. Record it.
3. Print the masked plan size: the rendered bootstrap for the pool is in `next_launch.plan`; a plan
   over the limit is a `Shape` refusal before any call (**CI**).

Pass: the names match and the limit is at least 10^6 bytes, with the rendered plan well under it.
**LIVE** (documents). A launch rejected for a field name or size shows as a refusal in `last_launch`
and is not a launch; it costs nothing.

### 3.3 Optional: the provider alone

`crates/js-provider-lambdalabs/examples/first_launch.rs` launches one instance through the provider with
hard boot, wall-clock and spend limits and no scaler or bootstrap. Its dry run (the default) prints a plan with a
hash; `--go --plan-hash <hash>` launches only that plan; `--teardown-only <state file>` terminates what
it launched. Use it only if you want the field names and limit proven before involving the scheduler.
It spends money, so it needs the same go as step 4. **LIVE.**

## 4. The go

Both people agree, aloud, in the launch record: the caps, the abort steps of section 7, and who watches
the Lambda console during the run.

1. Change one line in the pool: `launch = "live"`. Review the diff: it is the only change.
   Restart the service.
2. Read `/v1/pools` again: `launch` is `live`, `state` is `available`.
3. Request one lease:

   ```sh
   js --server https://js.example.ts.net:<port> --token-file <client token> --ca-cert <pem> \
      lease acquire --class whole-node --gpus 1 --gpu-class <class from types> --max-wall 30m
   ```

   Use the class and `gpu` spelling from the pool's `types` row. The answer is 202 `pending`: the machine
   does not exist yet.
4. Watch `/v1/pools` every 10 s. The machine record (`machines[].state`) goes Planned, Booting, Registered.
   Also watch the Lambda console list.

| what you see | pass |
|---|---|
| `last_launch.outcome` accepted, `host` set; the instance in Lambda's list carries our name and tags `jobscheduler:scheduler`, `jobscheduler:pool`, `jobscheduler:acquire`, and the firewall ruleset | the tags and name are as above (**LIVE**) |
| Booting within one tick of the request | **CI** for the order (record written before the call, one launch per request) |
| A tailnet device named like the host appears, then Registered once the host says hello, within `boot_timeout` (default 15 min) | **LIVE.** Time it: the Tailscale key lives 10 minutes (Q11). |
| `instances.n` = 1 and `spend_cents_per_hour` is the catalog price | **CI** (arithmetic), **LIVE** (price is Lambda's) |
| The lease moves from 202 `pending` to held on `js lease list` | **CI** (flow), **LIVE** (a real host) |

On fail:

- Booting longer than `boot_timeout` with no hello: the status reads "booting, no hello". The scaler
  releases the machine and starts `boot_fail_cooldown` (10 min). Watch it do that; if the instance is
  still in Lambda's list 5 min after `boot_timeout`, use section 7. Reasons to read: the Lambda
  console's output for the instance, `tailscale status` for the device, the scheduler's log.
- A refused launch: read `last_launch` and `blocked` (`max_instances`, `max_spend`, `max_spend_day`,
  `boot_failed`, `unavailable`). No instance exists.
- `network: key mint failed: <status>`: no launch was made. Fix the OAuth client's scopes or tag.
- A host refused `host-contract` (no container runtime or GPU toolkit): the stock image lacks something
  the install table does not carry (Q5). The table starts empty; this is expected on the first run.
  Record what was missing and tear down (section 6).

## 5. On the live host, while it runs

You cannot ssh in: that is the point. Run each of these as a container job on the leased host (the
run routes, or `js lease run`), or from outside, as stated. Do them in order,
while the lease is held. Keep the `max_wall` short.

### 5.1 Containers cannot reach the metadata address (D§7.3 step 8, F12b)

Run, as a job in a container on the host, a request to the provider's metadata address
(the link-local address for the cloud; look it up in Lambda's documentation and record it) with a 5 s
timeout, for the instance's user data.

Pass: the request times out or is refused; no user data is returned. **LIVE.** The bootstrap's
rule for this is not verified against the stock image (Q5), and CI tests only the rendered text.
On fail: stop, tear down (section 6), and do not run another job on any launched host until
the rule is fixed. The token is retired at the first hello, which limits what a reader of user data gets
(5.3), but the Tailscale key and agent url are in user data.

### 5.2 Cloud-init copies of user data: fill the cleanup list (D§7.3 step 7, Q5)

On the host, through a job with the root filesystem visible (or the agent's host-visible mount), and
before the host is released:

```sh
find / -xdev -newer /etc/jobscheduler/instance -type f 2>/dev/null | head -200
grep -rIl 'tskey-' / --exclude-dir=proc --exclude-dir=sys 2>/dev/null
grep -rIl 'jobscheduler/token' / --exclude-dir=proc --exclude-dir=sys 2>/dev/null
```

The `tskey-` pattern finds a Tailscale auth key; the second finds the rendered script, which writes the token. Compare the list with `CLOUD_INIT_TRACES` in
`crates/js-lease-server/src/bootstrap/mod.rs`.

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
   (set it short for this run). `machines[].state` goes Registered, then
   Draining, then Releasing. **CI**: a host is released only while it has no lease, run or stream.
3. Order, as the log and the status show it: the host's token or credential is revoked **before** the
   release is sent. If the revoke fails, the release waits and the revoke is retried each tick.
   **CI** (`order::*` and the crash test in the scaler crate).
4. The release is read back until the instance is gone. The status shows the machine
   until Lambda's list does not hold it. **LIVE** that Lambda's list agrees (open the console).
5. The tailnet device is deleted: `tailscale status` and the admin console no longer list the host name.
   A failed delete is logged and does not block the release; delete it by hand if so. **CI** (the
   delete, fake API), **LIVE** (the real one).
6. The join token and the host credential are revoked: the agent routes answer 401 to both. **CI**
   (revoke), **LIVE** (against the running service).
7. The Tailscale OAuth client and the Lambda API key remain; revoke or rotate them if the run showed any
   secret in a log or on a host path (5.2).
8. Return the pool to `launch = "plan"` and restart, so the next spend is a new go.

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
| An instance exists and the scheduler is wedged, unreachable or restarted badly | Terminate the instance in the Lambda console (or `first_launch --teardown-only` if it launched it). Instances are found by the tags `jobscheduler:scheduler=<id>`, `jobscheduler:pool=<pool>` and the name. Then delete the tailnet device in the admin console and restart the scheduler: it lists the account, finds no instance, and its record for the machine ends. |
| A secret may have leaked (5.1 or 5.2 failed) | Terminate first. Then delete the Tailscale OAuth client or the key's tag permission, rotate the Lambda API key, and remove the token file entries for the pool. |
| The abort cannot reach Lambda | Rotate the API key in the Lambda console; the scheduler stops being able to launch, and you terminate from the console. |

Notes the abort relies on:

- `launch = "plan"` stops new launches. Whether a pool put back to `plan` also releases the machines
  it holds is not verified here. Treat a running instance as yours to terminate until the log shows the
  release.
- Stopping the service does not terminate instances. They keep billing until they are terminated.
- Spend is bounded five ways (D§9): `max_instances`, the hour and day caps, the provider's own
  `max_machines` and launch interval, and `plan` with its second lock. **CI** proves the caps in the step.
  **LIVE** only that the figures you set are the ones the catalog prices.
- After an abort, verify by hand: Lambda's list has no instance with our tags, `tailscale status` has
  no `tag:js-agent` device, `/v1/pools` shows `instances.n` 0 and no `orphans`.

## What this run does not prove

The run covers one machine of one type in one region. It does not prove a second type, a second
region, the arm64 pin, behaviour under a restart of the scheduler while the instance is up (restart and
re-adopt: **CI** only, with fakes), or a stock image other than the one used. Re-run the live steps
when the stock image, the Tailscale pin or the agent pin changes.
