//! More of the step tests' rig, apart so that `step_rig.rs` stays short: the release link
//! reached through a symbolic link, as on a Mac, and what the end of a step leaves to compare.

use js_agent::update_hook::Units;

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

/// Everything a `cutover` leaves behind, without the directories of the rig: the record, the
/// link, the unit and the cordon.
pub fn end_state(rig: &Rig) -> serde_json::Value {
    let unit = rig.world.show(&rig.unit).unwrap();
    let state = serde_json::json!({
        "record": rig.record(),
        "link": rig.world.resolved_target(&rig.link).unwrap(),
        "unit": format!("{} {}", unit.active, unit.sub),
        "cordon": rig.cordon_expiry(),
    });
    let root = rig.tmp.path().display().to_string();
    serde_json::from_str(&state.to_string().replace(&root, "<root>")).unwrap()
}

/// The session of the host as the server showed it just before the stop, as the record on disk
/// holds it.
pub fn pre_stop(rig: &Rig) -> Option<u64> {
    rig.record().expect("a record").pre_stop_session_ms
}

/// The `stop_begun`, `link_moved` and `start_issued` marks on disk.
pub fn marks(rig: &Rig) -> [bool; 3] {
    let record = rig.record().unwrap();
    [record.stop_begun, record.link_moved, record.start_issued]
}
