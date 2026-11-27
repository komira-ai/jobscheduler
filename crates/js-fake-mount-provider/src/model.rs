//! What the fake holds and how it answers, with no I/O: the sources prepared, the placements
//! made, the mode the test sets, and every call it answered.
//!
//! It answers as tierstore's mount provider does where the agent can see it:
//! - `Prepare` is idempotent: a source prepared again with the same ref is a success, with
//!   another ref it is refused `FAILED_PRECONDITION`. A ref beginning [`REFUSED_PREFIX`] is
//!   refused `INVALID_ARGUMENT`, as tierstore refuses a ref it cannot read.
//! - `Mount` places the tree of a prepared source and answers its path. At the placement limit
//!   it answers `UNAVAILABLE` with tierstore's reason ("the tree was not made: too many
//!   placements: N are alive"); a source it does not hold (after a restart) is `UNAVAILABLE`
//!   "the source is not prepared here", as tierstore answers.
//! - With [`RefuseAt::Prepare`] the tree is placed at `Prepare`, and the limit is met there: a new
//!   source while the limit's placements are alive is `UNAVAILABLE` with the same reason, as
//!   tierstore answered when the agent's retry storm was seen; its `Mount` answers the path.
//! - `Release` frees the source and its placement; a source it does not know is a success.
//!   `Sweep` releases an agent's sources whose run is not kept.
//! - [`Mode::Stalled`] is a store that stopped answering: `Mount` waits (the service holds it
//!   for its wait) and then answers `UNAVAILABLE` "the tree is not ready". [`Mode::Unavailable`]
//!   answers every call `UNAVAILABLE`.

use std::collections::BTreeMap;
use std::fmt::Write as _;

/// A ref the fake refuses as one it cannot read.
pub(crate) const REFUSED_PREFIX: &str = "refused/";

/// A source: the agent, the run and the index of the source in the run.
pub(crate) type Key = (String, String, u32);

/// How the fake answers, as the test sets it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    /// As a working store.
    Serving,
    /// As a store whose upstream stopped answering: a `Mount` that would place waits.
    Stalled,
    /// Every call `UNAVAILABLE`.
    Unavailable,
}

/// Where the placement limit is met.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RefuseAt {
    /// At `Prepare` of a new source.
    Prepare,
    /// At `Mount`, as tierstore answers today.
    Mount,
}

/// A call the fake did not serve, and why.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// `UNAVAILABLE`: ask again later.
    Unavailable(String),
    /// `INVALID_ARGUMENT`: the ref is refused.
    Invalid(String),
    /// `FAILED_PRECONDITION`: the source holds another ref.
    Precondition(String),
}

/// What a `Mount` that is not refused comes to.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Mounted {
    /// The tree is placed at this path, relative to the fake's root.
    Placed(String),
    /// The store is stalled: wait, then ask again.
    Wait,
}

/// A source the fake holds.
struct Held {
    reference: String,
    placed: bool,
}

/// One answered call: when it came (milliseconds from the fake's start), its method, and its
/// answer (`ok`, `unavailable`, `invalid` or `precondition`).
#[derive(Debug, PartialEq, Eq)]
struct Call {
    at_ms: u64,
    method: &'static str,
    answer: &'static str,
}

/// The fake's state.
pub(crate) struct Store {
    limit: usize,
    refuse_at: RefuseAt,
    mode: Mode,
    sources: BTreeMap<Key, Held>,
    calls: Vec<Call>,
}

/// The label of an answer.
fn label<T>(answer: &Result<T, Refusal>) -> &'static str {
    match answer {
        Ok(_) => "ok",
        Err(Refusal::Unavailable(_)) => "unavailable",
        Err(Refusal::Invalid(_)) => "invalid",
        Err(Refusal::Precondition(_)) => "precondition",
    }
}

/// A path segment made of `text`: its letters, digits, `.`, `_` and `-`, every other byte `_`.
fn segment(text: &str) -> String {
    text.chars()
        .map(
            |c| match c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                true => c,
                false => '_',
            },
        )
        .collect()
}

impl Store {
    /// A store serving, with the placement `limit` met at `refuse_at`.
    pub(crate) fn new(limit: usize, refuse_at: RefuseAt) -> Store {
        Store {
            limit,
            refuse_at,
            mode: Mode::Serving,
            sources: BTreeMap::new(),
            calls: Vec::new(),
        }
    }

    /// Forgets every source and call, serves, and meets `limit` at `refuse_at`.
    pub(crate) fn reset(&mut self, limit: usize, refuse_at: RefuseAt) {
        *self = Store::new(limit, refuse_at);
    }

