//! The fake runtime's script of a start's file system (layers and mounts beyond the image):
//! a child of `fake`, so it sets the fake runtime's state as the runtime reads it.

use std::path::Path;

use js_agent::lifecycle::{FsPrepared, JobFs};

use super::{FakeRuntime, FsScript};

impl FakeRuntime {
    /// From now on the runtime gives a job layers and mounts (`layered_rootfs`), binds no
    /// mount of a named root, prepares each run's file system as the test scripts it
    /// (ready by default), and logs each call of it.
    pub fn give_layers(&self) {
        self.state.borrow_mut().fs = Some(FsScript::default());
    }

    pub(super) fn script(&self) -> std::cell::RefMut<'_, FsScript> {
        std::cell::RefMut::map(self.state.borrow_mut(), |state| {
            state.fs.as_mut().expect("give_layers first")
        })
    }

    /// From now on the runtime takes a mount source below `below` itself
    /// (`owns_host_path`), as a Mac's native runtime takes its put targets.
    pub fn own_host_paths(&self, below: &Path) {
        self.script().owned = Some(below.to_owned());
    }

    /// `resolve_fs` answers `answer` for `run_id` from now on.
    pub fn resolve_answers(&self, run_id: &str, answer: FsPrepared) {
        self.script().resolved.insert(run_id.to_owned(), answer);
    }

    /// The file system `resolve_fs` was last asked about for `run_id`: what its lookups were
    /// to open.
    pub fn looked_up(&self, run_id: &str) -> Option<JobFs> {
        self.state.borrow().looked_up.get(run_id).cloned()
    }

    /// `assemble_fs` answers `answer` for `run_id` from now on.
    pub fn assembly_answers(&self, run_id: &str, answer: FsPrepared) {
        self.script().assembled.insert(run_id.to_owned(), answer);
    }

    /// The layers, mounts and host ports of each `check_job_fs` call.
    pub fn job_fs_checks(&self) -> Vec<(usize, usize, usize)> {
        let state = self.state.borrow();
        state
            .fs
            .as_ref()
            .map(|fs| fs.job_fs_checks.clone())
            .unwrap_or_default()
    }

    /// `check_job_fs` refuses with `code` and `why` from now on.
    pub fn refuse_job_fs(&self, code: &'static str, why: &str) {
        self.script().job_fs_refusal = Some((code, why.to_owned()));
    }

    /// `check_job_fs` accepts again.
    pub fn accept_job_fs(&self) {
        self.script().job_fs_refusal = None;
    }

    /// `check_layer_image` refuses `image`.
    pub fn refuse_layer_image(&self, image: &str) {
        self.script().refused_images.push(image.to_owned());
    }

    /// The sources of `run_id` do not answer once it has ended.
    pub fn sources_gone(&self, run_id: &str) {
        self.script().unreadable.push(run_id.to_owned());
    }

    /// The runs' mounts are under `dir` (`mounts_of`).
    pub fn mount_in(&self, dir: &Path) {
        self.script().mounts = Some(dir.to_owned());
    }
}
