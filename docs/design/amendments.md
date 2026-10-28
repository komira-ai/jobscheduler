# What each amendment deleted and added (history of design.md section 1.1)

Moved from design.md section 1.1 to keep that file under its size limit; the latest amendment stays in 1.1. Section numbers refer to
design.md.

## The first amendment

Deleted: the delegated provider flavour and its former section 5.5 (`Flavour` leaves `js-provider`); the delegated-only unknown rules
(the delegated outcome-table column, its start and stop outcome rules, "silence is Unknown"), so the three-answer rule covers two places,
a lease on an agent and a machine at a capacity provider (5.6); non-goal N2 as written, the quota-ceiling cap of the ladder (7.4), the "no
standing cloud credential" rule of 11.1, and D1 as an open question; the kbf history (former section 4), moved unchanged to
`kbf-today.md`, with 5.3, 7, 11 and steps 0 to 22 moved to companions.

Added: the capacity-provider interface and the `static` and `aws` kinds (5.5); standing cloud access for the aws provider, the role it
assumes (K7, 12.4); `js-agent` as the one runner, with self-registration, per-pool join credentials and admission (5.4, 5.8, D18), and
`kbf-executor` beside it (9.2); two-level leasing and the `acquiring` state (9.1); config-driven providers and pools (5.7); pool scaling
up to `max_machines` (5.8); machine reuse, `max_machines` caps with a per-tick `max_step` and a consumer's own cap, and
the `contained`/`host` isolation with post-job checks and taint (5.10 to 5.12, [machines.md](machines.md)); the mapping of
the merged crates (5.9); the fake aws and the fake agent's taint paths (13.1); steps 27 to 35 (with 30b and 31b), decisions D14 to D25
and risks K8 to K18. (That amendment named the pool keys `min_machines` and `max_machines`; the amendment after the refinement below keeps only `max_machines`.)

## The capabilities amendment

Added: typed capabilities, a constraint language and refusal codes (5.13, [capabilities.md](capabilities.md)); a VM backend in
`js-agent` with a fresh VM per run, the guest channel and VM integrity (5.14); shared hosts and a whole-node class (5.15,
[vm-backend.md](vm-backend.md); the class itself is now 5.17 and [whole-node.md](whole-node.md)); the terms of section 3; steps 36 to 47;
decisions D26 to D31 (D26 open); risks K19 to K27. Text moved to companions to keep design.md short: 5.6, 12.1 to 12.3 and steps 23 to 35.

## The refinement that followed

Added: a fresh VM per run, the warm pool left as future work in [vm-backend.md](vm-backend.md); GPUs only on the GPU pool (D32); one
service in Kubernetes: objects, state store, high availability, upgrades and the move from the single host (5.16,
[deployment.md](deployment.md)); step 45 (now several runs per lease) and steps 48 to 53; D32 to D34; K28 to K31.

## The amendment after that (the owner's review of the design)

Text in [providers.md](providers.md). The `static` pool, whose hosts the operator lists, is **for testing only**, and the GPU machines move
to registering through `js-agent` (5.5, 5.9); a `default` provider (the `DefaultProvider`: hosts that register themselves, never creating
or destroying compute), with admission against registered hosts that refuses a request no host could ever serve with `no-capable-host` and
queues one a busy host could serve (D35, settled); elastic providers whose launched machine runs `js-agent` and registers itself, the
provider's job ending at boot plus registration; a per-pool **provider hierarchy**, an ordered list that falls through only when an
earlier entry cannot satisfy now (5.8); steps 30c, 31c and 54; D35 (settled by the owner; reading (a)) to D41; K32 to K34. Deleted:
`min_machines` everywhere (the key, its validation, the scaling rule that kept a floor, the floor choice of D21 and D23(c), the mitigation
in K11): pools specify `max_machines` only, and nothing is kept warm. The `register = true` provider flag becomes the `default` kind.

## The rollout amendment

D35 settled by the owner (reading (a); the other reading is recorded as considered and rejected); section 17 and
[rollout.md](rollout.md), rolling out the server, the agents and kbf as two services without generalising kbf's update coordinator; D42
to D45; K35 and K36; step 55.

## The whole-node amendment

Text in [whole-node.md](whole-node.md). Added: the **whole-node lease class** (5.17, D46 to D55): exclusive, quiet before the token, reserved
by a drain of one host, capped per pool; the merged `gpu-host` and kbf's `whole_machine` are this class; host jobs only in it; steps 46 and
54 reworded, steps 56 to 58; K37 to K41. Deleted: `max_draining_fraction` (now `max_draining`) and the drain text of 5.15, now in
whole-node.md.
