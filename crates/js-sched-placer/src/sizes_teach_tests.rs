//! A teaching of the ended jobs read after the passes began ([`SchedPlacer::begin_teaching`],
//! [`Teaching::read_page`], [`SchedPlacer::teach`]): what its start reads, the history it
//! leaves once taught, and a teaching whose read fails. The job tables on the in-memory fake,
//! every clock the test's; the reads are counted at the store, never timed.

use std::time::Duration;

use std::collections::BTreeMap;

use js_jobs::interfere::Read;
use js_jobs::{Booked, EndReason, JobEnd, JobId, JobRecord, NewJob, Output, SizedFrom};
use js_types::Labels;

use super::super::tests::{
    GIB, MIB, allocation, booked, exited, finish_at, needs, of_class, output, pass, placer, ran,
    runs, submit,
};
use super::super::{SIZE_CLASS_LABEL, SIZE_FALLBACK_LABEL};
use super::Teaching;
use crate::engine::rig::{Tables, host, spec_of};
use crate::place::SchedPlacer;

/// `n` jobs that were placed and cancelled before they ran, freed: ended jobs that teach
/// nothing, and that the read pages through.
fn cancelled(tables: &Tables, n: usize) {
    for at in 0..n {
        let id = JobId::parse(&format!("zz-{at:04}")).unwrap();
        let new = NewJob {
            id: Some(id.clone()),
            spec: spec_of(needs(1, 1), 600),
            priority: 0,
            client: None,
        };
        tables.jobs.insert_queued(new).unwrap();
        tables.jobs.place(&id, host("a"), booked(1, 1)).unwrap();
        let end = JobEnd::Cancelled {
            exit_code: None,
            output: Output::default(),
        };
        tables
            .jobs
            .end_and_free(&id, end, EndReason::Cancelled)
            .unwrap();
    }
}

/// Ended runs, in the order they ended, each with what it booked: successes of `build` (2 GiB
/// peaks), successes of `link` and then a memory kill of it (`a-oom`, which raises its floor and
/// sorts before them by its id), a failure of `test`, and a success of the client `ci` whose
/// class names the fallback class `kind`; then `fillers` cancelled jobs. The ids' order is not
/// the order the jobs ended in.
fn history(tables: &Tables, fillers: usize) -> Vec<(JobRecord, Booked)> {
    let mut ended = Vec::new();
    let build = || exited(0, 2 * GIB, Duration::from_secs(30));
    let built = runs(tables, "build", 4, "build", booked(2000, 4 * GIB), build);
    ended.extend(built.into_iter().map(|job| (job, booked(2000, 4 * GIB))));
    let link = || exited(0, 5 * GIB, Duration::from_secs(20));
    let linked = runs(tables, "link", 2, "link", booked(2000, 6 * GIB), link);
    ended.extend(linked.into_iter().map(|job| (job, booked(2000, 6 * GIB))));
    let killed = JobEnd::Memory {
        exit_code: Some(137),
        output: output(6 * GIB, Duration::from_secs(10)),
    };
    let spec = of_class("link", needs(0, 0));
    let job = ran(tables, ("a-oom", None), spec, booked(2000, 6 * GIB), killed);
    ended.push((job, booked(2000, 6 * GIB)));
    let spec = of_class("test", needs(0, 0));
    let failed = exited(1, GIB, Duration::from_secs(5));
    let job = ran(
        tables,
        ("failed", None),
        spec,
        booked(2000, 4 * GIB),
        failed,
    );
    ended.push((job, booked(2000, 4 * GIB)));
    let mut spec = of_class("target", needs(0, 0));
    let labels = BTreeMap::from([
        (SIZE_CLASS_LABEL.to_owned(), "target".to_owned()),
        (SIZE_FALLBACK_LABEL.to_owned(), "kind".to_owned()),
    ]);
    spec.labels = Labels::from_caller(&labels).unwrap();
    let end = exited(0, 3 * GIB, Duration::from_secs(40));
    let job = ran(
        tables,
        ("ci-0", Some("ci")),
        spec,
        booked(2000, 4 * GIB),
        end,
    );
    ended.push((job, booked(2000, 4 * GIB)));
    cancelled(tables, fillers);
    ended
}

