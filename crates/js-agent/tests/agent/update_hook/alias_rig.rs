//! More of the step tests' rig, apart so that `step_rig.rs` stays short: the release link
//! reached through a symbolic link, as on a Mac.

use super::step_rig::Rig;

impl Rig {
    /// The updater's act as hostkeeper does it on a Mac: the release link is moved to the
    /// release directory `version` (`1.4.0`), reached through a symbolic link to the releases
    /// directory (`<tmp>/aliased -> releases`, as `/var` is `/private/var`), while the params
    /// name every release by its real path.
    ///
    /// # Panics
    /// A file cannot be written.
    pub fn link_through_an_alias(&self, version: &str) {
        let aliased = self.tmp.path().join("aliased");
        if !aliased.exists() {
            std::os::unix::fs::symlink("releases", &aliased).unwrap();
            self.world.register_release(&aliased.join("1.4.0"), "1.4.0");
            self.world.register_release(&aliased.join("1.3.0"), "1.3.0");
        }
        js_update_hook::swap_link(
            &self.tmp.path().join("control/link/current"),
            &aliased.join(version),
        )
        .unwrap();
    }
}
