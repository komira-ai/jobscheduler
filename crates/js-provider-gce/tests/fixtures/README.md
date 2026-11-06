# Fixtures of js-provider-gce

Three directories:

- `api/`: one answer of the Compute Engine API or the Cloud Billing Catalog API per file.
  `tests/fixtures_parse.rs` and the modules under `tests/fixtures_parse/` feed each file's
  bytes through the crate's own client (`HttpGce` over a scripted transport) and assert the
  typed result.
- `credential/`: a made-up service-account key.
- `identity/`: made-up signed identity tokens and the modulus of the key that signed them.

## Provenance of `api/`

No file in `api/` is a capture. Each one is written from the provider's published response
shape (the Google Compute Engine REST reference for `machineTypes`, `acceleratorTypes`,
`regions`, `instances` and `zoneOperations`, the Cloud Billing Catalog API reference for
`services.skus`, and the error models of both). The column "provenance" says so for every
file: **documented response shape, not yet captured from a live account**. The values in
them (prices, quota limits, ids, timestamps) are examples, not what any account saw on any
day. A file's contents may therefore differ from a real answer in fields the reference
leaves optional and in the exact wording of messages.

Placeholders used in `api/`:

| Value | Placeholder |
|---|---|
| Project id | `example-project` |
| Project number (in messages) | `123456789012` |
| Region and zones | `us-central1`, `us-central1-a`, `us-central1-b`; `us-central1-c` and `us-central1-f` appear only as scopes the tests do not ask for or that hold a warning |
| Instance id | `1234567890123456789` and, in the list, `12345678901234567nn` (a string, as the API writes it) |
| Instance name | `js-0123456789ab-fedcba987654` and, in the list, `js-0123456789ab-fedcba98765n` |
| Scheduler label | `example-scheduler`; acquire labels `acquire-000n`; user label `js-label-validation-run` = `example-run-1` |
| Operation name | `operation-1700000000000-6000000000001-00000001-0000a001` |
| IPv4 addresses | `192.0.2.0/24`, `198.51.100.0/24`, `203.0.113.0/24` only (`192.0.2.10` stands for an internal address) |
| Page tokens | `EXAMPLE-PAGE-TOKEN-1` (machine types), `-2` (SKUs), `-3` (instances) |
| Fingerprints | `EXAMPLE-FINGERPRINT=` |
| Boot payload (`user-data` metadata) | `REDACTED-BOOT-PAYLOAD` |
| Service-account email | `js-launcher@example-project.iam.gserviceaccount.com` |
| SKU ids | `0000-0000-nnnn` |
| Timestamps | `2026-11-05T...`, an example, not a capture date |

`tests/fixtures_parse/hygiene.rs` fails on an IPv4 address outside the three documentation
ranges, a twelve-digit number other than `123456789012`, and an access key id
(`AKIA`/`ASIA` and sixteen capitals or digits) other than `AKIAIOSFODNN7EXAMPLE`. It
also fails on any file under this directory that no test names.

## Files of `api/`

