//! The rule of a host's cordon and of who holds it, which every place that keeps a cordon
//! applies: the job store's cordon rows, by which a server with the job service decides, and
//! the registry of a server without it.
//!
//! A cordon in force lapses at its `until` and may name its holder ([`Cordon`]). A request
//! names who asks ([`Asker`]):
//!
//! - with no cordon in force, any request places one, until the time it asks for, held by
//!   the holder it names (or by none);
//! - a request that names no holder ([`Asker::Unnamed`]: an operator's call, an agent's update
//!   hook, a requester of an earlier release) extends the cordon in force, never shortening it,
//!   and leaves its holder as it is;
//! - a request of the cordon's own holder ([`Asker::Holder`]) renews it the same way;
//! - a request of another holder leaves the cordon in force as it is, and says so
//!   ([`Cordoning::held_by_other`]): a requester that died is not kept alive by the next one,
//!   so its cordon lapses at the end of its last request;
//! - a take-over ([`Asker::TakeOver`]) makes the cordon in force the asker's, and extends it
//!   as a renewal does.
//!
//! A lift ([`uncordon`]) that names a holder ends only that holder's cordon; one that names
//! none ends any. The time `T` is the deciding clock's, or the time a cordon has left
//! (`Cordon<Duration>`) where a cordon passes from one clock to another.

use crate::CordonHolder;

/// Who asks for a cordon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Asker {
    /// A requester that names no holder: it extends any cordon in force and leaves its holder.
    Unnamed,
    /// The holder named: it places a cordon when none is in force and renews one it holds,
    /// and leaves one that another holds as it is.
    Holder(CordonHolder),
    /// The holder named, taking over the cordon in force whoever holds it.
    TakeOver(CordonHolder),
}

impl Asker {
    /// The holder the request names, if it names one.
    #[must_use]
    pub fn holder(&self) -> Option<&CordonHolder> {
        match self {
            Asker::Unnamed => None,
            Asker::Holder(holder) | Asker::TakeOver(holder) => Some(holder),
        }
    }
}

/// A cordon in force: when it lapses (on the deciding clock, or the time it has left) and who
/// holds it, when a holder placed or took it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cordon<T> {
    /// When the cordon lapses.
    pub until: T,
    /// The holder, when the cordon was placed or taken over by a request that named one.
    pub holder: Option<CordonHolder>,
}

/// What a cordon request does ([`cordon`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cordoning<T> {
    /// The cordon in force after the request.
    pub cordon: Cordon<T>,
    /// Whether a cordon was in force before the request.
    pub already: bool,
    /// Whether the cordon in force is another holder's and was left as it was: the request
    /// changed nothing.
    pub held_by_other: bool,
}

/// What a lift does ([`uncordon`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Uncordoning {
    /// Whether a cordon was in force and the lift ends it.
    pub lifted: bool,
    /// Whether a cordon is in force that another holder holds, so the lift leaves it.
    pub held_by_other: bool,
}

/// What a cordon request of `by`, for a cordon until `wanted`, does to `held`, the cordon in
/// force (none when there is none or it has lapsed): the rule of the module docs.
#[must_use]
pub fn cordon<T: Ord + Copy>(held: Option<&Cordon<T>>, wanted: T, by: &Asker) -> Cordoning<T> {
    let Some(held) = held else {
        let cordon = Cordon {
            until: wanted,
            holder: by.holder().cloned(),
        };
        return Cordoning {
            cordon,
            already: false,
            held_by_other: false,
        };
    };
    let holder = match by {
        Asker::Unnamed => held.holder.clone(),
        Asker::Holder(asker) if held.holder.as_ref() == Some(asker) => held.holder.clone(),
        Asker::Holder(_) => {
            return Cordoning {
                cordon: held.clone(),
                already: true,
                held_by_other: true,
            };
        }
        Asker::TakeOver(asker) => Some(asker.clone()),
    };
    let cordon = Cordon {
        until: held.until.max(wanted),
        holder,
    };
    Cordoning {
        cordon,
        already: true,
        held_by_other: false,
    }
}

