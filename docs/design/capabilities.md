# Capabilities as data (5.13)

Companion to [docs/design.md](../design.md), sections 3, 5.2 and 6.2. Section numbers refer to design.md; 5.13 is defined here.

A **capability** is a typed fact an agent advertises about its host (or about a base image the host holds). A consumer states what a
request needs as **constraints** over those facts. The scheduler stores the facts, matches the constraints exactly as written below, and
knows no consumer's vocabulary: a consumer such as kbf translates its own platform needs into constraints (below), and that table lives in
the consumer's configuration, as the class map of 6.2 does. Capabilities say what a host *can* do; resources say what is *free now*
(section 3): `vms >= 1` asks whether the host can run a VM at all, and a booking of the `vms` dimension decides whether it has a slot left.

## Model

Five kinds. A key has one kind, fixed by the schema (below).

| Kind | Value | Example |
|---|---|---|
| `text` | a token of the key grammar the merged wire checks (letters, digits, `.`, `_`, `-`; at most 128 bytes) | `os = macos` |
| `version` | a version (grammar below) | `os_version = 15.1` |
| `set_text` | a set of tokens | `base_images = {macos15-xcode16-a, macos14-xcode15-a}` |
| `set_version` | a set of versions | `xcode = {15.4, 16.0, 16.2}` |
| `count` | a non-negative integer, at most 2^32 | `vms = 2` |

**Version grammar.** One to four components separated by `.`; each is `0` or a decimal number without a leading zero and with at most nine
digits. Nothing else is a version: no `v` prefix, no `x` wildcard, no suffix. A pre-release build is not representable; an operator who
wants to offer one advertises it under another key (`xcode_beta`), so a plain `xcode` constraint never matches it.
**Order.** Pad the shorter version with zeros, then compare component by component as numbers. So `16`, `16.0` and `16.0.0` are equal,
`16.0.1 < 16.1 < 16.10 < 17`, and the order is total on canonical values. A set of versions holds each value once under this order; a
sender that lists `16` and `16.0` is read as listing it once.

**Core keys.** The schema ships these; a consumer or operator adds namespaced keys (`vendor.name`) in configuration, one kind each.

| Key | Kind | Reported as |
|---|---|---|
| `arch` | `text` | the CPU architecture as the merged agent reports it today (`aarch64`, `x86_64`; the merged `Platform`). A consumer that says `arm64` translates to `aarch64`; the scheduler does not alias |
| `os` | `text` | `macos`, `linux` (the merged `Platform`) |
| `os_version` | `version` | the product version where it parses to the grammar (macOS: the product version); absent otherwise |
| `xcode` | `set_version` | the versions of the Xcode applications installed |
| `sim.ios`, `sim.tvos`, `sim.watchos`, `sim.visionos` | `set_version` | the simulator runtime versions installed, one key per platform |
| `base_images` | `set_text` | the names of the primed VM base images the host holds (5.14) |
| `warm_images` | `set_text` | the subset of `base_images` for which a warm VM is booted and unassigned right now (5.14); a rank hint, never a constraint |
| `vms` | `count` | the most VMs the host runs at once, warm ones included |

An agent reports a key only when it can: a probe that fails reports the key absent and a warning (as the merged GPU probe does). An
operator may add or pin a fact the agent cannot probe with a repeatable flag `--cap key=value` (R4; the value is parsed by the key's
kind; a set is given by repeating the flag), which overrides the probe for that key.

**Image profiles.** A VM job runs inside a base image, so what it needs (the Xcode, the runtime) is a property of the *image*, not of the
host. Each primed image carries a profile in its manifest: the same typed map, over the same schema (`os`, `os_version`, `arch`, `xcode`,
`sim.*`). The agent reads the manifests and advertises `image_profiles: name -> map`. The names are exactly `base_images`. An image with no
manifest facts has an empty profile and can be matched by name only.

## Schema

The schema is configuration of the scheduler (5.7): key, kind, and an optional note. The keys above are built in. Rules (D10 holds):
a **request** key that the schema does not know is refused; a **reported** key it does not know, or whose reported kind differs from the
schema's, is skipped and logged, and the hello is accepted (R3). The pool's `[capabilities]` stay what a hello must satisfy to be
admitted (5.8); with this change the same constraint language may be used there.

## Wire: additive over the merged `js-agent-wire`

Today `Hello.capabilities` is `map<string,string>` (at most 64 entries, key 64 bytes, value 128), and `Heartbeat` carries none. Protocol
major 1 stays (the rules of `protocol.rs`: fields are only added; a receiver ignores what it does not know). Nothing existing changes.

