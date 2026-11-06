//! The `prepare` and `cutover` steps over the rig's fakes: what they read, what they refuse,
//! what they record, and the acts `cutover` makes and in which order.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use js_lease_server::update_hook::step::Env;
use js_lease_server::update_hook::{
    Done, Held, Link, Outcome, PortError, Reason, Schema, Step, StoreUnreachable, Units,
};
use js_update_hook::{Class, UnitState};

use crate::support::{RETRY_AFTER_SECS, Rig, as_root, free, held};

/// Makes the release link a case names, from the rig.
type LinkCase = Box<dyn Fn(&mut Rig) -> PathBuf>;

/// The ending a test expects: exit status, reason, class.
fn assert_ends(done: &Done, outcome: Outcome, reason: Reason, class: Class) {
    assert_eq!(done.outcome, outcome, "{done:?}");
    assert_eq!(done.report.reason, Some(reason), "{done:?}");
    assert_eq!(done.report.class, class, "{done:?}");
}

/// A rig whose `prepare` has run on a server holding the lock as `old`, with the store set to
/// release the lock after the stop.
fn cut_rig(name: &str) -> Rig {
    let rig = Rig::new(name);
    rig.put(&rig.prepared(Some("old")));
    rig.store.answer([held("old"), free()]);
    rig
}

fn stopped_calls(rig: &Rig) -> Vec<String> {
    vec![
        "stop".to_owned(),
        rig.relink_to(&rig.staged()),
        "start".to_owned(),
    ]
}

/// A unit manager that reads a state for `allowed` looks and fails every look after.
struct LimitedUnits<'a> {
    inner: &'a crate::support::FakeUnits,
    allowed: Cell<u32>,
}

impl Units for LimitedUnits<'_> {
    fn show(&self, unit: &str) -> Result<UnitState, PortError> {
        match self.allowed.get() {
            0 => Err(PortError::Command("systemctl: exit status 1".to_owned())),
            n => {
                self.allowed.set(n - 1);
                self.inner.show(unit)
            }
        }
    }

    fn stop_no_block(&self, unit: &str) -> Result<(), PortError> {
        self.inner.stop_no_block(unit)
    }

    fn start(&self, unit: &str) -> Result<(), PortError> {
        self.inner.start(unit)
    }

    fn is_active(&self, unit: &str) -> Result<bool, PortError> {
        self.inner.is_active(unit)
    }
}

/// A release link that answers its first reads from a script and then as the rig's does.
struct ScriptedLink<'a> {
    inner: &'a crate::support::FakeLink,
    script: RefCell<VecDeque<Result<Option<PathBuf>, PortError>>>,
}

impl Link for ScriptedLink<'_> {
    fn target(&self, link: &Path) -> Result<Option<PathBuf>, PortError> {
        match self.script.borrow_mut().pop_front() {
            Some(answer) => answer,
            None => self.inner.target(link),
        }
    }

    fn relink(&self, link: &Path, target: &Path) -> Result<(), PortError> {
        self.inner.relink(link, target)
    }
}

/// The rig's environment with its unit manager replaced.
fn env_units<'a>(rig: &'a Rig, units: &'a dyn Units) -> Env<'a> {
    let mut env = rig.env();
    env.ports.units = units;
    env
}

/// The rig's environment with its release link replaced.
fn env_link<'a>(rig: &'a Rig, link: &'a dyn Link) -> Env<'a> {
    let mut env = rig.env();
    env.ports.link = link;
    env
}

// ---------------------------------------------------------------------------------- prepare

/// Asserts that when the store's writer-lock probe does not answer, `prepare` ends 75 with class
/// environment and `store_unreachable`, gives the params' retry hint, makes no unit call, leaves
/// the link where it was and writes no record. Catches a prepare that goes on past an
/// unreachable store, or reports 1 or 76.
#[test]
fn prepare_refuses_when_the_store_is_unreachable() {
    let rig = Rig::new("spc-prepare-unreachable");
    rig.store.answer([Err(StoreUnreachable)]);
    let done = rig.run(Step::Prepare);
    assert_ends(
        &done,
        Outcome::Deferred,
        Reason::StoreUnreachable,
        Class::Environment,
    );
    assert_eq!(done.outcome.code(), 75);
    assert_eq!(done.report.retry_after_secs, Some(RETRY_AFTER_SECS));
    assert_eq!(
        done.report.detail.as_deref(),
        Some("the store could not be reached")
    );
    assert!(done.report.reversible);
    assert_eq!(rig.calls(), Vec::<String>::new());
    assert_eq!(*rig.link.target.borrow(), Some(rig.previous()));
    assert_eq!(rig.record(), None);
}

