//! The decide-in-shadow line ([`Engine::judge_in_shadow`]) on hosts of 88 cores (96 800 m under
//! the default slack), with the classes and live jobs of the curve room's tests: `early` bursts to
//! 22 cores from its 20th second, `late` from its 60th, and `steady` uses 4 cores for its first
//! 100 s and a quarter core after; each flat number is what its class's history books (1000 m
//! for `early` and `late`, 3000 m for `steady`).

use std::collections::{BTreeMap, BTreeSet};

use js_jobs::{JobId, JobRecord};
use js_sched::RefusalKind;
use js_types::HostId;

use super::super::tests::{NOW_MS, engine_in, live, mode, old_steady, steady, teach_bursts, walk};
use super::ShadowRound;
use crate::curves::{CurveHosts, CurveSettings};
use crate::engine::Engine;
use crate::engine::jobs_walker::walker;
use crate::engine::rig::{Tables, at, host};
use crate::map;
use crate::sizes::curve_rig::{capture_targets, queued_of};
use crate::sizes::{Curves, day_of};

/// What a line of the shadow says before its fields.
const SAID: &str =
    " INFO js_sched_placer::sizes::profile: a job's curve booking would place it differently";

/// The line of the shadow for `fields`.
fn line(fields: &str) -> String {
    format!("{SAID} {fields}")
}

/// The lines of the shadow in `lines`, the other lines left out.
fn shadow_lines(lines: Vec<String>) -> Vec<String> {
    lines
        .into_iter()
        .filter(|line| line.starts_with(SAID))
        .collect()
}

/// On `on`, three jobs of 20 000 m with no class (60 000 m) and a job of `early` a second old.
fn crowd_with_early(engine: &mut Engine, tables: &Tables, on: &str) {
    let early = queued_of(tables, &format!("model-{on}"), "early");
    for at in 0..3 {
        let fill = (&format!("{on}-fill-{at}")[..], 20_000);
        live(engine, on, fill, (None, None));
    }
    let running = (&format!("{on}-running")[..], 1_000);
    live(engine, on, running, (Some(&early), Some(1_000)));
}

/// What a walk decided in mode `curves` over two hosts, `a` holding 60 000 m of jobs with no
/// class and a job of `early` a second old, `b` holding 32 jobs of `steady` of 3000 m that are
/// 110 s old (96 000 m flat, a quarter core each by their curve), of a queue of a job of `early`
/// and one of `steady`: the placements, the jobs that wait, the refusals booked, and the shadow's
/// lines.
fn two_hosts(curves: Curves) -> (Decided, Vec<String>) {
    let tables = Tables::new();
    let mut engine = engine_in(mode(curves), CurveSettings::default(), &["a", "b"]);
    teach_bursts(&mut engine, &tables);
    crowd_with_early(&mut engine, &tables, "a");
    let model = steady(&mut engine, &tables);
    old_steady(&mut engine, &["b"], &model);
    let queued = [
        queued_of(&tables, "new-early", "early"),
        queued_of(&tables, "next", "steady"),
    ];
    let (lines, _guard) = capture_targets();
    let (placed, waiting) = walk(&mut engine, &queued, (&["a", "b"], NOW_MS));
    let refused = engine.take_refused();
    ((placed, waiting, refused), shadow_lines(lines.taken()))
}

/// What a walk decided: the placements, the jobs that wait and the refusals booked.
type Decided = (
    BTreeMap<JobId, HostId>,
    Vec<(JobId, bool)>,
    crate::engine::refusals::Refused,
);

