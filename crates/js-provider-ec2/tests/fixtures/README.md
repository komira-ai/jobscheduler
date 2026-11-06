# Fixtures of js-provider-ec2

The files in `api/` are the bodies of the answers the provider's client reads: EC2 Query
XML, and the JSON of the Pricing API and Service Quotas. `tests/fixtures_parse.rs` serves
each one through a scripted transport to `HttpEc2`, the client the provider runs on, and
asserts what it is read into.

**None of the files in `api/` is a capture.** Each is written from the response shape the
provider's API reference documents (the EC2 API Reference, the Price List API Reference and
the Service Quotas API Reference). No call to the cloud was made to produce them. Their
values are examples: the prices are plausible, not current, and the instance types are real
names with the sizes the reference lists for them. A fixture says "documented response
shape" until a person replaces it with a real capture (see "After a live run").

The placeholders, used the same way in every file:

| What | Placeholder |
|---|---|
| Account id | `123456789012` |
| Access key id | `AKIAIOSFODNN7EXAMPLE` (not used in the answers; the checks allow only it) |
| Instance id | `i-` and 17 lower-case hex digits: `i-0123456789abcdef0`, `i-0fedcba9876543210`, `i-00000000000000001` |
| Image id | `ami-0123456789abcdef0` |
| Reservation, subnet and VPC ids | the same form: `r-0123456789abcdef0`, `subnet-0123456789abcdef0`, `vpc-0123456789abcdef0` |
| IPv4 address | only the documentation ranges `192.0.2.0/24`, `198.51.100.0/24`, `203.0.113.0/24` |
| Request id | `00000000-0000-0000-0000-000000000000`, numbered in the last group |
| Page token | `EXAMPLE-PAGE-TOKEN-1` |
| Region and zones | `us-east-1`; `us-east-1a`, `us-east-1b` |
| Scheduler id and acquire ids in tags | `sched-one`; `acq-one` to `acq-six`; the `Name` tag is the name the provider derives for them |
| IAM user in a message | `arn:aws:iam::123456789012:user/example-user` |

## Files

The column "Case" says what the file holds. The status an error file is served with in the
tests is chosen by the test; the client picks the error class from the code in the body
alone.

