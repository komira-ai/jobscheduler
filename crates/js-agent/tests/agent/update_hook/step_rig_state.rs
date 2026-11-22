//! What the tests of `cutover` read back from a [`Rig`] after a step: the end state, the session
//! before the stop and the record's marks.

use js_agent::update_hook::Units as _;

use super::step_rig::Rig;

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