/// What a lift that names `holder` (or none) does to `held`, the cordon in force (none when
/// there is none or it has lapsed).
#[must_use]
pub fn uncordon<T>(held: Option<&Cordon<T>>, holder: Option<&CordonHolder>) -> Uncordoning {
    let Some(held) = held else {
        return Uncordoning {
            lifted: false,
            held_by_other: false,
        };
    };
    let ours = holder.is_none_or(|asker| held.holder.as_ref() == Some(asker));
    Uncordoning {
        lifted: ours,
        held_by_other: !ours,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn holder(name: &str) -> CordonHolder {
        CordonHolder::new(name).expect("a holder")
    }

    fn held(until: u64, by: Option<&str>) -> Cordon<u64> {
        Cordon {
            until,
            holder: by.map(holder),
        }
    }

    /// Asserts that with no cordon in force every asker places one until the time it asks
    /// for, held by the holder it names: an unnamed request places an unnamed cordon, and a
    /// take-over of nothing is a placement. It catches a placement that reports a cordon in
    /// force, or that drops or invents a holder.
    #[test]
    fn with_no_cordon_in_force_every_asker_places_one() {
        let placed = |holder_name: Option<&str>| Cordoning {
            cordon: held(10, holder_name),
            already: false,
            held_by_other: false,
        };
        assert_eq!(cordon(None, 10, &Asker::Unnamed), placed(None));
        let by = Asker::Holder(holder("run-a"));
        assert_eq!(cordon(None, 10, &by), placed(Some("run-a")));
        let by = Asker::TakeOver(holder("run-a"));
        assert_eq!(cordon(None, 10, &by), placed(Some("run-a")));
    }

    /// Asserts that the holder's own request renews its cordon to the later of the two
    /// times, never shortening it, and keeps it the holder's. It catches a renewal that
    /// shortens a cordon or reports the holder's own cordon as another's.
    #[test]
    fn the_holder_renews_its_cordon_and_never_shortens_it() {
        let by = Asker::Holder(holder("run-a"));
        let longer = cordon(Some(&held(10, Some("run-a"))), 20, &by);
        assert_eq!(
            longer,
            Cordoning {
                cordon: held(20, Some("run-a")),
                already: true,
                held_by_other: false,
            }
        );
        let shorter = cordon(Some(&held(30, Some("run-a"))), 20, &by);
        assert_eq!(shorter.cordon, held(30, Some("run-a")));
    }

    /// Asserts that a named request finds another holder's cordon, and an unnamed one, and
    /// leaves each exactly as it was: no extension, no change of holder. This is what lets the
    /// cordon of a requester that died lapse at the end of its last request. It catches a
    /// request that extends or takes another holder's cordon.
    #[test]
    fn another_holders_cordon_is_left_as_it_is() {
        let by = Asker::Holder(holder("run-b"));
        for found in [held(10, Some("run-a")), held(10, None)] {
            assert_eq!(
                cordon(Some(&found), 50, &by),
                Cordoning {
                    cordon: found.clone(),
                    already: true,
                    held_by_other: true,
                }
            );
        }
    }

    /// Asserts that an unnamed request extends whatever cordon is in force, never shortening
    /// it, and keeps its holder, as requesters of earlier releases expect. It catches an
    /// unnamed request that takes the cordon away from its holder or shortens it.
    #[test]
    fn an_unnamed_request_extends_any_cordon_and_keeps_its_holder() {
        let extended = cordon(Some(&held(10, Some("run-a"))), 20, &Asker::Unnamed);
        assert_eq!(
            extended,
            Cordoning {
                cordon: held(20, Some("run-a")),
                already: true,
                held_by_other: false,
            }
        );
        let kept = cordon(Some(&held(30, None)), 20, &Asker::Unnamed);
        assert_eq!(kept.cordon, held(30, None));
    }

    /// Asserts that a take-over makes another holder's cordon, or an unnamed one, the
    /// asker's, extended and never shortened. It catches a take-over that is refused, keeps
    /// the old holder, or cuts a longer cordon short.
    #[test]
    fn a_take_over_makes_the_cordon_the_askers() {
        let by = Asker::TakeOver(holder("run-b"));
        let taken = cordon(Some(&held(10, Some("run-a"))), 20, &by);
        assert_eq!(
            taken,
            Cordoning {
                cordon: held(20, Some("run-b")),
                already: true,
                held_by_other: false,
            }
        );
        let taken = cordon(Some(&held(30, None)), 20, &by);
        assert_eq!(taken.cordon, held(30, Some("run-b")));
    }

    /// Asserts each lift: nothing in force lifts nothing; an unnamed lift ends any cordon; a
    /// named lift ends its own holder's cordon and leaves another's, or an unnamed one, saying
    /// so. It catches a holder that lifts another's cordon, and a lift of nothing reported as
    /// another holder's.
    #[test]
    fn a_lift_ends_only_the_cordon_it_may() {
        let nothing = Uncordoning {
            lifted: false,
            held_by_other: false,
        };
        let lifted = Uncordoning {
            lifted: true,
            held_by_other: false,
        };
        let left = Uncordoning {
            lifted: false,
            held_by_other: true,
        };
        let run_a = holder("run-a");
        assert_eq!(uncordon::<u64>(None, Some(&run_a)), nothing);
        assert_eq!(uncordon::<u64>(None, None), nothing);
        assert_eq!(uncordon(Some(&held(10, Some("run-b"))), None), lifted);
        assert_eq!(
            uncordon(Some(&held(10, Some("run-a"))), Some(&run_a)),
            lifted
        );
        assert_eq!(uncordon(Some(&held(10, Some("run-b"))), Some(&run_a)), left);
        assert_eq!(uncordon(Some(&held(10, None)), Some(&run_a)), left);
    }

    /// Asserts the holder an asker names: none for an unnamed request, the holder for the
    /// other two. It catches a take-over that drops its holder.
    #[test]
    fn an_asker_names_its_holder() {
        let run_a = holder("run-a");
        assert_eq!(Asker::Unnamed.holder(), None);
        assert_eq!(Asker::Holder(run_a.clone()).holder(), Some(&run_a));
        assert_eq!(Asker::TakeOver(run_a.clone()).holder(), Some(&run_a));
    }

    /// Asserts that a holder is an id: 1 to 63 characters of `[a-z0-9._-]`, refused
    /// otherwise, naming its kind. It catches a holder that could carry text a log line or a
    /// key of the store cannot hold.
    #[test]
    fn a_holder_is_an_id() {
        assert_eq!(holder("cordon-1a2b.x_y").as_str(), "cordon-1a2b.x_y");
        assert_eq!(holder("run-a").to_string(), "run-a");
        for bad in ["", "Run-A", "a b", &"a".repeat(64)] {
            let refused = CordonHolder::new(bad).expect_err("refused");
            assert!(
                refused.to_string().starts_with("cordon holder id "),
                "{refused}"
            );
        }
    }
}
