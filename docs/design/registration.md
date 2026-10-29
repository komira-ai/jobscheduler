# Registration

Moved from design.md section 5.8. Section numbers refer to design.md.

**Registration.** A machine may join a pool by saying hello, so that nothing needs listing in advance. Proof of admission travels with the
hello in message metadata, never in a log line.

- *Identity.* The hello carries a host id (chosen by the operator or derived from the machine), the pool name, labels, the shape it
  offers, and `agent_version`. The control stream is TLS and the agent pins the scheduler's identity before it sends proof. The pool, the
  labels and the capacity booked come from configuration; hello labels are informational, and any `jobscheduler.dev/` key from an agent
  is refused.
- *Admission.* The pool exists; the host id is not in the tainted set (5.12), checked before the credential; the host id is not held by a live stream (a second stream for the same id replaces the first only after
  the first has been silent past the grace of 4.1); the reported shape satisfies the pool's shape (`Shape::satisfies`); the pool holds
  fewer than `max_machines` hosts, not counting an in-flight instance this hello completes (a refusal is `pool-full`); and either (a) the pool
  has a `join_token_file`, the pool's hierarchy includes the `default` provider, and the hello carries the pool's join credential, or (b) an
  elastic provider of the pool launched the instance and the hello carries its signed identity, matching a pending acquire (below).
  The first path admits a `default` host, the second an elastic one; after admission both are ordinary hosts of the pool. A host id is bound at first admission to the credential or instance id it first presented; a
  later hello with another binding is refused. A refusal is a `Welcome` with a constant code and no host is created.
- *Join credential.* One `join_token_file` per registering pool (D18), possibly holding several tokens so that rotation overlaps,
  compared in constant time and re-read every tick. A token admits to one pool. A host id in the provider's `deny_hosts_file`, or a
  removed token, closes any live stream of the host at the next tick: its lease token is revoked, it is quarantined, and its leases are
  given up under 5.6. Cordon and drain is for orderly removal, not revocation. A client certificate per pool is the alternative (D18).
- *Launched instances (elastic providers).* The machine's own `js-agent` registers it, so the provider's job ends at boot plus
  registration. The launch template holds no secret (K6) and requires IMDSv2 with hop limit 1. For aws the host presents the signed instance
  identity document in place of the token; the provider's `verify_instance` checks signature, account, region and instance id, matches it
  to an acquire record of this pool still `registering`, and accepts it once. Another elastic kind supplies its own proof of the same
  shape, or it cannot be elastic. A second hello for that instance needs the live-stream rule.
- *One-time join token (an elastic kind with no signed identity).* A provider that cannot sign an instance identity (a GPU cloud that
  gives the instance nothing signed) proves it another way: the operator's tooling mints a token for the instance it launched
  (`POST /v1/agent/join-tokens`, behind the client token, with the provider and instance id and a short lifetime, 30 minutes unless asked,
  at most a day) and delivers it to that instance after boot, never in user data, an argument or a commit. The server keeps only its digest, in
  `<state file>.joins`, so a restart keeps an unused token valid and a used one used. The first hello that presents the token, names that
  instance in `instance`, and arrives before the expiry registers the host and consumes the token: it is then tied to that host id. It
  carries that host's later hellos and heartbeats and nothing else; a hello for another host id is refused `join-token-used`, an unused token
  past its time `join-token-expired`, a hello that names another instance or none `join-token-instance`, and a heartbeat for a host the
  token did not register `join-token-not-admitted`. A second token for an instance replaces its unused one. The agent reads the token with
  `--token-file` (owner-only, as always) and names its instance with `--instance-file`. A token is also the host's credential, so the
  agent keeps the file; the host is revoked by `deny_hosts_file` or the operator's removal, as any host is. A used token is a credential
  only while its host keeps using it: its first use sets a bound 24 hours out (the registry's default retention of a lost host), every
  hello and heartbeat it serves moves the bound to 24 hours from then (written to the file when it gains an hour), and past the bound hello
  and heartbeat are refused `join-token-expired` and the next mint drops the entry. A host silent for longer than the bound needs a new
  token. The server is not told when the registry forgets a host, so the bound is what ends the token of a host that is gone, except for
  an instance the scheduler terminates itself: its terminate path revokes the instance's tokens with `DELETE /v1/agent/join-tokens`
  (client token only, body `{provider, instance_id}`), which drops every token bound to that instance, used or not, in one write of the
  file. A revoked token is unknown, so its hello and heartbeat get the 401 of a token that never existed. The call answers 204 also for an
  instance with no token (nothing is written then), so a retried terminate needs no care; an unused token past its expiry stays until the next
  mint drops it, so revoking one is a write, and also a 204. The registry's binding of the host id to the revoked token's digest persists, so
  a hello for that host id with a new token is refused `host-bound`: a fresh launch uses a fresh instance and host id; a file that cannot be written is a 503 `state-write-failed` and the tokens
  stay. This closes the window of a used token for the instances the scheduler terminates; an instance that dies without that call falls
  back to the bound. The 201 of the mint carries `Cache-Control: no-store`. A hello the
  registry refuses (the host id is bound to another credential, the pool is full, the shape is not accepted) gives the use back: the token
  stays unused for its instance. If the file cannot be written for that, the token stays tied to the host that named it and the operator
  mints another. The instance a hello names is the hello's own claim: the binding guards against mix-ups (a token delivered to the wrong
  machine, a host id reused by another instance), not against the holder of a token, who can name the instance too; and the client token can
  mint a token for any instance id. One server owns the state file at a time (the lock beside the lease file refuses a second one), and
  the file of tokens is written whole through a new temporary file, so a file a crash left under that name never decides its mode.
- *Lifecycle.* A host that stops heartbeating is Unknown, its leases stay and the agent self-fences. Past the grace its self-fence
  leases are given up, since the agent has fenced itself (5.4); a run-on lease is given up only when the host is proven stopped
  (operator removal or a provider Absent), and stays booked with an alert until then. The host stays listed as lost for a retention
  window. A restarted agent registers again with the same id and re-announces its leases; the announcement is reconciled against the
  table, and a lease id not committed to this host id is answered `Cancel` and never adopted.
- *Source.* The `DefaultProvider` (providers.md) is served by a `RegisteredAgentSource` of `CapacitySource` (5.9), which answers from admitted agents and ignores the bookings it is given: the lease
  file records leases and agents re-announce them.