/// What a server that read every ended job before its first pass did at its start, written
/// out apart from the code under test: `ended` taught in the order the jobs ended, and the live
/// allocations of `tables` taken as those of the last pass. Answers how many taught a sample.
fn taught_first(placer: &SchedPlacer, tables: &Tables, ended: &[(JobRecord, Booked)]) -> usize {
    let live = tables.jobs.live_allocations().unwrap();
    let mut engine = placer.engine();
    let sizes = engine.sizes();
    let taught = ended
        .iter()
        .filter(|(job, booked)| sizes.observe(job, *booked))
        .count();
    let _ = sizes.freed(&live);
    taught
}

/// Records `id` of `class` and places it on `a` booked as two cores and 4 GiB: a job whose
/// allocation is live.
fn live(tables: &Tables, id: &str, class: &str) {
    submit(tables, id, class);
    let id = JobId::parse(id).unwrap();
    tables
        .jobs
        .place(&id, host("a"), booked(2000, 4 * GIB))
        .unwrap();
}

/// Reads `teaching` through: every page, and the number of pages.
fn read_through(teaching: &mut Teaching, tables: &Tables) -> usize {
    while teaching.read_page(&tables.jobs).unwrap() {}
    teaching.pages()
}

/// The history of `placer` as its engine holds it, in full.
fn held_history(placer: &SchedPlacer) -> String {
    format!("{:?}", placer.engine().sizes())
}

/// A start over [`history`] with 1000 fillers and a job live, then the passes of a farm: a job
/// of `build` placed, the live job and it freed, a job of `link` placed and freed with no pass
/// between, and a job of each class placed after, one pass each. When `first`, the start
/// teaches every ended job before the first pass ([`taught_first`]); else it begins a teaching and the
/// teaching is read and taught after the passes have freed those jobs. Answers the history once
/// taught, what each job placed was booked, in order, and the pages the teaching read (0 when
/// `first`).
fn farm(first: bool) -> (String, Vec<(u64, u64)>, usize) {
    let tables = Tables::new();
    let ended = history(&tables, 1000);
    live(&tables, "live", "build");
    let placer = placer();
    let mut teaching = match first {
        true => {
            assert_eq!(taught_first(&placer, &tables, &ended), 8);
            None
        }
        false => Some(placer.begin_teaching(&tables.jobs).unwrap()),
    };
    submit(&tables, "next", "build");
    assert_eq!(pass(&placer, &tables).len(), 1);
    finish_at(&tables, "live", 3 * GIB);
    finish_at(&tables, "next", 6 * GIB);
    submit(&tables, "late", "link");
    assert_eq!(pass(&placer, &tables).len(), 1, "frees live and next");
    finish_at(&tables, "late", 7 * GIB);
    let pages = teaching.as_mut().map_or(0, |teaching| {
        let pages = read_through(teaching, &tables);
        assert_eq!(teaching.read_page(&tables.jobs), Ok(false));
        tables.store.take_reads();
        assert_eq!(teaching.read_page(&tables.jobs), Ok(false));
        assert_eq!(
            tables.store.take_reads(),
            Vec::new(),
            "read through: reads none"
        );
        pages
    });
    let taught = teaching.map(|teaching| placer.teach(teaching));
    assert_eq!(
        taught.unwrap_or(8),
        8,
        "the eight ended runs of the history with a sample"
    );
    submit(&tables, "after-build", "build");
    assert_eq!(pass(&placer, &tables).len(), 1, "frees late");
    finish_at(&tables, "after-build", 2 * GIB);
    submit(&tables, "after-link", "link");
    assert_eq!(pass(&placer, &tables).len(), 1);
    let placed = ["next", "late", "after-build", "after-link"].map(|id| allocation(&tables, id));
    let placed = placed.iter().map(|booked| match booked {
        js_jobs::Booked::Shape {
            cpu_millis,
            memory_bytes,
            ..
        } => (*cpu_millis, *memory_bytes),
        js_jobs::Booked::WholeHost => panic!("a share is booked as a share"),
    });
    (held_history(&placer), placed.collect(), pages)
}

