//! The replay loop: arrivals, a queue, placement rounds, truth, kills and metrics.
//!
//! Time is a logical tick of 1 s. Each tick, in this order:
//!
//! 1. leases whose run is over leave their host;
//! 2. residents that turned a minute older are re-charged;
//! 3. on every fifth tick (a round), the rows that have started enter the queue and the
//!    queue is walked in order: a lease goes through the exclusion flag, the predicate and
//!    the ranker and is placed, or is refused and stays. The round stops after 32
//!    refusals; placements neither count toward the 32 nor reset them, so a round looks
//!    at no more than its placements plus 32 leases;
//! 4. every resident burns the core-ms the [`Truth`] gives it for its second;
//! 5. at the end of a window (5 ticks) each host is read, and while the working sets on a
//!    host exceed its memory the largest (ties: the newest) is killed, never requeued;
//! 6. at the end of a minute the host-minutes are settled.
//!
//! A lease placed at a round holds its host for `ceil(wall / 1000)` ticks, its real wall,
//! so a short action waits at most one round to start and then holds what it held. CPU
//! is compressible: a host over its cores slows no lease.
//!
//! The run ends when every row has arrived and the queue and the pool are empty, or
//! `drain_ticks` after the last arrival, whichever is first; what is left is counted.

use std::collections::{BTreeMap, BTreeSet};

use super::fixture::Row;
use super::metrics::{Fnv, Metrics, Recorder, SCAN_REFUSALS, oversize_rows};
use super::model::{Beliefs, HostState, MINUTE_TICKS, Pool, Truth, WINDOW_TICKS};
use super::predicate::{Params, Refusal, admit};
use super::rules::{Choose, Ranker};

/// What a replay is run with.
#[derive(Clone, Debug)]
pub struct Config {
    /// The hosts.
    pub pool: Pool,
    /// The predicate's settings.
    pub params: Params,
    /// Logical minutes before the metric window opens.
    pub warm_up_min: u64,
    /// Ticks the run may go on after the last arrival.
    pub drain_ticks: u64,
    /// Hosts marked: nothing is placed on them and nothing is scored for them.
    pub excluded: Vec<usize>,
}

impl Config {
    /// The registered settings on `pool`: 60 minutes of warm-up and a drain of 6 hours.
    pub fn new(pool: Pool) -> Self {
        Self {
            pool,
            params: Params::default(),
            warm_up_min: 60,
            drain_ticks: 6 * 3_600,
            excluded: Vec::new(),
        }
    }
}

/// One placement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placed {
    /// The lease index.
    pub lease: usize,
    /// The host index.
    pub host: usize,
    /// The tick of the round.
    pub tick: u64,
}

/// What a replay produced.
#[derive(Clone, Debug)]
pub struct Replay {
    /// The measurements.
    pub metrics: Metrics,
    /// Every placement, in order.
    pub placements: Vec<Placed>,
    /// FNV-1a over the `(lease, host)` of every placement, in order.
    pub placement_hash: u64,
    /// FNV-1a over the `(tick, lease)` of every arrival taken into the queue.
    pub arrival_hash: u64,
    /// Every lease killed for memory, with the tick of the window that killed it.
    pub kills: Vec<(usize, u64)>,
    /// The tick the run stopped at.
    pub end_tick: u64,
}

struct Live {
    host: usize,
    start_tick: u64,
    end_tick: u64,
}