| File | Call that produces it | Case | Provenance | Region / zone |
|---|---|---|---|---|
| `api/describe_instance_types_page1.xml` | EC2 `DescribeInstanceTypes`, first page | success; `nextToken` set; m7i.large and a bare-metal arm64 type | documented response shape, not yet captured from a live account | us-east-1 |
| `api/describe_instance_types_page2.xml` | EC2 `DescribeInstanceTypes`, second page | success; no `nextToken`; a GPU type with an instance store and an 8-GPU type | documented response shape, not yet captured from a live account | us-east-1 |
| `api/describe_instance_type_offerings.xml` | EC2 `DescribeInstanceTypeOfferings` (`LocationType=availability-zone`) | success; six type and zone pairs | documented response shape, not yet captured from a live account | us-east-1a, us-east-1b |
| `api/describe_spot_price_history.xml` | EC2 `DescribeSpotPriceHistory` (`Linux/UNIX`) | success; empty `nextToken` element | documented response shape, not yet captured from a live account | us-east-1a, us-east-1b |
| `api/describe_capacity_block_offerings.xml` | EC2 `DescribeCapacityBlockOfferings` (24 hours) | success; two offerings in different zones | documented response shape, not yet captured from a live account | us-east-1a, us-east-1b |
| `api/get_products_page1.json` | Pricing `GetProducts`, first page | success; `NextToken` set; `PriceList` entries are JSON strings | documented response shape, not yet captured from a live account | us-east-1 |
| `api/get_products_page2.json` | Pricing `GetProducts`, second page | success; no `NextToken` | documented response shape, not yet captured from a live account | us-east-1 |
| `api/get_service_quota_standard_on_demand.json` | Service Quotas `GetServiceQuota` (`L-1216C47A`) | success; value 640.0 | documented response shape, not yet captured from a live account | us-east-1 |
| `api/get_service_quota_standard_spot.json` | Service Quotas `GetServiceQuota` (`L-34B43A08`) | success; value 320.0 | documented response shape, not yet captured from a live account | us-east-1 |
| `api/get_service_quota_p_on_demand.json` | Service Quotas `GetServiceQuota` (`L-417A185B`) | success; value 384.0 | documented response shape, not yet captured from a live account | us-east-1 |
| `api/get_service_quota_p_spot.json` | Service Quotas `GetServiceQuota` (`L-7212CCBC`) | success; value 192.0 | documented response shape, not yet captured from a live account | us-east-1 |
| `api/run_instances.xml` | EC2 `RunInstances` | success; one pending instance with the scheduler's tags and a label tag | documented response shape, not yet captured from a live account | us-east-1a |
| `api/describe_instances_by_ids.xml` | EC2 `DescribeInstances` by `InstanceId.N` | success; one running instance | documented response shape, not yet captured from a live account | us-east-1a |
| `api/describe_instances_by_tags_page1.xml` | EC2 `DescribeInstances` by tag filters, first page | success; `nextToken` set; running, pending and shutting-down instances in two reservations | documented response shape, not yet captured from a live account | us-east-1a, us-east-1b |
| `api/describe_instances_by_tags_page2.xml` | EC2 `DescribeInstances` by tag filters, second page | success; stopping, stopped and terminated instances | documented response shape, not yet captured from a live account | us-east-1a |
| `api/describe_instances_empty.xml` | EC2 `DescribeInstances` by tag filters | success; empty `reservationSet` | documented response shape, not yet captured from a live account | us-east-1 |
| `api/terminate_instances.xml` | EC2 `TerminateInstances` | success; running to shutting-down | documented response shape, not yet captured from a live account | us-east-1a |
| `api/error_invalid_instance_id_not_found.xml` | EC2 `DescribeInstances` or `TerminateInstances` by id | error `InvalidInstanceID.NotFound` (not found) | documented response shape, not yet captured from a live account | us-east-1 |
| `api/error_insufficient_instance_capacity.xml` | EC2 `RunInstances` | error `InsufficientInstanceCapacity` (capacity) | documented response shape, not yet captured from a live account | us-east-1b |
| `api/error_spot_max_price_too_low.xml` | EC2 `RunInstances` (spot) | error `SpotMaxPriceTooLow` (capacity) | documented response shape, not yet captured from a live account | us-east-1 |
| `api/error_instance_limit_exceeded.xml` | EC2 `RunInstances` | error `InstanceLimitExceeded` (quota) | documented response shape, not yet captured from a live account | us-east-1 |
| `api/error_vcpu_limit_exceeded.xml` | EC2 `RunInstances` | error `VcpuLimitExceeded` (quota) | documented response shape, not yet captured from a live account | us-east-1 |
| `api/error_max_spot_instance_count_exceeded.xml` | EC2 `RunInstances` (spot) | error `MaxSpotInstanceCountExceeded` (quota) | documented response shape, not yet captured from a live account | us-east-1 |
| `api/error_request_limit_exceeded.xml` | any EC2 call | error `RequestLimitExceeded` (throttled) | documented response shape, not yet captured from a live account | us-east-1 |
| `api/error_auth_failure.xml` | any EC2 call | error `AuthFailure` (auth) | documented response shape, not yet captured from a live account | us-east-1 |
| `api/error_unauthorized_operation.xml` | EC2 `RunInstances` | error `UnauthorizedOperation` (auth) | documented response shape, not yet captured from a live account | us-east-1 |
| `api/error_opt_in_required.xml` | any EC2 call | error `OptInRequired` (auth) | documented response shape, not yet captured from a live account | us-east-1 |
| `api/error_blocked.xml` | any EC2 call | error `Blocked` (auth) | documented response shape, not yet captured from a live account | us-east-1 |
| `api/error_invalid_parameter_value.xml` | EC2 `RunInstances` | error `InvalidParameterValue` (malformed request) | documented response shape, not yet captured from a live account | us-east-1 |
| `api/error_invalid_ami_id_not_found.xml` | EC2 `RunInstances` | error `InvalidAMIID.NotFound` (malformed request) | documented response shape, not yet captured from a live account | us-east-1 |
| `api/error_unsupported.xml` | EC2 `RunInstances` | error `Unsupported` (malformed request) | documented response shape, not yet captured from a live account | us-east-1 |
| `api/error_insufficient_free_addresses_in_subnet.xml` | EC2 `RunInstances` | error `InsufficientFreeAddressesInSubnet` (malformed request) | documented response shape, not yet captured from a live account | us-east-1 |
| `api/error_internal_error.xml` | any EC2 call | error `InternalError` (transient server error) | documented response shape, not yet captured from a live account | us-east-1 |
| `api/error_service_unavailable.xml` | any EC2 call | error `ServiceUnavailable` (transient server error) | documented response shape, not yet captured from a live account | us-east-1 |
| `api/error_unavailable.xml` | any EC2 call | error `Unavailable` (transient server error) | documented response shape, not yet captured from a live account | us-east-1 |
| `api/error_pricing_throttling_exception.json` | Pricing `GetProducts` | JSON error `ThrottlingException` (throttled) | documented response shape, not yet captured from a live account | us-east-1 |
| `api/error_pricing_access_denied_exception.json` | Pricing `GetProducts` | JSON error `AccessDeniedException` (auth) | documented response shape, not yet captured from a live account | us-east-1 |
| `api/error_pricing_internal_error_exception.json` | Pricing `GetProducts` | JSON error `InternalErrorException` (transient server error) | documented response shape, not yet captured from a live account | us-east-1 |
| `api/error_quotas_no_such_resource_exception.json` | Service Quotas `GetServiceQuota` | JSON error `NoSuchResourceException`, namespaced as `com.amazonaws.servicequotas#NoSuchResourceException` (not found) | documented response shape, not yet captured from a live account | us-east-1 |
| `api/error_quotas_too_many_requests_exception.json` | Service Quotas `GetServiceQuota` | JSON error `TooManyRequestsException` (throttled) | documented response shape, not yet captured from a live account | us-east-1 |
| `api/error_quotas_access_denied_exception.json` | Service Quotas `GetServiceQuota` | JSON error `AccessDeniedException` (auth) | documented response shape, not yet captured from a live account | us-east-1 |
| `identity/doc.json` | an instance identity document, as the instance metadata service serves it | made-up document for instance i-0123456789abcdef0, region `region-a`, zone `zone-a1`, account 000000000000 | not from the cloud: written for the tests (see below) | `region-a` / `zone-a1` (made up) |
| `identity/doc.sig` | the base64 RSA signature of `identity/doc.json` | signed with a throwaway test key whose private half was destroyed | not from the cloud: written for the tests (see below) | `region-a` / `zone-a1` (made up) |
| `identity/other.json` | an instance identity document | made-up document for a second instance and image, region `region-a` | not from the cloud: written for the tests (see below) | `region-a` / `zone-a1` (made up) |
| `identity/other.sig` | the signature of `identity/other.json` | same throwaway key | not from the cloud: written for the tests (see below) | `region-a` / `zone-a1` (made up) |
| `identity/region.json` | an instance identity document | made-up document that names the region `region-b` | not from the cloud: written for the tests (see below) | `region-a` / `zone-a1` (made up) |
| `identity/region.sig` | the signature of `identity/region.json` | same throwaway key | not from the cloud: written for the tests (see below) | `region-a` / `zone-a1` (made up) |