| File | Call that produces it | Case | Provenance | Region or zone |
|---|---|---|---|---|
| `machine_types_aggregated_page1.json` | `machineTypes.aggregatedList` (`GET /projects/{project}/aggregated/machineTypes?returnPartialSuccess=true&maxResults=500`) | first page: `n1-standard-4` and `a2-highgpu-1g` (built-in A100, stated `X86_64`) in zone a, `n1-standard-4` in a zone the tests do not ask for, `nextPageToken` | documented response shape, not yet captured from a live account | us-central1-a, us-central1-c |
| `machine_types_aggregated_page2.json` | same call with `pageToken=EXAMPLE-PAGE-TOKEN-1` | last page: `n1-standard-4`, `a2-highgpu-1g` and `t2a-standard-4` (stated `ARM64`) in zone b; zone f holds only a `NO_RESULTS_ON_PAGE` warning | documented response shape, not yet captured from a live account | us-central1-b, us-central1-f |
| `accelerator_types_aggregated.json` | `acceleratorTypes.aggregatedList` | A100 (16 per instance) and T4 (4) in zone a, T4 in zone b, a warning-only scope for zone f; the `zone` of each type is a URL | documented response shape, not yet captured from a live account | us-central1-a, -b, -f |
| `regions_get_us_central1.json` | `regions.get` (`GET /projects/{project}/regions/us-central1`) | the region with its `quotas`: limits and usage written as `24.0`; `PREEMPTIBLE_CPUS` has limit 0 | documented response shape, not yet captured from a live account | us-central1 |
| `billing_skus_page1.json` | `services.skus.list` (`GET /v1/services/6F81-5844-456A/skus?currencyCode=USD&pageSize=5000`) | first page: N1 core and memory, on-demand and Spot Preemptible; plus three SKUs the catalog leaves out (the same words offered in `us-east1` only, a one-year commitment, a storage SKU); `nextPageToken` | documented response shape, not yet captured from a live account | serviceRegions us-central1 (and us-east1 for the SKU left out) |
| `billing_skus_page2.json` | same call with `pageToken=EXAMPLE-PAGE-TOKEN-2` | last page: T4 (on-demand with two tiers, Spot), A2 core and memory (on-demand, Spot), A100 (on-demand, and Spot with no `startUsageAmount`); memory SKUs use `usageUnit` `GiBy.h`, the others `h` | documented response shape, not yet captured from a live account | serviceRegions us-central1 |
| `instances_insert_operation.json` | `instances.insert` (`POST /projects/{project}/zones/{zone}/instances?requestId=...`) | the zone Operation it answers: `RUNNING`, `targetId` set | documented response shape, not yet captured from a live account | us-central1-a |
| `zone_operations_get_running.json` | `zoneOperations.get` | still `RUNNING`, progress 50 | documented response shape, not yet captured from a live account | us-central1-a |
| `zone_operations_get_done.json` | `zoneOperations.get` | `DONE` with no `error` | documented response shape, not yet captured from a live account | us-central1-a |
| `zone_operations_get_done_zone_resource_pool_exhausted.json` | `zoneOperations.get` | `DONE` with `error.errors[].code` `ZONE_RESOURCE_POOL_EXHAUSTED` | documented response shape, not yet captured from a live account | us-central1-a |
| `zone_operations_get_done_quota_exceeded.json` | `zoneOperations.get` | `DONE` with `error.errors[].code` `QUOTA_EXCEEDED` | documented response shape, not yet captured from a live account | us-central1-a |
| `instances_get_provisioning.json` | `instances.get` | one instance, `PROVISIONING`, with the three ownership labels | documented response shape, not yet captured from a live account | us-central1-a |
| `instances_aggregated_list_page1.json` | `instances.aggregatedList` (`GET /projects/{project}/aggregated/instances?returnPartialSuccess=true&filter=labels...`) | first page of the instances that carry the scheduler's labels: `PROVISIONING`, `STAGING`, `RUNNING` (an A2 with a guest accelerator); a warning-only scope; `nextPageToken` | documented response shape, not yet captured from a live account | us-central1-a, us-central1-f |
| `instances_aggregated_list_page2.json` | same call with `pageToken=EXAMPLE-PAGE-TOKEN-3` | last page: `STOPPING`, `TERMINATED`, `SUSPENDED` | documented response shape, not yet captured from a live account | us-central1-b |
| `instances_aggregated_list_empty.json` | `instances.aggregatedList` | no instance carries the labels: every scope holds only a warning | documented response shape, not yet captured from a live account | us-central1-a, us-central1-b |
| `instances_delete_operation.json` | `instances.delete` (`DELETE /projects/{project}/zones/{zone}/instances/{id}`) | the zone Operation it answers: `RUNNING`, `targetId` set | documented response shape, not yet captured from a live account | us-central1-a |
| `error_not_found_404.json` | any instance call | HTTP 404, `errors[0].reason` `notFound` | documented response shape, not yet captured from a live account | us-central1-a |
| `error_already_exists_409.json` | `instances.insert` | HTTP 409, reason `alreadyExists` | documented response shape, not yet captured from a live account | us-central1-a |
| `error_rate_limit_exceeded_403.json` | any call | HTTP 403, reason `rateLimitExceeded` (domain `usageLimits`) | documented response shape, not yet captured from a live account | none |
| `error_rate_limit_exceeded_429.json` | any call | HTTP 429, reason `rateLimitExceeded`, status `RESOURCE_EXHAUSTED` | documented response shape, not yet captured from a live account | none |
| `error_quota_exceeded_403.json` | any call | HTTP 403, reason `quotaExceeded` (domain `usageLimits`); read as a denial today, see komira-ai/jobscheduler#768 | documented response shape, not yet captured from a live account | none |
| `error_forbidden_403.json` | `instances.insert` | HTTP 403, reason `forbidden`, a missing permission | documented response shape, not yet captured from a live account | us-central1-a |
| `error_auth_error_401.json` | any call | HTTP 401, reason `authError` | documented response shape, not yet captured from a live account | none |
| `error_invalid_400.json` | `instances.insert` | HTTP 400, reason `invalid`, a field value the API refuses | documented response shape, not yet captured from a live account | none |
| `error_bad_request_400.json` | any call | HTTP 400, reason `badRequest` | documented response shape, not yet captured from a live account | none |
| `error_backend_error_503.json` | any call | HTTP 503, reason `backendError`, status `UNAVAILABLE` | documented response shape, not yet captured from a live account | none |
| `error_internal_error_500.json` | any call | HTTP 500, reason `internalError`, status `INTERNAL` | documented response shape, not yet captured from a live account | none |
| `error_billing_permission_denied_403.json` | `services.skus.list` | Cloud Billing error in the `google.rpc` form: HTTP 403, `status` `PERMISSION_DENIED`, `details` with an `ErrorInfo`, no `errors` list | documented response shape, not yet captured from a live account | none |
| `error_billing_resource_exhausted_429.json` | `services.skus.list` | Cloud Billing error: HTTP 429, `status` `RESOURCE_EXHAUSTED`, `ErrorInfo` reason `RATE_LIMIT_EXCEEDED` | documented response shape, not yet captured from a live account | none |