    /// Forgets every source, as a provider that restarted at `at_ms`; the calls stay, and the
    /// restart is listed among them (`<at_ms> Restart ok`).
    pub(crate) fn restart(&mut self, at_ms: u64) {
        self.sources.clear();
        let _ = self.record(at_ms, "Restart", Ok(()));
    }

    /// Answers in `mode` from now on.
    pub(crate) fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
    }

    /// Records the call of `method` that came at `at_ms` and returns its `answer`.
    fn record<T>(
        &mut self,
        at_ms: u64,
        method: &'static str,
        answer: Result<T, Refusal>,
    ) -> Result<T, Refusal> {
        let answer_label = label(&answer);
        self.calls.push(Call {
            at_ms,
            method,
            answer: answer_label,
        });
        answer
    }

    /// The placements alive.
    fn placed(&self) -> usize {
        self.sources.values().filter(|held| held.placed).count()
    }

    /// The refusal at the limit.
    fn too_many(&self) -> Refusal {
        Refusal::Unavailable(format!(
            "the tree was not made: too many placements: {} are alive",
            self.limit
        ))
    }

    /// The refusal of a store that answers nothing.
    fn down() -> Refusal {
        Refusal::Unavailable("the store is unavailable".to_owned())
    }

    /// `Prepare` of `key` with `reference`, which came at `at_ms`.
    pub(crate) fn prepare(&mut self, at_ms: u64, key: Key, reference: &str) -> Result<(), Refusal> {
        let answer = if self.mode == Mode::Unavailable {
            Err(Store::down())
        } else if reference.starts_with(REFUSED_PREFIX) {
            Err(Refusal::Invalid(format!(
                "the ref {reference} cannot be read"
            )))
        } else if let Some(held) = self.sources.get(&key) {
            match held.reference == reference {
                true => Ok(()),
                false => Err(Refusal::Precondition(format!(
                    "the source is prepared with the ref {}",
                    held.reference
                ))),
            }
        } else if self.refuse_at == RefuseAt::Prepare && self.placed() >= self.limit {
            Err(self.too_many())
        } else {
            let held = Held {
                reference: reference.to_owned(),
                placed: self.refuse_at == RefuseAt::Prepare,
            };
            self.sources.insert(key, held);
            Ok(())
        };
        self.record(at_ms, "Prepare", answer)
    }

    /// `Mount` of `key`, which came at `at_ms`; `waited` when it has waited the whole wait of a
    /// stalled store. A [`Mounted::Wait`] is not recorded: the call is answered later.
    pub(crate) fn mount(
        &mut self,
        at_ms: u64,
        key: &Key,
        waited: bool,
    ) -> Result<Mounted, Refusal> {
        let path = format!("{}/{}/{}", segment(&key.0), segment(&key.1), key.2);
        let full = (self.placed() >= self.limit).then(|| self.too_many());
        let stalled = self.mode == Mode::Stalled;
        let answer = match (self.mode, self.sources.get_mut(key), full) {
            (Mode::Unavailable, _, _) => Err(Store::down()),
            (_, None, _) => Err(Refusal::Unavailable(
                "the source is not prepared here".to_owned(),
            )),
            (_, Some(held), _) if held.placed => Ok(Mounted::Placed(path)),
            _ if stalled && !waited => return Ok(Mounted::Wait),
            _ if stalled => Err(Refusal::Unavailable("the tree is not ready".to_owned())),
            (_, Some(_), Some(full)) => Err(full),
            (_, Some(held), None) => {
                held.placed = true;
                Ok(Mounted::Placed(path))
            }
        };
        self.record(at_ms, "Mount", answer)
    }

    /// The answer to a `Mount` that came at `at_ms` and was waiting when the provider stopped:
    /// `UNAVAILABLE`, recorded.
    pub(crate) fn cut_mount(&mut self, at_ms: u64) -> Refusal {
        let cut = Refusal::Unavailable("the provider is restarting".to_owned());
        let recorded = self.record::<()>(at_ms, "Mount", Err(cut));
        recorded.err().unwrap_or_else(Store::down)
    }

    /// `Release` of `key`, which came at `at_ms`.
    pub(crate) fn release(&mut self, at_ms: u64, key: &Key) -> Result<(), Refusal> {
        let answer = match self.mode {
            Mode::Unavailable => Err(Store::down()),
            _ => {
                self.sources.remove(key);
                Ok(())
            }
        };
        self.record(at_ms, "Release", answer)
    }

    /// `Sweep` of the sources of `agent` whose run is not in `keep`, which came at `at_ms`:
    /// how many it released.
    pub(crate) fn sweep(
        &mut self,
        at_ms: u64,
        agent: &str,
        keep: &[String],
    ) -> Result<u32, Refusal> {
        let answer = match self.mode {
            Mode::Unavailable => Err(Store::down()),
            _ => {
                let before = self.sources.len();
                self.sources
                    .retain(|(owner, run, _), _| owner != agent || keep.contains(run));
                Ok(u32::try_from(before - self.sources.len()).unwrap_or(u32::MAX))
            }
        };
        self.record(at_ms, "Sweep", answer)
    }

    /// Every call answered, one line each: `<at_ms> <method> <answer>`, in the order answered.
    pub(crate) fn calls(&self) -> String {
        let mut text = String::new();
        for call in &self.calls {
            let _ = writeln!(text, "{} {} {}", call.at_ms, call.method, call.answer);
        }
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(run: &str, index: u32) -> Key {
        ("host-1".to_owned(), run.to_owned(), index)
    }

    fn placed(run: &str, index: u32) -> Result<Mounted, Refusal> {
        Ok(Mounted::Placed(format!("host-1/{run}/{index}")))
    }

    fn unavailable(why: &str) -> Result<(), Refusal> {
        Err(Refusal::Unavailable(why.to_owned()))
    }

    const FULL: &str = "the tree was not made: too many placements: 2 are alive";

    /// Asserts a source's way from Prepare to Mount to Release, a Prepare again with the same
    /// ref and with another, and the calls recorded with their answers.
    #[test]
    fn a_source_is_prepared_placed_and_released() {
        let mut store = Store::new(2, RefuseAt::Mount);
        assert_eq!(store.prepare(1, key("js.a", 0), "cas/l/1"), Ok(()));
        assert_eq!(store.prepare(2, key("js.a", 0), "cas/l/1"), Ok(()));
        assert_eq!(
            store.prepare(3, key("js.a", 0), "cas/l/2"),
            Err(Refusal::Precondition(
                "the source is prepared with the ref cas/l/1".to_owned()
            ))
        );
        assert_eq!(store.mount(4, &key("js.a", 0), false), placed("js.a", 0));
        assert_eq!(store.mount(5, &key("js.a", 0), false), placed("js.a", 0));
        assert_eq!(store.release(6, &key("js.a", 0)), Ok(()));
        assert_eq!(store.release(7, &key("js.a", 0)), Ok(()));
        assert_eq!(
            store.calls(),
            "1 Prepare ok\n2 Prepare ok\n3 Prepare precondition\n4 Mount ok\n5 Mount ok\n\
             6 Release ok\n7 Release ok\n"
        );
    }

    /// Asserts the limit met at Mount: the third placement is refused with tierstore's reason
    /// until a Release frees one, while its Prepare succeeds.
    #[test]
    fn the_limit_is_met_at_mount() {
        let mut store = Store::new(2, RefuseAt::Mount);
        for index in 0..3 {
            assert_eq!(store.prepare(0, key("js.a", index), "cas/l/1"), Ok(()));
        }
        assert_eq!(store.mount(1, &key("js.a", 0), false), placed("js.a", 0));
        assert_eq!(store.mount(1, &key("js.a", 1), false), placed("js.a", 1));
        assert_eq!(
            store.mount(1, &key("js.a", 2), false).map(|_| ()),
            unavailable(FULL)
        );
        assert_eq!(store.release(2, &key("js.a", 0)), Ok(()));
        assert_eq!(store.mount(3, &key("js.a", 2), false), placed("js.a", 2));
    }

    /// Asserts the limit met at Prepare: the tree is placed at Prepare (its Mount answers the
    /// path even while the store stalls), a new source while the limit's placements are alive
    /// is refused, a source already held is not, and a Release lets the new one in.
    #[test]
    fn the_limit_is_met_at_prepare() {
        let mut store = Store::new(1, RefuseAt::Prepare);
        assert_eq!(store.prepare(0, key("js.a", 0), "cas/l/1"), Ok(()));
        store.set_mode(Mode::Stalled);
        assert_eq!(store.mount(0, &key("js.a", 0), false), placed("js.a", 0));
        store.set_mode(Mode::Serving);
        assert_eq!(
            store.prepare(1, key("js.b", 0), "cas/l/1"),
            unavailable("the tree was not made: too many placements: 1 are alive")
        );
        assert_eq!(store.prepare(1, key("js.a", 0), "cas/l/1"), Ok(()));
        assert_eq!(store.release(2, &key("js.a", 0)), Ok(()));
        assert_eq!(store.prepare(3, key("js.b", 0), "cas/l/1"), Ok(()));
    }

    /// Asserts a stalled store: a Mount that would place waits, and is answered not ready once
    /// it has waited; a placed source still mounts; serving again places.
    #[test]
    fn a_stalled_store_holds_mount_then_answers_not_ready() {
        let mut store = Store::new(2, RefuseAt::Mount);
        assert_eq!(store.prepare(0, key("js.a", 0), "cas/l/1"), Ok(()));
        assert_eq!(store.prepare(0, key("js.a", 1), "cas/l/1"), Ok(()));
        assert_eq!(store.mount(0, &key("js.a", 1), false), placed("js.a", 1));
        store.set_mode(Mode::Stalled);
        assert_eq!(store.mount(1, &key("js.a", 0), false), Ok(Mounted::Wait));
        assert_eq!(
            store.mount(1, &key("js.a", 0), true).map(|_| ()),
            unavailable("the tree is not ready")
        );
        assert_eq!(store.mount(2, &key("js.a", 1), false), placed("js.a", 1));
        store.set_mode(Mode::Serving);
        assert_eq!(store.mount(3, &key("js.a", 0), false), placed("js.a", 0));
        assert_eq!(
            store.cut_mount(4),
            Refusal::Unavailable("the provider is restarting".to_owned())
        );
        assert_eq!(
            store.calls(),
            "0 Prepare ok\n0 Prepare ok\n0 Mount ok\n1 Mount unavailable\n2 Mount ok\n3 Mount ok\n4 Mount unavailable\n"
        );
    }

    /// Asserts an unavailable store answers every call unavailable and keeps its sources.
    #[test]
    fn an_unavailable_store_answers_every_call_unavailable() {
        let mut store = Store::new(2, RefuseAt::Mount);
        assert_eq!(store.prepare(0, key("js.a", 0), "cas/l/1"), Ok(()));
        store.set_mode(Mode::Unavailable);
        let down = "the store is unavailable";
        assert_eq!(
            store.prepare(1, key("js.a", 0), "cas/l/1"),
            unavailable(down)
        );
        assert_eq!(
            store.mount(1, &key("js.a", 0), false).map(|_| ()),
            unavailable(down)
        );
        assert_eq!(store.release(1, &key("js.a", 0)), unavailable(down));
        assert_eq!(
            store.sweep(1, "host-1", &[]),
            Err(Refusal::Unavailable(down.to_owned()))
        );
        store.set_mode(Mode::Serving);
        assert_eq!(store.mount(2, &key("js.a", 0), false), placed("js.a", 0));
    }

    /// Asserts a refused ref (`INVALID_ARGUMENT`) and a restart, after which a source must be
    /// prepared again.
    #[test]
    fn a_refused_ref_and_a_restart() {
        let mut store = Store::new(2, RefuseAt::Mount);
        assert_eq!(
            store.prepare(0, key("js.a", 0), "refused/x"),
            Err(Refusal::Invalid(
                "the ref refused/x cannot be read".to_owned()
            ))
        );
        assert_eq!(store.prepare(0, key("js.a", 0), "cas/l/1"), Ok(()));
        store.restart(1);
        assert_eq!(
            store.mount(2, &key("js.a", 0), false).map(|_| ()),
            unavailable("the source is not prepared here")
        );
        assert_eq!(
            store.calls(),
            "0 Prepare invalid\n0 Prepare ok\n1 Restart ok\n2 Mount unavailable\n"
        );
        store.reset(3, RefuseAt::Prepare);
        assert_eq!(store.calls(), "");
        assert_eq!(store.limit, 3);
        assert_eq!(store.refuse_at, RefuseAt::Prepare);
    }

    /// Asserts a sweep releases only the named agent's sources whose run is not kept, and a
    /// path segment of a run with bytes a path must not hold.
    #[test]
    fn a_sweep_releases_the_runs_not_kept() {
        let mut store = Store::new(9, RefuseAt::Mount);
        assert_eq!(store.prepare(0, key("js.a", 0), "r"), Ok(()));
        assert_eq!(store.prepare(0, key("js.b", 0), "r"), Ok(()));
        let other = ("host-2".to_owned(), "js.c".to_owned(), 0);
        assert_eq!(store.prepare(0, other.clone(), "r"), Ok(()));
        assert_eq!(store.sweep(1, "host-1", &["js.b".to_owned()]), Ok(1));
        assert_eq!(store.sources.len(), 2);
        assert!(store.sources.contains_key(&other));
        assert_eq!(segment("js.a/../b c"), "js.a_.._b_c");
    }
}
