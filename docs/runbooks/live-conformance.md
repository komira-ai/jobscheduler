# Live conformance of a machine provider

This is the checklist for running the live conformance suite of one machine provider (EC2, Compute
Engine or Lambda) against the real cloud, and for refreshing the provider's response fixtures from what
the cloud answered. The suites are `crates/js-provider-ec2/tests/live.rs`,
`crates/js-provider-gce/tests/live.rs` and `crates/js-provider-lambdalabs/tests/live.rs`, behind the cargo
feature `live`. Each `live.rs` is only the `main` of its suite; what the flags make of the account, the
provider table, the key file reader and (EC2 and Compute Engine) the transport that writes the answers to
`--capture-dir` are in the crate's `tests/live/cloud.rs`. The ordinary test target `tests/live_cloud.rs` of
the crate includes that file and tests it without a network or a credential. The fixtures are in each
crate's `tests/fixtures`, and their `README.md` says which file comes from which call.

Each item is marked:

- **CI** a test in this repository proves it against fakes. It says nothing about the real cloud.
- **LIVE** only a person looking at the real systems can prove it. Nothing in CI covers it.

## 1. What it proves, and what it costs

The fixture tests feed documented response shapes through the provider's parser. They cannot show that
the cloud still answers in those shapes, that its error codes are the ones the provider maps, or that
a launch, a read-back and a terminate work with a real credential. The live suite does: it reads the
real catalog, launches three machines one at a time (`acquire_is_idempotent_on_the_acquire_id`,
`accepted_means_launched_not_ready`, `list_and_observe_see_the_machine`), and ends with a cleanup pass
that lists the run's machines by their marks and terminates what is left. **LIVE**.

Every machine carries the label `validation-run=<run id>` and the scheduler mark `live-<run id>`. The
suite never touches a machine without them.

The most a run can spend is three launches at the plan's price per hour, if each machine is gone within
its first hour. The dry run prints this bound (`cost bound`) before anything is launched. Billing minimums
and storage are the cloud's; read them in the console. **LIVE**.

## 2. The go

A spending run needs the CEO's explicit go, written in the run's record. Until then only dry runs are
made. A dry run launches nothing, and reads only the catalog and the list of the run's machines.

## 3. Prerequisites

- The credential, by the provider's usual chain, allowed the actions of the provider's launch runbook:
  `first-ec2-launch.md` section 1.3 (EC2; `AWS_*` variables or the instance role), `first-gce-launch.md`
  section 1.3 (Compute Engine; `GOOGLE_APPLICATION_CREDENTIALS`) and `first-live-launch.md` section 1
  (Lambda; the API key in a file). **LIVE**.
- The quotas and the image of the launch runbook's pre-checks (section 1.4 of the EC2 and Compute Engine
  runbooks), for the location and shape chosen. **LIVE**.
- For the workflow `.github/workflows/provider-live.yml`: the repository environment `provider-live`
  with a required reviewer, and its secrets `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`,
  `GCE_SERVICE_ACCOUNT_KEY` and `LAMBDALABS_API_KEY` (the ones for the providers to be run). **LIVE**.

## 4. Steps

1. Dry run. By hand, from a checkout, with the held script
   `tools/held/provider-live/provider_live.sh --provider ec2 --run-id <run id> --location <region> --shape <type> --image <ami>`
   (Compute Engine: `--location <zone> --shape <type> --project <project> --image <image>`, where the type
   is of a series the provider prices, such as `n1-standard-1`; Lambda:
   `--location <region> --shape <type> --credential-file <key file>`). `--shape` is required.
   Choose the cheapest type that boots the image: the suite launches only the shape it is told and
   never picks one, and it refuses a shape that is missing from the catalog for the location, is
   marked unavailable, cannot boot the image, or has no price (exit status 1, nothing launched). Or run the workflow with
   `dry_run` left true. The run id is 1 to 40 characters of `a-z`, `0-9` and `-`, and is used once.
   **LIVE**.
2. Read the plan: provider, run id, scheduler id `live-<run id>`, location, every provider flag (`provider flags: name=value, ...`, sorted by name), the offer
   chosen (the `--shape` given), its price in US cents per hour, the three launches, the cost bound, and the line `plan hash: <hash>`. Pass: the offer is the one you meant, the
   bound is within what the CEO agreed, the catalog check printed no failure, and no leftover of an
   earlier run with this id is listed. Stop at the first miss. Nothing has been spent. **LIVE**.
3. Get the go of section 2.
4. Spend run: the same command with `--spend --plan-hash <hash>` (the workflow: `dry_run` false and the
   `plan_hash` input; the environment's reviewer approves the run). The suite refuses, and launches
   nothing, when the plan it computes now hashes to another value, as it does when a price, the shape or a
   provider flag (the image, the project, the key file) changed since the dry run. Do not cancel the run: a cancelled run, or a workflow run stopped by its 90-minute timeout, skips the
   cleanup pass, so go to section 5 and check the console by hand. **LIVE**.
5. Read the result. The exit status is 0 for a pass, 1 for a failure, a leftover, a cleanup that could not look or panicked, or a changed plan, and 2
   for a usage error. The suite reports each case as passed or failed, and the cleanup pass reports
   each machine it terminated and any it could not. Pass: every case passed and no `LEFT OVER` line. **LIVE**.
6. Check the console. Pass: no instance with the label `validation-run=<run id>` is running. **LIVE**.

## 5. LEFT OVER

A `LEFT OVER` line, or exit status 1 with a machine still listed, means a machine did not stop. Terminate
it by hand in the provider's console, then check it is gone. Find it by the label `validation-run=<run id>`
(on EC2 the tag `js-label:validation-run`, on Compute Engine the label `js-label-validation-run`, on
Lambda the tag `jobscheduler:validation-run`), or by the scheduler mark `live-<run id>` (EC2 tag `js-scheduler`,
Compute Engine label `js-scheduler`, Lambda tag `jobscheduler:scheduler`). Write the machine and the time
it was terminated in the run's record. **LIVE**.

The suite does not clean what is free: Lambda's firewall ruleset `js-<scheduler id>-noinbound` and the
account's ssh key stay after the run.

## 6. Refresh the fixtures

A response that differs from the committed fixture is a finding, and the fixture is replaced.

1. For EC2 and Compute Engine, add `--capture-dir <directory>` to the run, a directory outside the
   checkout (the held script refuses one inside it). The suite writes the raw
   answers it received there. For Lambda, take the same calls with any HTTP client and the key file.
   **LIVE**.
2. The raw answers hold real ids, addresses and account numbers. Sanitise each by the rule in the crate's
   `tests/fixtures/README.md`, and replace the file of the same call and case. Never commit the capture
   directory itself.
3. In that README, change the file's provenance cell to the capture date and the region or zone.
4. Run `cargo test -p js-provider-<provider>`: the fixture tests parse the new file. A failure is a parser
   that does not read what the cloud sends; fix it in its own change.
5. Put the fixture diff in a pull request.