/// Replays `rows` (sorted by start; the lease index is the position) on `cfg.pool`.
pub fn replay(
    cfg: &Config,
    rows: &[Row],
    truth: &dyn Truth,
    beliefs: &dyn Beliefs,
    ranker: &mut dyn Ranker,
) -> Replay {
    let pool = cfg.pool;
    let last_arrival_ms = rows.last().map_or(0, |r| r.start_ms);
    let end_limit = last_arrival_ms / 1_000 + cfg.drain_ticks;
    let mut rec = Recorder::new(pool.hosts, pool.cap, cfg.warm_up_min, last_arrival_ms);
    rec.set_oversize(oversize_rows(rows, &pool.cap));
    rec.counters().submitted = rows.len() as u64;

    let mut hosts: Vec<HostState<'_>> = (0..pool.hosts)
        .map(|h| {
            let mut host = HostState::new(pool.cap);
            host.excluded = cfg.excluded.contains(&h);
            host
        })
        .collect();
    let mut queue: Vec<usize> = Vec::new();
    let mut next_arrival = 0usize;
    let mut live: BTreeMap<usize, Live> = BTreeMap::new();
    let mut ends: BTreeSet<(u64, usize)> = BTreeSet::new();
    let mut burnt = vec![0u64; rows.len()];
    let mut window_core_ms = vec![0u64; pool.hosts];
    let mut placements = Vec::new();
    let mut kills = Vec::new();
    let mut placement_hash = Fnv::new();
    let mut arrival_hash = Fnv::new();
    let mut tick = 0u64;

    loop {
        if next_arrival == rows.len() && queue.is_empty() && live.is_empty() {
            break;
        }
        if tick > end_limit {
            break;
        }
        // 1. Runs that are over.
        while let Some(&(end, lease)) = ends.first() {
            if end > tick {
                break;
            }
            ends.pop_first();
            let l = live.remove(&lease).expect("a live lease");
            hosts[l.host].leave(lease);
            let c = rec.counters();
            c.completed += 1;
            let books = rec.books();
            books.completed_core_ms += burnt[lease];
            if burnt[lease] != rows[lease].cpu_ms {
                books.completed_mismatch += 1;
            }
            rec.completed(tick);
        }
        // 2. Ageing.
        for h in &mut hosts {
            h.tick(tick);
        }
        // 3. A round.
        if tick.is_multiple_of(WINDOW_TICKS) {
            for h in &mut hosts {
                h.begin_round(tick);
            }
            while next_arrival < rows.len() && rows[next_arrival].start_ms <= tick * 1_000 {
                queue.push(next_arrival);
                arrival_hash.u64(tick);
                arrival_hash.u64(next_arrival as u64);
                rec.arrival(rows[next_arrival].start_ms);
                next_arrival += 1;
            }
            round(
                cfg,
                rows,
                beliefs,
                ranker,
                &mut hosts,
                &mut queue,
                tick,
                &mut rec,
                &mut |lease, host, belief_ends| {
                    placements.push(Placed { lease, host, tick });
                    placement_hash.u64(lease as u64);
                    placement_hash.u64(host as u64);
                    live.insert(
                        lease,
                        Live {
                            host,
                            start_tick: tick,
                            end_tick: belief_ends,
                        },
                    );
                    ends.insert((belief_ends, lease));
                },
            );
            rec.queue_after_round(!queue.is_empty());
        }
        // 4. Truth.
        for (&lease, l) in &live {
            let age = tick - l.start_tick;
            let ms = truth.core_ms(lease, age);
            burnt[lease] += ms;
            window_core_ms[l.host] += ms;
            rec.books().host_core_ms += ms;
        }
        for (h, host) in hosts.iter().enumerate() {
            rec.residents(h, host.residents().len());
        }
        // 5. The end of a window.
        if tick % WINDOW_TICKS == WINDOW_TICKS - 1 {
            for h in 0..hosts.len() {
                end_window(
                    &mut kills,
                    &mut hosts[h],
                    h,
                    tick,
                    window_core_ms[h],
                    truth,
                    &mut live,
                    &mut ends,
                    &burnt,
                    &mut rec,
                );
                window_core_ms[h] = 0;
            }
        }
        // 6. The end of a minute.
        if tick % MINUTE_TICKS == MINUTE_TICKS - 1 {
            rec.minute_end(tick / MINUTE_TICKS);
        }
        tick += 1;
    }

    let queued: Vec<u64> = queue
        .iter()
        .copied()
        .chain(next_arrival..rows.len())
        .map(|i| rows[i].start_ms)
        .collect();
    let c = rec.counters();
    c.queued_at_end = queued.len() as u64;
    c.running_at_end = live.len() as u64;
    let running: u64 = live.keys().map(|&l| burnt[l]).sum();
    rec.books().running_core_ms = running;
    Replay {
        metrics: rec.finish(tick, &queued),
        placements,
        placement_hash: placement_hash.0,
        arrival_hash: arrival_hash.0,
        kills,
        end_tick: tick,
    }
}

