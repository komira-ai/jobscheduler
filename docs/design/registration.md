# Registration

Moved from design.md section 5.8. Section numbers refer to design.md.

**Registration.** A machine may join a pool by saying hello, so that nothing needs listing in advance. Proof of admission travels with the
hello in message metadata, never in a log line.

- *Identity.* The hello carries a host id (chosen by the operator or derived from the machine), the pool name, labels, the shape it
  offers, and `agent_version`. The control stream is TLS and the agent pins the scheduler's identity before it sends proof. The pool and
  the labels come from configuration; the capacity booked is what the host reports, less or more than the pool's shape. Hello
  labels are informational, and any `jobscheduler.dev/` key from an agent is refused.
- *Admission.* The pool exists; the host id is not in the tainted set (5.12), checked before the credential; the host id is not held by a live stream (a second stream for the same id replaces the first only after
  the first has been silent past the grace of 4.1); the reported shape meets the pool's capabilities and GPU class (`Shape::satisfies` against what the host would be booked at, so the
  CPU, memory, disk and GPUs it reports are never compared with the pool's shape; a refusal is `shape-not-accepted`, `below-floor`); the pool holds
  fewer than `max_machines` hosts, not counting an in-flight instance this hello completes (a refusal is `pool-full`); and the request
  carries a verified host certificate (below). An elastic provider's instance also says its signed identity in the hello, matching a pending acquire
  (below); after admission every host is an ordinary host of the pool. A host id is bound at first admission to the credential (the host certificate) and instance id it first presented; a
  later hello with another binding is refused. A refusal is a `Welcome` with a constant code and no host is created.
- *Host certificate (G3).* A host is known by its host certificate alone: the agent presents it in the TLS handshake, the server's
  `--agent-client-ca` names the CA that signs it, and the host id is the certificate's one DNS name. A request without a verified host
  certificate is refused `401` with the body `{"error":"host-certificate-required"}` by the one middleware of the agent API
  (`crates/js-lease-server/src/agent_api.rs`, `authenticate`); a bearer is not read there. The earlier join credentials are gone: the pool's
  `join_token_file`, the one-time join token with its mint and revoke routes (`POST` and `DELETE /v1/agent/join-tokens`), the host credential a
  welcome carried (`Welcome.host_credential`, `Hello.accepts_host_credential`, both names retired and not to be reused) and the agent's
  `--token-file`, `--instance-file` and credential flags. The server's `--agent-token-file`, a pool's `join_token_file` and
  `--require-host-credential`, and the agent's flags, are accepted and ignored with one warning at start, because deployed units and manifests still pass them. A host id in the provider's
  `deny_hosts_file`, or a removed host, closes any live stream of the host at the next tick: its lease token is revoked, it is quarantined, and its leases are
  given up under 5.6. Cordon and drain is for orderly removal, not revocation.
- *Launched instances (elastic providers).* The machine's own `js-agent` registers it, so the provider's job ends at boot plus
  registration. The launch template holds no secret (K6) and requires IMDSv2 with hop limit 1. For aws the host presents the signed instance
  identity document in place of the token; the provider's `verify_instance` checks signature, account, region and instance id, matches it
  to an acquire record of this pool still `registering`, and accepts it once. Another elastic kind supplies its own proof of the same
  shape, or it cannot be elastic. A second hello for that instance needs the live-stream rule.
- *No token for a launched machine (G3).* A provider that cannot sign an instance identity used to deliver a one-time join token in the
  launch payload. That token and the scaler's mint and revoke of it are deleted, and nothing replaces them until G9, when a launched machine joins
  with a certificate. The scaler is off on the deployment meanwhile.
- *Lifecycle.* A host that stops heartbeating is Unknown, its leases stay and the agent self-fences. Past the grace its self-fence
  leases are given up, since the agent has fenced itself (5.4); a run-on lease is given up only when the host is proven stopped
  (operator removal or a provider Absent), and stays booked with an alert until then. The host stays listed as lost for a retention
  window. A restarted agent registers again with the same id and re-announces its leases; the announcement is reconciled against the
  table, and a lease id not committed to this host id is answered `Cancel` and never adopted.
- *Configuration.* With `--config` the registration settings are keys of the file and no flag may be given beside it. The rate limit on
  hellos is `server.agent_hello_burst` (60) and `server.agent_hello_interval` (`1s`): one host certificate may say a burst of hellos and then
  one more per interval, counted per credential, refusals included. `server.require_host_credential` is accepted and ignored. The pool that registers hosts is a `[[pool]]` table (its `join_token_file`, if given, is ignored), and the three server keys are refused when no pool registers agents. They have the flags'
  names and defaults (`--agent-hello-burst`, `--agent-hello-interval`, `--require-host-credential`; `--placement` and `server.placement` are deprecated and do nothing: `js-sched` places every job, `js-sched` is accepted with a warning and `registry` is refused at start), and the flags work as before when
  `--config` is absent.
- *Source.* The `DefaultProvider` (providers.md) was served by a `RegisteredAgentSource` of `CapacitySource` (5.9), which answered from admitted agents. `CapacitySource` and the lease
  file are deleted; the server lists the hosts of the registry (the static pool file was later removed).