/// Asserts that when the store's schema read does not answer, `prepare` defers the same way
/// (75, environment, the retry hint) before it probes the lock, and writes no record.
#[test]
fn prepare_defers_when_the_schema_read_does_not_answer() {
    let rig = Rig::new("spc-prepare-schema-unreachable");
    *rig.store.schema.borrow_mut() = Err(StoreUnreachable);
    let done = rig.run(Step::Prepare);
    assert_ends(
        &done,
        Outcome::Deferred,
        Reason::StoreUnreachable,
        Class::Environment,
    );
    assert_eq!(done.report.retry_after_secs, Some(RETRY_AFTER_SECS));
    assert_eq!(rig.store.probed.get(), 0);
    assert_eq!(rig.record(), None);
}

/// Asserts that `prepare` on a running server exits 0 with a clean report, records the holder
/// the probe found, the previous link target, the schema and no mark, and makes no call.
#[test]
fn prepare_records_the_holder_and_stops_nothing() {
    let rig = Rig::new("spc-prepare-holder");
    rig.store.answer([held("h-1")]);
    let done = rig.run(Step::Prepare);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(done.outcome.code(), 0);
    assert_eq!(done.report.detail, None);
    let record = rig.record().expect("a record");
    assert_eq!(record.holder.as_deref(), Some("h-1"));
    assert_eq!(record.previous_target, Some(rig.previous()));
    assert_eq!(record.store_schema, Some(2));
    assert_eq!(record, rig.prepared(Some("h-1")));
    assert_eq!(rig.calls(), Vec::<String>::new());
}

/// Asserts that `prepare` finding nobody holding the lock (a stopped server) succeeds and
/// records no holder.
#[test]
fn prepare_with_no_server_running_is_not_a_failure() {
    let rig = Rig::new("spc-prepare-free");
    rig.store.answer([free()]);
    rig.units.set("inactive", "dead");
    let done = rig.run(Step::Prepare);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(rig.record().expect("a record").holder, None);
    assert_eq!(rig.calls(), Vec::<String>::new());
}

/// Asserts that a first install (no previous release, no link) prepares: exit 0, the record
/// holds no previous target.
#[test]
fn prepare_on_a_first_install_records_no_previous_target() {
    let rig = Rig::first_install("spc-prepare-first");
    let done = rig.run(Step::Prepare);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    let record = rig.record().expect("a record");
    assert_eq!(record.previous_target, None);
    assert_eq!(record.previous, None);
}

/// Asserts that a release link that cannot be read as a symbolic link halts the step (76,
/// `state_invalid`, class release) with the link named, and writes no record.
#[test]
fn prepare_halts_when_the_link_is_not_a_symlink() {
    let rig = Rig::new("spc-prepare-not-symlink");
    rig.link.read_fails.set(true);
    let done = rig.run(Step::Prepare);
    assert_ends(&done, Outcome::Halt, Reason::StateInvalid, Class::Release);
    assert_eq!(done.outcome.code(), 76);
    let link = rig.path("link/current");
    assert_eq!(
        done.report.detail,
        Some(format!(
            "{} cannot be read as a symbolic link: Invalid argument (os error 22)",
            link.display()
        ))
    );
    assert_eq!(rig.record(), None);
}

/// Asserts that a link pointing outside the releases directory halts (76, `state_invalid`).
#[test]
fn prepare_halts_when_the_link_points_outside_the_releases_directory() {
    let rig = Rig::new("spc-prepare-outside");
    *rig.link.target.borrow_mut() = Some(rig.path("link/elsewhere"));
    let done = rig.run(Step::Prepare);
    assert_ends(&done, Outcome::Halt, Reason::StateInvalid, Class::Release);
    assert_eq!(
        done.report.detail,
        Some(format!(
            "{} points outside the release directory {}",
            rig.path("link/current").display(),
            rig.path("releases").display()
        ))
    );
    assert_eq!(rig.record(), None);
}

