# Lambda Cloud API fixtures

The files here are the answers of the Lambda Cloud API that the tests feed to the crate's
own client. `tests/fixtures_parse.rs` serves each one from a server on 127.0.0.1 to a real
`HttpApi` and asserts the typed result; `tests/http/*.rs`, `tests/logging.rs` and
`tests/user_data_logging.rs` serve them too; `tests/offline/host_prep.rs` compares against
the two `host_prep_*.txt` files.

**None of these files is a capture.** Every JSON file was written by hand from the response
shapes in the Lambda Cloud API reference. No call was made to Lambda to produce them. The
provenance column says so for each file, and says what a live run replaces it with. A file
whose provenance cell names a date and region is a capture; until a person makes one, no
cell does.

Two things are not in the files. The HTTP status of an answer is set by the test that serves
it (the error tables in `tests/fixtures_parse.rs` list the status each error is served
under); a live capture should note the status it came with in the pull request. The same
goes for response headers: the 429 test adds `Retry-After` itself.

## Placeholders

The region names `region-a` and `region-b` (descriptions `Place A` and `Place B`) are the
placeholder regions of this crate's tests, which also use them outside this directory.
`instance_types_documented.json` uses `us-east-1` and `us-west-1` with their published
descriptions, because it stands for the catalog as Lambda documents it. A capture carries
the region codes the account saw.

Every instance id is 32 lower-case hex digits. Every address is in 192.0.2.0/24,
198.51.100.0/24 or 203.0.113.0/24. Error `request_id` values are `req-` and a short name.
The page token is `EXAMPLE-PAGE-TOKEN-1`. Prices are examples, in whole US cents per hour,
and are not Lambda's prices on any day. `tests/fixtures_parse.rs` checks that no file holds
an address outside those ranges, a twelve-digit number other than `123456789012`, or an AWS
access key id other than `AKIAIOSFODNN7EXAMPLE` (the two exemptions it lists, each with a
reason, are `0.0.0.0/0` as a firewall source and the counter `000000000001` that ends an
instance name).

## Files

All JSON files are "provenance: documented response shape, not yet captured from a live
account". The call is `METHOD path`, under the `/api/v1` prefix of the API address.

