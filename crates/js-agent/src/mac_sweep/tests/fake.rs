//! A process table in memory: a list of processes, a kill that removes a pid (at the first
//! kill, the second, or never), children that appear when a pid is killed (a fork between
//! the listing and the kill), a listing that fails on a given call, and a record of every
//! listing, kill and pause.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io;

use super::super::{Proc, ProcessTable};

/// How a process answers a kill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Death {
    /// It ends at the first kill (every process not named otherwise).
    First,
    /// It ends at the second kill only.
    Second,
    /// It never ends.
    Never,
}

#[derive(Debug, Default)]
struct State {
    procs: Vec<Proc>,
    deaths: BTreeMap<i32, Death>,
    forks: BTreeMap<i32, Vec<Proc>>,
    failing: Option<usize>,
    listings: Vec<bool>,
    kills: Vec<i32>,
    pauses: usize,
}

/// The fake table.
#[derive(Debug, Default)]
pub(super) struct FakeTable {
    state: RefCell<State>,
}

impl FakeTable {
    /// A table holding `procs`.
    pub(super) fn new(procs: Vec<Proc>) -> Self {
        Self {
            state: RefCell::new(State {
                procs,
                ..State::default()
            }),
        }
    }

    /// `pid` answers a kill as `death` says.
    pub(super) fn dying(self, pid: i32, death: Death) -> Self {
        self.state.borrow_mut().deaths.insert(pid, death);
        self
    }

    /// When `pid` is killed, `children` appear first: they were forked after the listing
    /// that found `pid` and before its kill.
    pub(super) fn forking(self, pid: i32, children: Vec<Proc>) -> Self {
        self.state.borrow_mut().forks.insert(pid, children);
        self
    }

    /// The listing of number `call` (0 for the first) fails.
    pub(super) fn failing_at(self, call: usize) -> Self {
        self.state.borrow_mut().failing = Some(call);
        self
    }

    /// The pids in the table now, in their order.
    pub(super) fn pids(&self) -> Vec<i32> {
        self.state.borrow().procs.iter().map(|p| p.pid).collect()
    }

    /// The processes in the table now.
    pub(super) fn procs(&self) -> Vec<Proc> {
        self.state.borrow().procs.clone()
    }

    /// The pids killed, in order, once per kill.
    pub(super) fn kills(&self) -> Vec<i32> {
        self.state.borrow().kills.clone()
    }

    /// Each listing's `with_asid`, in order.
    pub(super) fn listings(&self) -> Vec<bool> {
        self.state.borrow().listings.clone()
    }

    /// How many times the sweep paused.
    pub(super) fn pauses(&self) -> usize {
        self.state.borrow().pauses
    }
}

impl ProcessTable for FakeTable {
    fn list(&self, with_asid: bool) -> io::Result<Vec<Proc>> {
        let mut state = self.state.borrow_mut();
        let call = state.listings.len();
        state.listings.push(with_asid);
        if state.failing == Some(call) {
            return Err(io::Error::other("the table is gone"));
        }
        Ok(state
            .procs
            .iter()
            .map(|p| Proc {
                asid: p.asid.filter(|_| with_asid),
                ..p.clone()
            })
            .collect())
    }

    fn kill(&self, pid: i32) {
        let mut state = self.state.borrow_mut();
        let earlier = state.kills.iter().filter(|&&killed| killed == pid).count();
        state.kills.push(pid);
        if let Some(children) = state.forks.remove(&pid) {
            state.procs.extend(children);
        }
        let ends = match state.deaths.get(&pid).copied().unwrap_or(Death::First) {
            Death::First => true,
            Death::Second => earlier >= 1,
            Death::Never => false,
        };
        if ends {
            state.procs.retain(|p| p.pid != pid);
        }
    }

    fn pause(&self) {
        self.state.borrow_mut().pauses += 1;
    }
}