/// Asserts that shadow places as off does and logs each job the curve rule would place on another
/// host: both jobs go to `a` in both modes, with the same jobs waiting and the same refusals; in
/// shadow the job of `early` is logged with `b` as its curve's host and slot 10 (its burst would
/// meet the running job's on `a`: 82 000 m with 22 000 m more, over 96 800 m), and the job of
/// `steady` with `b` and slot 10 as well (the job of `early` placed before it on `a` is charged
/// there by its curve); off logs no such line. Catches a shadow that acts on the curve (a
/// placement, a refusal or a queue status that differs from off), a line that lacks one of its
/// five fields or names the wrong slot, and a table that does not follow the flat placements.
#[test]
fn shadow_places_as_off_and_logs_each_job_the_curve_would_place_on_another_host() {
    let (off, off_lines) = two_hosts(Curves::Off);
    let a = host("a");
    let both_on_a = BTreeMap::from([
        (JobId::parse("new-early").unwrap(), a.clone()),
        (JobId::parse("next").unwrap(), a),
    ]);
    assert_eq!(off.0, both_on_a);
    assert_eq!(off.1, Vec::new());
    assert_eq!(off.2.count(&host("b"), RefusalKind::Cpu), 2);
    assert_eq!(off_lines, Vec::<String>::new());

    let (shadow, lines) = two_hosts(Curves::Shadow);
    assert_eq!(shadow, off);
    assert_eq!(
        lines,
        [
            line("job=new-early class=early flat_host=a curve_host=b slot=10"),
            line("job=next class=steady flat_host=a curve_host=b slot=10"),
        ]
    );
}

/// Asserts the line of a job the flat placement leaves waiting: on `b` alone (96 000 m flat, 8000
/// m by curve) a job of `steady` waits in shadow as in off, and is logged once with `b` as its
/// curve's host and neither a flat host nor a slot; a second walk logs nothing more; once the job
/// has left the queue and comes back, it is logged again. Catches a job that waits logged at every
/// pass, one never logged again once it left the queue, and a line with a flat host or a slot
/// where the flat placement placed nothing.
#[test]
fn a_job_the_flat_rule_leaves_waiting_is_logged_once_while_it_waits() {
    let tables = Tables::new();
    let mut engine = engine_in(mode(Curves::Shadow), CurveSettings::default(), &["b"]);
    let model = steady(&mut engine, &tables);
    old_steady(&mut engine, &["b"], &model);
    let next = queued_of(&tables, "next", "steady");
    let queued = std::slice::from_ref(&next);
    let expected = [line("job=next class=steady curve_host=b")];
    let (lines, _guard) = capture_targets();
    let (placed, waiting) = walk(&mut engine, queued, (&["b"], NOW_MS));
    assert_eq!(placed, BTreeMap::new());
    assert_eq!(waiting, vec![(next.id.clone(), true)]);
    assert_eq!(shadow_lines(lines.taken()), expected);

    walk(&mut engine, queued, (&["b"], NOW_MS));
    assert_eq!(shadow_lines(lines.taken()), Vec::<String>::new());

    walk(&mut engine, &[], (&["b"], NOW_MS));
    walk(&mut engine, queued, (&["b"], NOW_MS));
    assert_eq!(shadow_lines(lines.taken()), expected);
}

/// Asserts that a job both rules place alike is not logged: a job of `late` on a host of four jobs
/// with no class of 24 000 m (96 000 m) waits under both rules (its flat 1000 m is over the
/// limit, and its burst at 60 s would be too), and one on a host of 60 000 m and a job of `early`
/// a second old goes there under both (their bursts fall apart). Catches a line for a job the
/// curve would leave waiting too, and one for a job whose flat host the curve keeps.
#[test]
fn a_job_both_rules_leave_waiting_or_place_alike_is_not_logged() {
    let tables = Tables::new();
    let mut engine = engine_in(mode(Curves::Shadow), CurveSettings::default(), &["a", "c"]);
    teach_bursts(&mut engine, &tables);
    for at in 0..4 {
        live(
            &mut engine,
            "c",
            (&format!("c-fill-{at}")[..], 24_000),
            (None, None),
        );
    }
    let late = queued_of(&tables, "new-late", "late");
    let (lines, _guard) = capture_targets();
    let (placed, waiting) = walk(&mut engine, std::slice::from_ref(&late), (&["c"], NOW_MS));
    assert_eq!(placed, BTreeMap::new());
    assert_eq!(waiting, vec![(late.id.clone(), true)]);
    assert_eq!(shadow_lines(lines.taken()), Vec::<String>::new());

    crowd_with_early(&mut engine, &tables, "a");
    let (placed, _) = walk(&mut engine, std::slice::from_ref(&late), (&["a"], NOW_MS));
    assert_eq!(placed, BTreeMap::from([(late.id, host("a"))]));
    assert_eq!(shadow_lines(lines.taken()), Vec::<String>::new());
}

