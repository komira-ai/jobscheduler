# First live launch on Compute Engine

This is the checklist for the first time a scheduler spends money on a Compute Engine instance. A lead and
the owner of the project run it together. It is an operator procedure: no pull request runs it. The design
is `docs/design/pools-providers-network.md`; the provider is `crates/js-provider-gce`. The steps that do not
depend on the cloud (the caps, the records file, the teardown order, the abort table) are those of
`docs/runbooks/first-live-launch.md`; the cloud-side shape is that of `docs/runbooks/first-ec2-launch.md`.
This file says what differs for Compute Engine and refers to those for the rest.

Each item is marked:

- **CI** a test in this repository proves it against fakes. It says nothing about the real cloud.
- **LIVE** only a person looking at the real systems can prove it. Nothing in CI covers it.

The service is started with `--config <file>` and nothing else. Stop at the first failed pass criterion; do
not go on to see what happens. Write down each step's output (with every secret masked) in the launch
record: the date, the scheduler version, the head of this repository, and who ran it.

The first live launch needs an explicit go from the CEO. Until then the pool stays at `launch = "plan"`.

## 1. Prerequisites

### 1.1 The machine provider

```toml
[[machine_provider]]
name       = "gcp"
kind       = "gce"
project    = "<project id>"
zones      = ["<zone>"]                                   # in preference order, none twice
network    = "global/networks/<network>"
subnetwork = "regions/<region>/subnetworks/<subnetwork>"  # every zone must be in this region
images     = { x86_64 = "projects/<project>/global/images/<image>", arm64 = "projects/<project>/global/images/family/<family>" }   # at least one architecture
# optional: purchase = ["on-demand"]      # the default; "spot" is the other term launched
# optional: allowed_machine_types = ["<machine type>"]
```

The kind refuses a key it does not know, so a mistyped key stops the start. There is no key for a
credential: a `private_key`, `api_key` or the like is refused naming the key. **CI**
(`kind_gce_builds_and_only_the_platform_kinds_are_refused`). An instance gets no external address and the
provider sets no service account on it.

Name the provider in a `[[platform]]` of kind `default` after `static`, and name the platform in the
`[[pool]]`, as in section 1 of `first-live-launch.md`. The pool sets `max_instances = 1`, the spend caps, the
lifetime keys (`idle`, `max_age`, `min_host_lifetime`) and `launch = "plan"`. **CI**
(`gce_pool_needs_the_lifetime_keys`). The provider's own `max_machines` is the pool's `max_instances`. With
`launch = "plan"` the provider is built disabled and in dry run: it makes no launch call. **CI**.

### 1.2 Credentials

The service never reads a credential from the file. The client uses the application-default credentials,
read once when the service starts, in this order:

1. the service-account key file that the variable `GOOGLE_APPLICATION_CREDENTIALS` names, set in the
   service's environment by the supervisor; the file is owned by the service user and readable by nobody
   else (mode 0600 or 0400); or
2. the service account of the machine the scheduler runs on, whose access token the metadata server hands
   out.

With neither source the calls fail as `NoCredential` and the pool launches nothing. **CI** (no credential
appears in any `Debug` or log line). **LIVE** that the credential is the intended one: use a service account
used only for this scheduler, in a project where every instance is ours to touch. `server.scheduler_id` is
unique in the project.

### 1.3 Permissions

The credential is allowed these permissions and nothing else in the project:

- launch side: `compute.instances.create`, `compute.instances.get`, `compute.instances.list`,
  `compute.instances.delete`, `compute.instances.setMetadata`, `compute.instances.setLabels`,
  `compute.zoneOperations.get`, `compute.disks.create`, `compute.subnetworks.use`,
  `compute.images.useReadOnly`; the insert carries the payload as metadata and the scheduler's marks as
  labels, so the cloud asks for `setMetadata` and `setLabels` on it though no separate call is made
- catalog side: `compute.machineTypes.list`, `compute.acceleratorTypes.list`, `compute.regions.get`, and the
  read of the Cloud Billing Catalog (the price list). The catalog is public data: the client sends the access
  token of the credential above to it, and no API key is configured. Check **LIVE** that the first
  catalog read in plan mode succeeds with the account used; if the Cloud Billing Catalog refuses the token,
  stop and tell the lead.

The predefined role `roles/compute.instanceAdmin.v1` holds the Compute Engine permissions above. It is
enough as it stands: `roles/iam.serviceAccountUser` is needed only when instances run as a service account,
and these do not. Grant the role on the one project, not on the organisation. **LIVE**.

If the image lives in another project, that project grants the credential `compute.images.useReadOnly` on
the image too.

### 1.4 Pre-checks

| # | item | pass criterion | mark |
|---|---|---|---|
| 1.4.1 | The APIs are enabled in the project: Compute Engine and Cloud Billing. | The console's API list shows both enabled. | LIVE |
| 1.4.2 | An image for each architecture the pool's offers have, built as `docs/runbooks/machine-images.md` says, readable by the project. An offer whose architecture has no image is not launched. | `gcloud compute images describe <image> --project <image project>` shows it `READY`, with the architecture the config names. | LIVE |
| 1.4.3 | The regional quotas cover one instance of the smallest allowed machine type: CPUs, the GPU family when the type has one, and Preemptible/Spot CPUs when `purchase` has `spot`. | The console's quota page shows each quota above the type's need and above what is in use. The catalog reads the same quotas, and an offer above them is not launched. | LIVE (the figure), CI (the check) |
| 1.4.4 | Each zone offers the machine type. | `gcloud compute machine-types list --zones <zone>` lists the type. | LIVE |
| 1.4.5 | The network and subnetwork give the host a route to `advertise_url` with no external address (Cloud NAT on the subnetwork, for one), and no inbound firewall rule is open to the hosts, port 22 included. The provider sets no firewall rule; the project's decides. | `gcloud compute firewall-rules list` shows no ingress allow rule for the network that reaches the hosts. | LIVE |
| 1.4.6 | `max_instances = 1`, and the hour and day caps set to the smallest allowed type's price for one hour and about twice that for a day. | `/v1/pools` shows `instances.max` 1 and the caps in cents. | CI (arithmetic), LIVE (the price is the cloud's) |
| 1.4.7 | `launch = "plan"`. | `/v1/pools` shows `launch: plan`. | CI |

