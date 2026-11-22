//! The kill points of the step rig, and the state directory check that passes whoever runs the
//! test.

use std::cell::Cell;
use std::io;
use std::os::fd::BorrowedFd;
use std::path::Path;
use std::rc::Rc;

use js_agent::update_hook::{DirStat, Gate};

/// A state directory that is as the hook requires, whoever runs the test.
pub fn as_root(_: &Path, _: BorrowedFd<'_>) -> io::Result<DirStat> {
    Ok(DirStat {
        is_symlink: false,
        is_dir: true,
        uid: 0,
        gid: 0,
        mode: 0o700,
    })
}

/// The kill points: counts them, and panics at the one it is armed for (a process killed
/// there, from the point of view of the step: nothing after it runs).
#[derive(Default)]
pub struct Kill {
    points: Cell<u32>,
    at: Cell<Option<u32>>,
    agent: Cell<bool>,
}

impl Kill {
    pub fn points(&self) -> u32 {
        self.points.get()
    }

    /// Counts from zero again and dies at the `n`th point of the next run.
    pub fn arm(&self, n: u32) {
        self.points.set(0);
        self.at.set(Some(n));
    }

    /// Runs `work`, which is the agent's and not the hook's (a process of its own, that the
    /// hook's death does not stop half way): its changes to the world pass no kill point.
    pub fn as_agent<T>(&self, work: impl FnOnce() -> T) -> T {
        self.agent.set(true);
        let done = work();
        self.agent.set(false);
        done
    }

    /// Counts from zero again and never dies.
    pub fn disarm(&self) {
        self.points.set(0);
        self.at.set(None);
    }
}

impl Gate for Kill {
    fn point(&self) {
        if self.agent.get() {
            return;
        }
        let n = self.points.get() + 1;
        self.points.set(n);
        assert!(self.at.get() != Some(n), "killed at kill point {n}");
    }
}

pub(super) struct Shared(pub(super) Rc<Kill>);

impl Gate for Shared {
    fn point(&self) {
        self.0.point();
    }
}