/// Asserts the shadow of a canary: with the curve hosts naming `a` alone, a job of `early` the
/// flat rule places on `a` (the less full of two hosts alike but for 1000 m more on `b`) is logged
/// with `b`, which the curve rule does not judge and the round offered, as its curve's host; with
/// the curve hosts naming `b` alone, the same placement on `a` is not logged, `a` being judged as
/// today. Catches a canary whose other hosts are never the curve's host, and a line for a flat
/// host the settings leave out.
#[test]
fn on_a_canary_the_curve_moves_a_job_to_a_host_it_does_not_judge() {
    for (named, logged) in [("a", true), ("b", false)] {
        let tables = Tables::new();
        let curve = CurveSettings {
            hosts: CurveHosts::new([host(named)].into_iter().collect()),
            ..CurveSettings::default()
        };
        let mut engine = engine_in(mode(Curves::Shadow), curve, &["a", "b"]);
        teach_bursts(&mut engine, &tables);
        crowd_with_early(&mut engine, &tables, "a");
        crowd_with_early(&mut engine, &tables, "b");
        live(&mut engine, "b", ("b-more", 1_000), (None, None));
        let early = queued_of(&tables, "new-early", "early");
        let (lines, _guard) = capture_targets();
        let (placed, _) = walk(
            &mut engine,
            std::slice::from_ref(&early),
            (&["a", "b"], NOW_MS),
        );
        assert_eq!(placed, BTreeMap::from([(early.id, host("a"))]), "{named}");
        let expected = if logged {
            vec![line(
                "job=new-early class=early flat_host=a curve_host=b slot=10",
            )]
        } else {
            Vec::new()
        };
        assert_eq!(shadow_lines(lines.taken()), expected, "{named}");
    }
}

/// Asserts which refusals of the scheduler keep a curve host from being the curve's host: a job of
/// `steady` whose curve fits `b` (8000 m by curve) is logged with `b` when the round's only
/// refusal of `b` is `cpu`, the test the curve replaces, and not logged when it is
/// `predicted-memory`, which the curve rule keeps. Catches a curve's host that the scheduler
/// refuses by memory, and one refused for its slot-0 CPU test alone left out.
#[test]
fn a_curve_host_the_scheduler_refused_for_another_kind_than_cpu_is_not_the_curves_host() {
    let tables = Tables::new();
    let mut engine = engine_in(mode(Curves::Shadow), CurveSettings::default(), &["b"]);
    let model = steady(&mut engine, &tables);
    old_steady(&mut engine, &["b"], &model);
    walk(&mut engine, &[], (&["b"], NOW_MS));
    let next: JobRecord = queued_of(&tables, "next", "steady");
    let now = at(NOW_MS);
    let sized = engine.sizes().sized(&next, day_of(now));
    let shaped = engine.shaped(walker(&sized, now).unwrap(), &next, now);
    let b = host("b");
    let open = [&b];
    let offered = BTreeSet::from([map::worker(&b)]);
    for (kind, expected) in [
        (RefusalKind::PredictedMemory, Vec::new()),
        (
            RefusalKind::Cpu,
            vec![line("job=next class=steady curve_host=b")],
        ),
    ] {
        let refused = [(map::worker(&b), kind)];
        let (lines, _guard) = capture_targets();
        engine.judge_in_shadow(&shaped, &ShadowRound::new(&open, &offered, None, &refused));
        assert_eq!(shadow_lines(lines.taken()), expected, "{kind:?}");
    }
}