On fail: fix the item. Nothing has been spent.

## 2. Plan mode

Start the service with `launch = "plan"`. Read `/v1/pools`: the pool is `available` once the catalog has
been read. The catalog reads are the only calls made in plan mode, and they spend nothing. Request a lease
and check that the status shows what would be launched (the machine type, the zone, the term) and that
`gcloud compute instances list` shows no new instance. **CI** (`a_plan_pool_launches_nothing`), **LIVE** (the
console).

## 3. The go

Both people agree, aloud, in the launch record: the caps, the abort steps of section 5, who watches the
instance list during the run, and that the CEO has given the go for this live launch.

1. Set `launch = "live"` in the pool and `boot = "initialise"` for this start only, as in step 4 of
   `first-live-launch.md`. These two lines are the only changes. Restart the service.
2. Request one lease, the way section 3 of `first-ec2-launch.md` does:
   `js lease acquire --class whole-node --max-wall 30m`. The answer is 202 `pending`.
3. Watch `/v1/pools` every 10 s and the instance list:

   ```
   gcloud compute instances list --project <project id> --filter="labels.js-scheduler=<scheduler id>"
   ```

| what you see | pass |
|---|---|
| One instance of the allowed machine type, in the zone, in the status `PROVISIONING`, `STAGING`, then `RUNNING`. It carries the labels `js-scheduler=<scheduler id>` and `js-acquire-id=<acquire id>`, and its name is the host name. | labels and name as above (**LIVE**) |
| A retry of the same request, or a restart, does not start a second instance: the provider looks the instance up by its labels before it inserts, and the insert carries a request id derived from the acquire id. | one instance in the list (**CI**: the fake keeps the request id and the labels; **LIVE**) |
| `last_launch.outcome` is `accepted`, then the machine is `booting`, then `registered` once the agent says hello within `boot_timeout`. | **CI** for the order, **LIVE** for the real boot |
| `instances.n` is 1 and `spend_cents_per_hour` is the catalog price. | **CI** (arithmetic), **LIVE** (the price is the cloud's) |
| The lease moves from 202 `pending` to held. GPU jobs on Compute Engine run on the fallback runtime until GPU passthrough exists; this run does not test one. | **LIVE** |

A launch the cloud refused leaves no record and no instance; the reason is in the log (a quota or capacity
refusal is held back for the offer's cool-down, an authorisation refusal is not retried). If the launch call
was lost and the scheduler cannot tell whether an instance exists, it holds the machine for `unknown_hold`
(600 s by default) and finds it by its labels. **CI** (`acquire` and `list` tests of the fake).

## 4. Teardown

Release the lease (`js lease release <id>`) or let `max_wall` end. The host drains after the pool's `idle`
time, its token is revoked before the release is sent, and the instance is deleted, then read back until the
cloud reports it gone (a `TERMINATED` instance is still there). The order is the one in section 6 of `first-live-launch.md`. **CI**
(release and read-back against the fake), **LIVE** (the instance list agrees).

Pass for the run: the instance carried our labels and name; a hello arrived; the lease ran; the instance is
gone from `gcloud compute instances list --filter="labels.js-scheduler=<scheduler id>"`; `instances.n` is 0;
no spend after the release. Open the billing page after an hour and compare the charge with
`max_spend_per_hour`. Then return the pool to `launch = "plan"`, set `boot = "resume"` (check
`<state file>.scaler` exists first), and restart, so the next spend is a new go.

## 5. Abort and rollback

Stop spend at the first doubt.

| situation | action |
|---|---|
| Before the flip | Nothing was launched. Leave `launch = "plan"`. |
| After the flip, before a launch | Set `launch = "plan"` and restart. Release the lease request. |
| An instance exists and the scheduler is healthy | Release the lease; let the teardown run. Watch it. |
| An `alerts` entry about a machine, or a registered host that never heartbeated past `boot_timeout` | Delete the instance by hand: `gcloud compute instances delete <name> --zone <zone> --project <project id>`. Instances are found with `gcloud compute instances list --filter="labels.js-scheduler=<scheduler id>"`. |
| The scheduler is wedged or unreachable | The same action. Stopping the service does not delete instances; they bill until deleted. |
| A secret may have leaked | Delete first. Then rotate the service-account key, or remove the role from the service account, and rotate any join token. |

After an abort, verify by hand: the list with the filter `labels.js-scheduler=<scheduler id>` shows no
instance, `/v1/pools` shows `instances.n` 0, and `orphans` and `alerts` are empty.

## What this run does not prove

One instance of one machine type in one zone. It does not prove spot, the second architecture, a launch
against the quota edge, a restart of the scheduler while the instance is up (**CI** only, with fakes), or
the signed instance identity: the agent sends none yet, so a host is matched by its name and instance id.