- `Hello.facts` (optional): `{ rev, host, images }`. `rev` is a number the agent increments on every change, starting at 1 in a session.
  `host` is `map<key, Value>` and `images` is `map<image name, map<key, Value>>`. `Value` is a tagged object: `{"kind":"text","value":".."}`,
  `{"kind":"version","value":".."}`, `{"kind":"set_text","items":[..]}`, `{"kind":"set_version","items":[..]}`, `{"kind":"count","value":n}`.
  New limits beside the existing ones: 64 host keys, 16 images of 32 keys each, 256 items per set, 64 bytes per item, 32 bytes per version.
  A value over a limit is refused with `invalid-hello` as every other limit is.
- `Heartbeat.facts_rev` (optional): the revision the agent now holds. `Heartbeat.facts` (optional): the full snapshot, sent whenever the
  agent's revision is newer than the last `facts_ack` it received, so a lost update is repeated with the next heartbeat (the pattern of
  `taint` and `taint_ack`). A full replacement, never a delta: no gap can leave the two sides apart.
- `HeartbeatReply.facts_ack` (optional): the newest revision the server stored. A snapshot whose revision is not above the stored one is
  acknowledged and ignored. The revision orders updates within one session; a new `Hello` replaces everything and starts again at 1.
- **Old peers.** An old server ignores `facts`. To stay matchable by it, a new agent also mirrors every `text` and `version` host fact into
  the legacy `capabilities` map (within its 64 entries). A key present in both forms with different text is refused: `invalid-hello`. A new
  server reading a hello without `facts` takes each legacy entry whose schema kind is `text` or `version` (a value that does not parse is
  skipped) and nothing else, so old agents keep working and can satisfy scalar constraints only.
- `JobSpec` and the lease request gain fields in 5.14 and below; every field is optional and an absent field means today's behaviour.

## Storage and refresh

The server keeps, per host, the newest accepted `facts` in memory beside the registry's shape. It is soft state: the agent repeats it at
every hello, so the lease file does not hold it. A lease records, additively (R3), the `rev` it was matched against and the image chosen.
A change reaches the scheduler at the next heartbeat; a change never touches a lease already granted (a runtime removed under a running
job is the job's problem), and it applies to every grant after the revision is stored. The agent re-probes at start, after a job (the
post-job check hook of 5.12 already runs then), after any VM store change, and every `--facts-interval`; it bumps the revision only when
a value changed. `warm_images` changes with the warm pool and is the one fact that changes often.

## Constraint language