| File | Call that produces it | Case | Provenance | Region |
|---|---|---|---|---|
| `instance_types.json` | `GET /instance-types` | Four types: one with capacity in two regions, one with none, an arm64 one, one with no description, no regions key and a control character in the architecture; carries an unknown field | documented response shape, not yet captured from a live account | region-a, region-b |
| `instance_types_documented.json` | `GET /instance-types` | The catalog as documented: four types with real instance-type names, one with no region having capacity, one not in the crate's catalog table | documented response shape, not yet captured from a live account | us-east-1, us-west-1 |
| `instances.json` | `GET /instances` | Seven instances, one per status the client maps (active, booting, terminated, terminating, preempted, unhealthy) and one it does not know; null tags, null and unreadable `first_healthy`, an unknown field; `page_token` null | documented response shape, not yet captured from a live account | region-a, region-b |
| `instances_empty.json` | `GET /instances` | No instances | documented response shape, not yet captured from a live account | none |
| `instances_page_token.json` | `GET /instances` | One instance and a non-empty `page_token`; the client answers `ApiError::Incomplete`. The API documents no paging, so this is the multi-page case as the client meets it | documented response shape, not yet captured from a live account | region-a |
| `instances_by_owner.json` | `GET /instances` | Seven instances for the ownership marks of scheduler `sched-one`: four that are its own (active, booting, terminated, preempted), one renamed by a person (some marks), one of scheduler `sched-two`, one unmarked | documented response shape, not yet captured from a live account | region-a |
| `instance.json` | `GET /instances/{id}` | One active instance with one tag | documented response shape, not yet captured from a live account | region-a |
| `launch.json` | `POST /instance-operations/launch` | Success with one instance id | documented response shape, not yet captured from a live account | none |
| `terminate.json` | `POST /instance-operations/terminate` | Success listing the terminated instance in full, with a field of a newer API | documented response shape, not yet captured from a live account | region-a |
| `terminate_none.json` | `POST /instance-operations/terminate` | Success listing nothing | documented response shape, not yet captured from a live account | none |
| `firewall_rulesets.json` | `GET /firewall-rulesets` | Two rulesets: one with no rule, one with an ssh rule open to `0.0.0.0/0` | documented response shape, not yet captured from a live account | region-a |
| `firewall_ruleset.json` | `POST /firewall-rulesets` | A ruleset just created, with no rule | documented response shape, not yet captured from a live account | region-a |
| `ssh_keys.json` | `GET /ssh-keys` | Two keys, public halves only | documented response shape, not yet captured from a live account | none |
| `ssh_key_generated.json` | `POST /ssh-keys` (name only) | A key the API generated, answered with both halves; the private half is a canary string, not a key | documented response shape, not yet captured from a live account | none |
| `error_invalid_api_key.json` | any call | `global/invalid-api-key`, served as 401 | documented response shape, not yet captured from a live account | none |
| `error_account_inactive.json` | any call | `global/account-inactive`, served as 403 | documented response shape, not yet captured from a live account | none |
| `error_forbidden.json` | any call | `global/forbidden`, served as 403 | documented response shape, not yet captured from a live account | none |
| `error_invalid_address.json` | any call | `global/invalid-address`, served as 400 | documented response shape, not yet captured from a live account | none |
| `error_object_does_not_exist.json` | `GET /instances/{id}`, terminate | `global/object-does-not-exist`, served as 404 | documented response shape, not yet captured from a live account | none |
| `error_not_found.json` | any call | `global/not-found`, served as 404 | documented response shape, not yet captured from a live account | none |
| `error_invalid_parameters.json` | `POST /instance-operations/launch` | `global/invalid-parameters`, served as 400 | documented response shape, not yet captured from a live account | none |
| `error_file_system_in_wrong_region.json` | `POST /instance-operations/launch` | `instance-operations/launch/file-system-in-wrong-region`, served as 400 | documented response shape, not yet captured from a live account | none |
| `error_insufficient_capacity.json` | `POST /instance-operations/launch` | `instance-operations/launch/insufficient-capacity`, served as 400 (and as 503 to show a server error is no refusal) | documented response shape, not yet captured from a live account | none |
| `error_quota_exceeded.json` | `POST /instance-operations/launch` | `global/quota-exceeded`, served as 400 | documented response shape, not yet captured from a live account | none |
| `error_rate_limited.json` | any call | `global/rate-limited`, served as 429, with and without a `Retry-After` header | documented response shape, not yet captured from a live account | none |
| `error_internal.json` | any call | `global/internal-error`, served as 500 | documented response shape, not yet captured from a live account | none |
| `error_provider_unavailable.json` | any call | `provider/internal-unavailable`, served as 502 | documented response shape, not yet captured from a live account | none |
| `host_prep_default.txt` | none: not an API answer | The boot steps the provider gives a roomy host with the default scratch size, one argument list per line; the golden of `the_default_steps_match_their_golden_file` | written by hand from the provider's own `host_prep` output | none |
| `host_prep_small_disk.txt` | none: not an API answer | The two steps the provider gives a 240 GiB disk (log the shortage, then fail); the golden of `a_disk_that_cannot_hold_the_image_and_the_reserve_gets_a_logged_failure` | written by hand from the provider's own `host_prep` output | none |

The error files carry the words `CANARY-MESSAGE-TEXT` and `CANARY-SUGGESTION-TEXT` in
`message` and `suggestion`. The tests assert that no error the client returns repeats them.
A capture keeps the `code` and `request_id` fields as the API sent them and replaces the
text of `message` and `suggestion` with the same canary words.

## Sanitising a capture

A person replaces a file with a capture only after this pass:

- Instance ids, ruleset ids and key ids: keep the shape (32 lower-case hex digits for an
  instance) and replace the digits with a made-up value; keep two rows that carried the same
  id carrying the same new id.
- Public and private addresses and the `hostname`: addresses become an unused address from
  192.0.2.0/24, 198.51.100.0/24 or 203.0.113.0/24; the hostname becomes `host-<n>`.
- Instance, key and ruleset names the account chose: replace with a neutral name. Names the
  provider derives (`js-<hash>-<hash>`) stay as the scheduler wrote them, because
  `instances_by_owner.json` is checked against `HostName::derive` for scheduler `sched-one`.
- Tag values that name people, teams or projects: replace with `fixture`.
- `public_key`: replace with `ssh-ed25519 AAAAC3Nzc <name>`. `private_key`: delete the
  real key and put back the canary string of the current file. A real private key is never
  committed, not even sanitised.
- `request_id`: replace with `req-` and a short name for the case.
- Error `message` and `suggestion`: replace with the canary words.
- Region codes stay as the API sent them.
- The API key never appears in an answer; if it does, stop and tell the owner.

## After a live run

The person who ran the live suite (`docs/runbooks/live-conformance.md`) re-captures any
response that differs from the committed fixture, sanitises it by the rule above, replaces
the file, changes the provenance cell of that row to the capture date and region, and puts
the diff in the pull request. The Lambda suite has no capture flag: take the same call with
any HTTP client and the key file, and keep the status and headers in the pull request text.