/// Asserts that a start that begins a teaching reads the live allocations alone: the same
/// reads, key for key, over a store of 8 ended jobs and over one of 1008, and none of a job's
/// row or of the job table, and that it teaches nothing (a job sized then books the cold size).
/// Catches a start whose reads grow with the history the store keeps, which is what held a
/// server's start for the minutes its reads of every ended job took.
#[test]
fn a_begun_teaching_reads_the_live_allocations_alone_whatever_the_history() {
    let begun = |fillers: usize| {
        let tables = Tables::new();
        history(&tables, fillers);
        live(&tables, "live", "build");
        let placer = placer();
        tables.store.take_reads();
        let _teaching = placer.begin_teaching(&tables.jobs).unwrap();
        let reads = tables.store.take_reads();
        submit(&tables, "next", "build");
        pass(&placer, &tables);
        (reads, allocation(&tables, "next"))
    };
    let (few, sized) = begun(0);
    let (many, _) = begun(1000);
    assert_eq!(few, many, "the same reads whatever the history");
    assert!(
        few.iter().all(|read| match read {
            Read::Get(key) => !key.starts_with("job/"),
            Read::Scan(prefix) => !prefix.starts_with("job/") && !prefix.starts_with("alloc/"),
        }),
        "no job row and no table scan: {few:?}"
    );
    assert!(
        few.contains(&Read::Get("alloc/live".to_owned())),
        "the live allocation is read: {few:?}"
    );
    assert_eq!(sized, booked(2000, 4 * GIB), "nothing taught yet: cold");
}

/// Asserts that a teaching read and taught after the passes leaves the history, in full, that a
/// start which taught every ended job before its first pass leaves (the same summaries, the
/// same live allocations, nothing held), and books the jobs placed after it the same; that its
/// read pages through every job (two pages for 1000 fillers and the rest); and that a job
/// placed while it is read, before any run was freed, books its cold size, where the other
/// start booked it from the history. The live job at its start, a job placed and freed during
/// it, and one freed after the last pass before it is taught are each taught once, after the
/// ended jobs. Catches a run taught twice (by the read and by a pass), one lost between the
/// read and the passes, and held runs taught before the ended jobs, each of which changes the
/// history.
#[test]
fn a_teaching_taught_after_the_passes_leaves_the_history_of_one_taught_first() {
    let (first_history, first_placed, _) = farm(true);
    let (later_history, later_placed, pages) = farm(false);
    assert_eq!(later_history, first_history);
    assert_eq!(pages, 2);
    assert_eq!(later_placed[2..], first_placed[2..], "after it: the same");
    assert_eq!(first_placed[0], (3000, 11 * 256 * MIB), "from the history");
    assert_eq!(later_placed[0], (2000, 4 * GIB), "none freed yet: cold");
}