/// One placement round over `queue`: the leases placed leave it, the rest keep their order.
#[allow(clippy::too_many_arguments)]
fn round<'w>(
    cfg: &Config,
    rows: &[Row],
    beliefs: &'w dyn Beliefs,
    ranker: &mut dyn Ranker,
    hosts: &mut [HostState<'w>],
    queue: &mut Vec<usize>,
    tick: u64,
    rec: &mut Recorder,
    on_place: &mut dyn FnMut(usize, usize, u64),
) {
    let waiting = std::mem::take(queue);
    rec.counters().rounds += 1;
    rec.counters().lease_rounds += waiting.len() as u64;
    let mut refusals = 0u64;
    let mut attempts = 0u64;
    let mut placed_here = 0u64;
    let mut looked = 0usize;
    for (i, &lease) in waiting.iter().enumerate() {
        if refusals >= SCAN_REFUSALS {
            queue.extend_from_slice(&waiting[i..]);
            break;
        }
        looked += 1;
        attempts += 1;
        let belief = beliefs.belief(lease);
        let mut candidates = Vec::new();
        for (h, host) in hosts.iter().enumerate() {
            if host.excluded {
                continue;
            }
            match admit(&cfg.params, host, belief) {
                Ok(()) => candidates.push(h),
                Err(why) => {
                    rec.refused(h);
                    let c = rec.counters();
                    match why {
                        Refusal::LeaseCap => c.refused_lease_cap += 1,
                        Refusal::Cpu { .. } => c.refused_cpu += 1,
                        Refusal::Memory { .. } => c.refused_memory += 1,
                        Refusal::ColdCap => c.refused_cold_cap += 1,
                    }
                }
            }
        }
        if candidates.is_empty() {
            refusals += 1;
            rec.counters().refusals += 1;
            queue.push(lease);
            continue;
        }
        let pick = ranker.choose(&Choose {
            lease,
            tick,
            belief,
            hosts,
            candidates: &candidates,
            params: &cfg.params,
        });
        assert!(
            candidates.contains(&pick.host),
            "the ranker chose host {} which is not a candidate",
            pick.host
        );
        let c = rec.counters();
        c.scored_candidates += pick.scored_candidates;
        c.evaluations += pick.evaluations;
        c.scored_placements += u64::from(pick.scored_candidates >= 2);
        c.scored_beyond_candidates += pick
            .scored_candidates
            .saturating_sub(candidates.len() as u64);
        c.placed += 1;
        placed_here += 1;
        hosts[pick.host].place(lease, tick, belief);
        ranker.placed(lease);
        let wall_ticks = rows[lease].wall_ms.div_ceil(1_000);
        on_place(lease, pick.host, tick + wall_ticks);
        rec.placed(rows[lease].start_ms, tick, belief.cold, pick.decided);
    }
    let unexamined = (waiting.len() - looked) as u64;
    let c = rec.counters();
    c.attempts += attempts;
    c.worst_round_excess = c
        .worst_round_excess
        .max(attempts.saturating_sub(placed_here));
    c.unexamined_sum += unexamined;
    c.unexamined_max = c.unexamined_max.max(unexamined);
    c.rounds_with_unexamined += u64::from(unexamined > 0);
}

/// Reads `host` at the end of a window and kills while its working sets exceed its memory.
#[allow(clippy::too_many_arguments)]
fn end_window(
    kills: &mut Vec<(usize, u64)>,
    host: &mut HostState<'_>,
    index: usize,
    tick: u64,
    core_ms: u64,
    truth: &dyn Truth,
    live: &mut BTreeMap<usize, Live>,
    ends: &mut BTreeSet<(u64, usize)>,
    burnt: &[u64],
    rec: &mut Recorder,
) {
    let mut sets: Vec<(u64, u64, usize)> = host
        .residents()
        .iter()
        .map(|(&lease, r)| {
            (
                truth.working_set_mib(lease, tick - r.start_tick),
                r.start_tick,
                lease,
            )
        })
        .collect();
    let mut total: u64 = sets.iter().map(|s| s.0).sum();
    rec.window(index, tick, core_ms, total);
    sets.sort_unstable();
    while total > host.cap.mem_mib {
        // The largest working set; the newest, then the higher lease index, among equals.
        let (ws, _, lease) = sets.pop().expect("a resident holds the memory");
        let l = live.remove(&lease).expect("a live lease");
        ends.remove(&(l.end_tick, lease));
        host.leave(lease);
        total -= ws;
        kills.push((lease, tick));
        let c = rec.counters();
        c.killed += 1;
        rec.books().killed_core_ms += burnt[lease];
    }
    host.read(core_ms, total);
}