/// Asserts that a link pointing at a release other than the params' previous one halts (76,
/// `state_invalid`) and the message names both.
#[test]
fn prepare_halts_when_the_link_points_at_another_release() {
    let rig = Rig::new("spc-prepare-other-release");
    *rig.link.target.borrow_mut() = Some(rig.path("releases/3.0.0"));
    let done = rig.run(Step::Prepare);
    assert_ends(&done, Outcome::Halt, Reason::StateInvalid, Class::Release);
    assert_eq!(
        done.report.detail,
        Some(format!(
            "{} points at {}, not at the previous release the params name ({})",
            rig.path("link/current").display(),
            rig.path("releases/3.0.0").display(),
            rig.previous().display()
        ))
    );
    assert_eq!(rig.record(), None);
}

/// Asserts that a link present on a first install halts (76, `state_invalid`).
#[test]
fn prepare_halts_when_a_first_install_finds_a_link() {
    let rig = Rig::first_install("spc-prepare-first-with-link");
    *rig.link.target.borrow_mut() = Some(rig.previous());
    let done = rig.run(Step::Prepare);
    assert_ends(&done, Outcome::Halt, Reason::StateInvalid, Class::Release);
    assert_eq!(
        done.report.detail,
        Some(format!(
            "{} points at {}, not at the previous release the params name (nothing)",
            rig.path("link/current").display(),
            rig.previous().display()
        ))
    );
    assert_eq!(rig.record(), None);
}

/// Asserts that no link while the params name a previous release halts (76, `state_invalid`)
/// and the message says the link points at nothing.
#[test]
fn prepare_halts_when_the_link_is_missing_but_a_previous_release_is_named() {
    let rig = Rig::new("spc-prepare-no-link");
    *rig.link.target.borrow_mut() = None;
    let done = rig.run(Step::Prepare);
    assert_ends(&done, Outcome::Halt, Reason::StateInvalid, Class::Release);
    assert_eq!(
        done.report.detail,
        Some(format!(
            "{} points at nothing, not at the previous release the params name ({})",
            rig.path("link/current").display(),
            rig.previous().display()
        ))
    );
    assert_eq!(rig.record(), None);
}

/// Asserts that a staged release that is not directly inside a declared directory (outside
/// every one, inside the link's directory, or inside the state directory) is `bad_params`
/// (exit 1, class release) and nothing is written.
#[test]
fn prepare_refuses_a_staged_release_outside_the_declared_release_directories() {
    for (n, staged) in ["elsewhere/2.0.0", "link/2.0.0", "state/2.0.0"]
        .iter()
        .enumerate()
    {
        let mut rig = Rig::new(&format!("spc-prepare-staged-{n}"));
        rig.params.staged_release = rig.path(staged);
        let done = rig.run(Step::Prepare);
        assert_ends(&done, Outcome::Failure, Reason::BadParams, Class::Release);
        assert_eq!(
            done.report.detail,
            Some(format!(
                "the staged release {} is not directly inside a declared release directory",
                rig.path(staged).display()
            )),
            "{staged}"
        );
        assert_eq!(rig.record(), None);
        assert_eq!(rig.calls(), Vec::<String>::new());
    }
}

/// Asserts that a store with no schema fails `prepare` with `schema_unsupported` (exit 1, class
/// release), before the lock is probed, and nothing is written.
#[test]
fn prepare_fails_when_the_store_holds_no_schema() {
    let rig = Rig::new("spc-prepare-schema-missing");
    *rig.store.schema.borrow_mut() = Ok(Schema::Missing);
    let done = rig.run(Step::Prepare);
    assert_ends(
        &done,
        Outcome::Failure,
        Reason::SchemaUnsupported,
        Class::Release,
    );
    assert_eq!(
        done.report.detail.as_deref(),
        Some("the store holds no schema")
    );
    assert_eq!(rig.store.probed.get(), 0);
    assert_eq!(rig.record(), None);
}

/// Asserts that a schema older than the release's oldest fails `prepare` with
/// `schema_unsupported` and says both versions.
#[test]
fn prepare_fails_when_the_schema_is_too_old() {
    let rig = Rig::new("spc-prepare-schema-old");
    *rig.store.schema.borrow_mut() = Ok(Schema::TooOld { found: 1, min: 2 });
    let done = rig.run(Step::Prepare);
    assert_ends(
        &done,
        Outcome::Failure,
        Reason::SchemaUnsupported,
        Class::Release,
    );
    assert_eq!(
        done.report.detail.as_deref(),
        Some("the store's schema is version 1, older than this release's 2")
    );
    assert_eq!(rig.record(), None);
}