/// Asserts that a job placed while a teaching is read books from the runs the passes freed since
/// it began, not the cold size: a job of `build` books the one 3 GiB run of its class freed
/// meanwhile (three cores and 3.75 GiB), a job of `test`, a class with no run, books that run as
/// its client's typical run, and a job cancelled before it started, freed meanwhile, teaches
/// nothing (the job placed after it alone still books cold). The four 2 GiB runs of `build` the
/// store holds are not read yet and size nothing. Once taught, the history is the one a placer
/// taught the four ended runs and then that run holds. Catches a start whose jobs book the cold
/// size for as long as its read of the ended jobs takes, which on a farm keeping seven days of
/// jobs is every job placed in the minutes after each start.
#[test]
fn a_job_placed_while_a_teaching_is_read_books_from_the_runs_freed_since_it_began() {
    let tables = Tables::new();
    let build = || exited(0, 2 * GIB, Duration::from_secs(30));
    let built = runs(&tables, "build", 4, "build", booked(2000, 4 * GIB), build);
    let placer = placer();
    let mut teaching = placer.begin_teaching(&tables.jobs).unwrap();
    submit(&tables, "cancelled", "build");
    assert_eq!(pass(&placer, &tables).len(), 1);
    let end = JobEnd::Cancelled {
        exit_code: None,
        output: Output::default(),
    };
    let cancelled = JobId::parse("cancelled").unwrap();
    tables
        .jobs
        .end_and_free(&cancelled, end, EndReason::Cancelled)
        .unwrap();
    submit(&tables, "first", "build");
    assert_eq!(pass(&placer, &tables).len(), 1, "frees the cancelled job");
    assert_eq!(allocation(&tables, "first"), booked(2000, 4 * GIB), "cold");
    finish_at(&tables, "first", 3 * GIB);
    pass(&placer, &tables);
    submit(&tables, "again", "build");
    submit(&tables, "other", "test");
    assert_eq!(pass(&placer, &tables).len(), 2);
    let learned = booked(3000, 15 * 256 * MIB);
    assert_eq!(allocation(&tables, "again"), learned, "its class's run");
    assert_eq!(allocation(&tables, "other"), learned, "its client's run");
    let day = super::super::day_of(placer.now());
    let from = ["again", "other"].map(|id| {
        let id = JobId::parse(id).unwrap();
        let job = tables.jobs.get_job(&id).unwrap().unwrap();
        placer.engine().sizes().sized_from(&job, day)
    });
    assert_eq!(from, [Some(SizedFrom::Class), Some(SizedFrom::Fallback)]);
    read_through(&mut teaching, &tables);
    assert_eq!(placer.teach(teaching), 4);
    let first = JobId::parse("first").unwrap();
    let first = tables.jobs.get_job(&first).unwrap().unwrap();
    let reference = super::super::tests::placer();
    let bounds = placer.engine().sizes().bounds;
    let mut engine = reference.engine();
    let sizes = engine.sizes();
    sizes.set_bounds(bounds);
    for job in built.iter().chain([&first]) {
        let _ = sizes.observe(job, booked(2000, 4 * GIB));
    }
    let taught = format!("{:?}", placer.engine().sizes().summaries);
    assert_eq!(taught, format!("{:?}", sizes.summaries));
}

/// Asserts that a run freed while a teaching is read and before any host is known teaches the
/// interim history nothing: a 20 GiB peak of `build`, above the 16 GiB host the first pass
/// learns of, would book the class the whole host; the next job of `build` books its cold size
/// instead. Catches a run judged against no host at all, which the history itself holds until
/// a host is known and then discards as at or above the host's memory.
#[test]
fn a_run_freed_before_any_host_is_known_teaches_the_interim_history_nothing() {
    let tables = Tables::new();
    let placer = placer();
    let _teaching = placer.begin_teaching(&tables.jobs).unwrap();
    live(&tables, "first", "build");
    finish_at(&tables, "first", 20 * GIB);
    let first = JobId::parse("first").unwrap();
    let first = tables.jobs.get_job(&first).unwrap().unwrap();
    assert_eq!(placer.engine().sizes().bounds, None, "no pass yet");
    placer
        .engine()
        .sizes()
        .freed_run(first, booked(2000, 4 * GIB));
    submit(&tables, "next", "build");
    assert_eq!(pass(&placer, &tables).len(), 1);
    assert_eq!(allocation(&tables, "next"), booked(2000, 4 * GIB), "cold");
}

/// Asserts that a teaching whose read fails teaches none of the ended jobs it read and still
/// teaches the run a pass freed while it was begun: the next job of the class books from that
/// run alone (three cores and 7.5 GiB for a 6 GiB peak), not from the four 2 GiB runs of the
/// history; and that a pass frees and teaches at once from then on. Catches a failed read that
/// teaches a part of the history, one that drops the held runs, and passes that keep holding
/// runs once the teaching is given up.
#[test]
fn a_teaching_whose_read_fails_teaches_the_held_runs_alone() {
    let tables = Tables::new();
    let build = || exited(0, 2 * GIB, Duration::from_secs(30));
    runs(&tables, "build", 4, "build", booked(2000, 4 * GIB), build);
    let placer = placer();
    let mut teaching = placer.begin_teaching(&tables.jobs).unwrap();
    submit(&tables, "next", "build");
    pass(&placer, &tables);
    finish_at(&tables, "next", 6 * GIB);
    pass(&placer, &tables);
    tables.store.fail_read_after(1);
    assert!(teaching.read_page(&tables.jobs).is_err());
    assert_eq!(teaching.pages(), 0);
    assert_eq!(placer.teach(teaching), 0);
    submit(&tables, "after", "build");
    pass(&placer, &tables);
    assert_eq!(allocation(&tables, "after"), booked(3000, 30 * 256 * MIB));
    finish_at(&tables, "after", 10 * GIB);
    submit(&tables, "then", "build");
    pass(&placer, &tables);
    assert_eq!(
        allocation(&tables, "then"),
        booked(3000, 49 * 256 * MIB),
        "a 10 GiB peak taught by the pass that freed it"
    );
}