The HTTP status of an error file is not part of the file: the test passes it with the file's
bytes, as the cloud sends it in the status line.

## The other directories

Both are made up for the tests that read them; neither is a capture of anything.

| File | Used by | What it is |
|---|---|---|
| `credential/test-key.json` | `tests/http/credential.rs`, `tests/secrets/secrets.rs` | A service-account key file for a made-up account (`sample-project`) with a throwaway RSA key. The tests build a credential from it, check the assertion it signs, and check that no part of the key reaches a debug text, an error or a log line. |
| `identity/modulus.hex` | `tests/offline/identity.rs` | The modulus of the throwaway RSA key (hex) that signed the tokens below; the private half was destroyed. The verifier under test is built with it as its only key. |
| `identity/good.jwt` | `tests/offline/identity.rs` | A correctly signed token for the instance and project of the tests. |
| `identity/other-instance.jwt`, `identity/other-project.jwt`, `identity/bare-issuer.jwt` | `tests/offline/identity.rs` | Correctly signed tokens that name another instance, another project, or give the issuer without its scheme; all are accepted with the claims they carry. |
| `identity/altered-signature.jwt` | `tests/offline/identity.rs` | A token whose signature bytes were changed: rejected. |
| `identity/hs256.jwt`, `identity/no-alg.jwt`, `identity/no-kid.jwt`, `identity/unknown-kid.jwt` | `tests/offline/identity.rs` | Correctly signed tokens whose header does not say RS256, names no algorithm, names no key, or names a key the verifier does not hold: rejected. |
| `identity/other-issuer.jwt`, `identity/no-issuer.jwt`, `identity/no-google.jwt`, `identity/no-compute-engine.jwt`, `identity/missing-claim.jwt` | `tests/offline/identity.rs` | Correctly signed tokens whose claims name another issuer, none, no `google` claim, no `compute_engine` claim, or lack one of the four fields: rejected. |
| `identity/claims-not-base64.jwt`, `identity/claims-not-json.jwt` | `tests/offline/identity.rs` | Tokens whose claims are not base64, or not JSON: rejected. |

## After a live run

The person who ran the live suite (`docs/runbooks/live-conformance.md`) re-captures any
answer that differs from the committed file, and:

1. Takes the raw answer. The live suite's `--capture-dir` flag writes the raw answers it
   received (see the runbook); for a call it does not make, send the request with any HTTP
   client and the access token of `gcloud auth print-access-token`.
2. Sanitises it by the rule above: the project id becomes `example-project` and the project
   number `123456789012`; instance, operation and SKU ids, instance names, labels and
   fingerprints become the placeholders in the table; every IPv4 address becomes one in a
   documentation range; the `user-data` metadata value becomes `REDACTED-BOOT-PAYLOAD`;
   service-account emails become `js-launcher@example-project.iam.gserviceaccount.com`;
   page tokens become `EXAMPLE-PAGE-TOKEN-n`. Prices, quota limits, machine shapes, statuses
   and error reasons are kept as received. Messages that echo the request (a field value, a
   resource path) are kept with the project and names replaced.
3. Replaces the file under `api/`, changes its "provenance" cell to the capture date and the
   region or zone, and puts the diff in the pull request. If the diff shows a field the
   parser does not read, or a shape the tests do not assert, that is the finding the live
   run exists to produce: add the assertion, or the parser change, in the same pull
   request.
4. Runs `cargo test -p js-provider-gce`; the placeholder check fails on any address, number
   or key id the sanitising missed.