/// Asserts that a backend that cannot be read without the lock (`Unchecked`) prepares with
/// `store_schema` recorded as `None`.
#[test]
fn prepare_records_no_schema_when_the_backend_cannot_be_read() {
    let rig = Rig::new("spc-prepare-schema-unchecked");
    *rig.store.schema.borrow_mut() = Ok(Schema::Unchecked);
    let done = rig.run(Step::Prepare);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(rig.record().expect("a record").store_schema, None);
}

/// Asserts that a rerun of `prepare` with the update's record there observes again (probes the
/// store) but keeps the record as it was: a holder that changed is not written. It exits 0 and
/// says the record is kept.
#[test]
fn prepare_rerun_keeps_the_record_it_found() {
    let rig = Rig::new("spc-prepare-rerun");
    rig.put(&rig.prepared(Some("old")));
    rig.store.answer([held("new")]);
    let done = rig.run(Step::Prepare);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(
        done.report.detail.as_deref(),
        Some("the record of this update is there already and is kept")
    );
    assert_eq!(rig.store.probed.get(), 1);
    assert_eq!(
        rig.record().expect("a record").holder.as_deref(),
        Some("old")
    );
}

/// Asserts that a `--unit` other than the declared unit is refused before anything is read or
/// written: no record that every later step would refuse, `cleanup` included. Catches a record
/// the hook can never delete.
#[test]
fn prepare_refuses_a_unit_other_than_the_declared_one() {
    let mut rig = Rig::new("spc-prepare-other-unit");
    rig.params.unit = "other.service".to_owned();
    let done = rig.run(Step::Prepare);
    assert_ends(&done, Outcome::Failure, Reason::BadParams, Class::Release);
    assert_eq!(
        done.report.detail.as_deref(),
        Some("--unit other.service is not the declared unit js-lease-server.service")
    );
    assert_eq!(rig.record(), None);
    assert_eq!(rig.store.probed.get(), 0);
}

/// Asserts that a rerun of `prepare` against a record whose stop is begun is refused as
/// `state_invalid` (exit 1) with the advice to roll back, and the record is not touched.
#[test]
fn prepare_refuses_a_record_whose_stop_is_begun() {
    let rig = Rig::new("spc-prepare-stop-begun");
    let mut record = rig.prepared(Some("old"));
    record.stop_begun = true;
    rig.put(&record);
    let done = rig.run(Step::Prepare);
    assert_ends(
        &done,
        Outcome::Failure,
        Reason::StateInvalid,
        Class::Release,
    );
    assert_eq!(
        done.report.detail.as_deref(),
        Some("the record says cutover has begun: run rollback")
    );
    assert_eq!(rig.record(), Some(record));
}

/// Asserts that a record of another update (another staged release) is `state_invalid` for both
/// steps, and that a record of another unit is too; nothing is changed.
#[test]
fn a_record_of_another_update_or_unit_is_state_invalid() {
    for step in [Step::Prepare, Step::Cutover] {
        let rig = cut_rig(&format!("spc-other-update-{}", step.name()));
        let mut record = rig.prepared(Some("old"));
        record.staged = rig.path("releases/9.9.9");
        rig.put(&record);
        let done = rig.run(step);
        assert_ends(
            &done,
            Outcome::Failure,
            Reason::StateInvalid,
            Class::Release,
        );
        assert_eq!(
            done.report.detail.as_deref(),
            Some(
                "the record is not valid: the record is of another update than the params table's"
            )
        );
        assert_eq!(rig.record(), Some(record));
        assert_eq!(rig.calls(), Vec::<String>::new());

        let mut record = rig.prepared(Some("old"));
        record.unit = "other.service".to_owned();
        rig.put(&record);
        let done = rig.run(step);
        assert_ends(
            &done,
            Outcome::Failure,
            Reason::StateInvalid,
            Class::Release,
        );
        assert_eq!(
            done.report.detail.as_deref(),
            Some(
                "the record is not valid: the record's unit \"other.service\" is not the declared unit named by --unit"
            )
        );
        assert_eq!(rig.calls(), Vec::<String>::new());
    }
}