/// Asserts that a job a pass named freed while the teaching was begun, whose run the pass had
/// not yet handed over when the teaching was taught (the pass reads the job between the two,
/// outside the engine's lock), is taught once: by the pass, after the ended jobs, as a server
/// that taught first teaches it; the read, which found it freed, leaves it out. Catches the run
/// taught by the read and again by the pass.
#[test]
fn a_job_named_freed_as_the_teaching_is_taught_is_taught_once() {
    let tables = Tables::new();
    let ended = history(&tables, 0);
    live(&tables, "live", "build");
    let reference = placer();
    let _ = taught_first(&reference, &tables, &ended);
    let placer = placer();
    let mut teaching = placer.begin_teaching(&tables.jobs).unwrap();
    finish_at(&tables, "live", 3 * GIB);
    let live = JobId::parse("live").unwrap();
    let record = tables.jobs.get_job(&live).unwrap().unwrap();
    for (placer, pages) in [(&reference, None), (&placer, Some(&mut teaching))] {
        let named = placer.engine().sizes().freed(&[]);
        assert_eq!(named, vec![(live.clone(), booked(2000, 4 * GIB))]);
        if let Some(teaching) = pages {
            assert_eq!(read_through(teaching, &tables), 1);
        }
    }
    assert_eq!(placer.teach(teaching), 8);
    for placer in [&reference, &placer] {
        placer
            .engine()
            .sizes()
            .freed_run(record.clone(), booked(2000, 4 * GIB));
    }
    assert_eq!(held_history(&placer), held_history(&reference));
}

/// Asserts that a teaching whose read fails after it kept a page teaches none of the ended jobs
/// that page held (the four `build` runs among the first thousand jobs): its history is that of
/// a placer that was taught nothing, and a job of `build` books its cold size. Catches a failed
/// read that teaches the part of the history it read, which a later start could not repeat.
#[test]
fn a_read_that_fails_after_a_page_teaches_none_of_that_page() {
    let tables = Tables::new();
    let _ = history(&tables, 1000);
    let teacher = placer();
    let mut teaching = teacher.begin_teaching(&tables.jobs).unwrap();
    assert_eq!(teaching.read_page(&tables.jobs), Ok(true));
    tables.store.fail_read_after(0);
    assert!(teaching.read_page(&tables.jobs).is_err());
    assert_eq!(teaching.pages(), 1);
    assert_eq!(teacher.teach(teaching), 0);
    let untaught = placer();
    let _ = untaught.engine().sizes().freed(&[]);
    assert_eq!(held_history(&teacher), held_history(&untaught));
    submit(&tables, "next", "build");
    pass(&teacher, &tables);
    assert_eq!(allocation(&tables, "next"), booked(2000, 4 * GIB), "cold");
}

/// Asserts that [`SchedPlacer::learn_ended`] answers the error of a read of the ended jobs that
/// fails after the live allocations were read (the first scan of the jobs), and teaches none of
/// them. Catches a failed read answered as a history taught.
#[test]
fn learning_the_ended_jobs_answers_a_failed_read_of_them() {
    let tables = Tables::new();
    let _ = history(&tables, 0);
    tables.store.take_reads();
    let _probe = placer().begin_teaching(&tables.jobs).unwrap();
    let begun = tables.store.take_reads().len();
    tables.store.fail_read_after(begun);
    let learner = placer();
    assert!(learner.learn_ended(&tables.jobs).is_err());
    let untaught = placer();
    let _ = untaught.engine().sizes().freed(&[]);
    assert_eq!(held_history(&learner), held_history(&untaught));
}