The `identity/` files are not answers of the API. `tests/offline/identity.rs` checks the
provider's verification of an instance's signed identity document against them: the
documents are made up (an account of zeros, the region `region-a`) and signed with a
throwaway RSA key whose private half was destroyed, and the test gives the key's modulus.
Their text is signed, so it cannot change; the placeholder check does not read the three
documents, for that reason (the list is `UNCHECKED` in `tests/fixtures_parse.rs`).

## The checks on the fixtures

Three tests in `tests/fixtures_parse.rs` guard the directory:

- `fixtures_are_all_named_by_a_test`: every file is named by a source under `tests/`.
- `fixtures_are_all_listed_in_the_readme`: every file has a row in the table above.
- `fixtures_carry_only_placeholder_values`: no IPv4 address outside the three
  documentation ranges, no 12-digit number other than `123456789012`, and no access key id
  (`AKIA` or `ASIA` and 16 characters) other than `AKIAIOSFODNN7EXAMPLE`.

## Replacing a fixture with a capture

Sanitise a real answer before it is committed, by these rules:

- The account id, in an `ownerId`, an ARN or a message, becomes `123456789012`.
- Every instance id becomes `i-` and 17 hex digits as above, keeping the same real id the
  same placeholder within one file and across the files of one test; image, reservation,
  subnet and VPC ids likewise.
- Every IP address, private or public, becomes an address of a documentation range.
- Request ids become the zero request id; the client token and any encoded authorization
  message become `EXAMPLE-` text.
- The page token becomes `EXAMPLE-PAGE-TOKEN-1` (and `-2` for a third page); the test
  asserts that the second request carries the token of the first answer, so keep it
  consistent.
- Tags keep their keys; the values of the scheduler, acquire and `Name` tags become the
  ones the tests derive.
- IAM user names and ARNs in messages become `example-user`.
- No access key, session token or secret appears in an answer; if one does, stop and do not
  commit the file.

## After a live run

The person who ran the live suite (`docs/runbooks/live-conformance.md`) re-captures any
response that differs from the committed fixture, sanitises it by the rules above, replaces
the file, changes its "Provenance" cell to the capture date and the region, and puts the
diff of the fixture in the pull request. The live suite's `--capture-dir` flag writes the
raw answers it received (see the runbook); the files there are raw, so sanitise them before
copying them in.