A request carries `needs`: a list of at most 32 constraints, all of which must hold (a conjunction; there is no `or` and no `not`; an
alternative is a second request or the consumer's own retry, and `in` covers alternatives of one key). A constraint is
`{ "key", "op", "value", "scope" }`; `scope` is `host` (the default when absent) or `guest` (the facts of an image profile).

| Kind of the key | Ops | Value | True when |
|---|---|---|---|
| `text` | `eq` | token | host value equals it, byte for byte (case matters) |
| `text` | `in` | 1 to 32 tokens | host value equals one of them |
| `version` | `eq` `gt` `ge` `lt` `le` | version | host value compares as named, under the order above |
| `version` | `series` | a version prefix of 1 to 3 components | the host value's first n components equal the prefix's (missing components are zero): `series 16` matches `16`, `16.4.1`, not `17`; `series 16.2` matches `16.2.9`, not `16.3` |
| `version` | `range` | `{lo, hi}`, either may be absent, not both | `lo <= value < hi` |
| `version` | `in` | 1 to 32 versions | equals one of them under the order |
| `count` | `eq` `gt` `ge` `lt` `le` | integer | compares as named |
| `set_text` | `has` | token | the set contains it |
| `set_text` | `has_all` `has_any` | 1 to 32 tokens | contains all, or at least one |
| `set_version` | `has` `has_gt` `has_ge` `has_lt` `has_le` `has_series` `has_range` | as the scalar op | **some member** satisfies the scalar op |
| `set_version` | `has_all` `has_any` | 1 to 32 versions | contains all (equal under the order), or at least one |

**Semantics.**

1. **Absent is unmet.** A host with no value for the key fails every op on it, `ge 0` on a count included. There is no default and no zero.
2. **Constraints are independent.** `xcode has_series 16` and `xcode has_ge 16.2` may be met by different members. To require one member to
   satisfy both, use one op (`has_range`).
3. **Guest scope.** A host meets the `guest` constraints when at least one image in its `base_images` meets *all* of them against that
   image's profile (one image, not one per constraint). `host` constraints are evaluated on the host's own facts. The match returns the
   eligible images; the grant names one: a warm image first, then the smallest name. A request with a `guest` constraint is a VM request;
   its class must have a `vm` runtime (6.2), else `constraint-bad-scope`.
4. **Evaluated when the grant is made**, under the table lock, on the newest stored facts, and only on hosts the placement pipeline
   already finds eligible (serving, not tainted, not releasing; 5.10). A constraint no host meets leaves the request queued and counts
   toward the unservable wait of 8.2. On an elastic pool a request is demand for a new machine only if the pool's *declared* shape
   capabilities (configuration) meet its `host` constraints and the pool declares a base image that can meet its `guest` ones.
5. **Unknown means refuse.** A constraint with an op this server does not know is refused, never skipped: skipping a need could place
   work on a host that cannot run it. This is the second deliberate exception to "skip what you do not know" (after the phase of 5.3).

**Refusal codes** (plain constants, as the merged wire's; no central table, in line with 12.1). A request with a malformed constraint is
refused at submit, before it is queued, naming the index of the constraint.

| Code | When |
|---|---|
| `constraint-unknown-key` | the key is not in the schema |
| `constraint-bad-op` | the op is not defined for the key's kind, or not known to this server |
| `constraint-bad-value` | the value's type does not fit the op (a non-integer for a count, a list where one token is wanted, an empty or over-long list, a token outside the grammar) |
| `constraint-bad-version` | a version outside the grammar, or a `series` prefix of more than 3 components |
| `constraint-empty-range` | `lo >= hi`, or neither bound |
| `constraint-bad-scope` | a scope other than `host` or `guest`, or `guest` on a class without a VM runtime |
| `constraint-too-many` | more than 32 constraints |

A well-formed constraint that no host can meet is not an error; the request waits (8.2). Selecting pools by class and labels is unchanged
(6.2); a request that matches no pool is refused `no-matching-pool`.

## What a consumer does: kbf's translation

A consumer keeps a table from its own platform properties to constraint templates, in its configuration. The scheduler never sees the
property names. For kbf the table maps an action's platform properties (the names below are illustrative; the property names are kbf's)
and its `sandbox` choice. An unknown property follows kbf's own policy for an unknown request key (step 15); it is never dropped quietly.

Worked example: an iOS test that needs Xcode 16.x and an iOS 18 simulator runtime on arm64, on macOS 15 or newer, run in a VM.

| kbf property | Constraint |
|---|---|
| `OSFamily=darwin` | `{scope:host, key:os, op:eq, value:"macos"}` (the guest is macOS, so the host is too) |
| `Arch=arm64` | `{scope:host, key:arch, op:eq, value:"aarch64"}`, and the same on the guest |
| `sandbox=vm` | `{scope:host, key:vms, op:ge, value:1}` |
| `min-os=15.0` | `{scope:guest, key:os_version, op:ge, value:"15.0"}` |
| `xcode=16.x` | `{scope:guest, key:xcode, op:has_series, value:"16"}` |
| `sim-runtime=ios-18` | `{scope:guest, key:sim.ios, op:has_series, value:"18"}` |

A host with `os=macos, arch=aarch64, vms=2, base_images={mac15-x16-a, mac14-x15-a}` where `mac15-x16-a` has
`os_version=15.2, xcode={16.0,16.2}, sim.ios={18.0,18.2}, arch=aarch64` meets every constraint; the eligible image set is `{mac15-x16-a}`,
the grant names it, and the `Start` of the VM job names it (5.14). A host holding only `mac14-x15-a` (`os_version=14.6, xcode={15.4}`) is
skipped: no image meets all four guest constraints, though each constraint alone is met by some image of some host. The same need run
natively (a `host` job, off by default, 5.12) drops `sandbox=vm` and puts the three Xcode and OS constraints in `host` scope.

## Tests (13.2)

A table test per op and kind from the table above, with a case that holds and one that does not, and the ordering properties on generated
versions (`16 == 16.0`; transitivity; `ge` and `lt` partition; `series n` equals `range [n, n+1)` for a one-component prefix; the
boundary `16.9.9` against `17`). Absent is unmet for every op. Each refusal code has a case, and unknown-op is refused, not skipped. Guest
scope: three images, each meeting a different constraint, none meeting all, is no match. Wire: a golden hello with `facts` is skipped by a
decoder of the previous crate version (the cross-version test of 13.6); a legacy-only hello still meets a scalar constraint; a
conflicting legacy entry is `invalid-hello`; a heartbeat whose ack was lost is repeated and a repeat is ignored; a snapshot over each limit
is refused. The kbf translation table is tested in kbf with the example above.