/// Asserts that a record file that is not a record ends both steps `state_invalid`.
#[test]
fn an_unreadable_record_is_state_invalid() {
    for step in [Step::Prepare, Step::Cutover] {
        let rig = cut_rig(&format!("spc-corrupt-{}", step.name()));
        std::fs::write(rig.path("state/record.json"), "this is not a record").unwrap();
        let done = rig.run(step);
        assert_ends(
            &done,
            Outcome::Failure,
            Reason::StateInvalid,
            Class::Release,
        );
        assert_eq!(rig.calls(), Vec::<String>::new());
    }
}

/// Asserts that a state directory another step holds the lock of ends both steps `locked`, and
/// the holder's lock is untouched.
#[test]
fn a_locked_state_directory_ends_the_step_locked() {
    for step in [Step::Prepare, Step::Cutover] {
        let rig = cut_rig(&format!("spc-locked-{}", step.name()));
        let Ok(holder) = Held::open(&rig.declaration.state_dir, &as_root) else {
            panic!("the test's own lock");
        };
        let done = rig.run(step);
        assert_ends(&done, Outcome::Failure, Reason::Locked, Class::Release);
        assert_eq!(rig.calls(), Vec::<String>::new());
        drop(holder);
        assert_ne!(rig.run(Step::Prepare).report.reason, Some(Reason::Locked));
    }
}

/// Asserts that a missing `--release-link` is `bad_params` for both steps and nothing is
/// changed.
#[test]
fn a_missing_release_link_is_bad_params() {
    for step in [Step::Prepare, Step::Cutover] {
        let mut rig = cut_rig(&format!("spc-no-link-param-{}", step.name()));
        rig.params.release_link = None;
        let done = rig.run(step);
        assert_ends(&done, Outcome::Failure, Reason::BadParams, Class::Release);
        assert_eq!(
            done.report.detail.as_deref(),
            Some("--release-link is required")
        );
        assert_eq!(rig.calls(), Vec::<String>::new());
    }
}

/// Asserts that a release link outside every declared directory, one that is a declared
/// directory itself or has no parent, one directly in the state directory, one that is not
/// absolute, and ones inside the staged or the previous release are all `bad_params`, for both
/// steps, and nothing is changed.
#[test]
fn a_release_link_out_of_bounds_is_bad_params() {
    for step in [Step::Prepare, Step::Cutover] {
        let cases: [(&str, LinkCase); 7] = [
            ("outside", Box::new(|rig| rig.path("elsewhere/current"))),
            ("declared-dir", Box::new(|rig| rig.path("link"))),
            ("no-parent", Box::new(|_| PathBuf::from("/"))),
            ("state-dir", Box::new(|rig| rig.path("state/current"))),
            ("relative", Box::new(|_| PathBuf::from("link/current"))),
            (
                "in-staged",
                Box::new(|rig| {
                    let staged = rig.path("releases/2.0.0");
                    rig.declaration.write_paths.push(staged.clone());
                    staged.join("current")
                }),
            ),
            (
                "in-previous",
                Box::new(|rig| {
                    let previous = rig.path("releases/1.0.0");
                    rig.declaration.write_paths.push(previous.clone());
                    previous.join("current")
                }),
            ),
        ];
        for (label, make) in &cases {
            let mut rig = cut_rig(&format!("spc-bad-link-{}-{label}", step.name()));
            let link = make(&mut rig);
            rig.params.release_link = Some(link.clone());
            let done = rig.run(step);
            assert_ends(&done, Outcome::Failure, Reason::BadParams, Class::Release);
            assert_eq!(
                done.report.detail,
                Some(format!(
                    "the release link {} is not directly inside a declared directory other than the state directory",
                    link.display()
                )),
                "{label}"
            );
            assert_eq!(rig.calls(), Vec::<String>::new(), "{label}");
        }
    }
}

/// Asserts that a `prepare` killed at its only kill point (before the record is written) leaves
/// no record, and a rerun writes it and exits 0.
#[test]
fn prepare_killed_before_its_save_leaves_no_record_and_reruns_clean() {
    let rig = Rig::new("spc-prepare-killed");
    assert!(rig.run_killed_at(Step::Prepare, 1).is_none());
    assert_eq!(rig.record(), None);
    assert_eq!(rig.run(Step::Prepare).outcome, Outcome::Success);
    assert_eq!(rig.record(), Some(rig.prepared(Some("old"))));
}

#[path = "steps_prepare_cutover/cutover.rs"]
mod cutover;
