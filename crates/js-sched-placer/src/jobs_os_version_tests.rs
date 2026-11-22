//! Placement of a job that asks for a least operating-system version: kept apart from
//! `jobs_tests.rs`, which is near its line limit, over the same rig.

use super::*;

/// Catches: a job that asks for a least `os_version` (the capability, as a submit records
/// it) left queued on every host because the scheduler does not read the key, placed on a
/// host of an earlier major, or kept off a host of a later one. Of `a`, which reports 26, and
/// `b`, which reports 27, a job asking for 27 is placed on `b`; with `a` on 27 and `b` on 26 it
/// is placed on `a`; a job asking for 26 is placed when the only host reporting a version is
/// on 27; and when one host reports 26 and the other none, a job asking for 27 stays queued.
#[test]
fn a_job_that_asks_for_an_os_version_is_placed_only_on_a_host_of_that_major_or_later() {
    let needs = |version: &'static str| {
        move |spec: &mut JobSpec| {
            let caps = &mut spec.constraints.capabilities;
            caps.insert("os_version".to_owned(), version.to_owned());
        }
    };
    let rig = Rig::reporting([&[("os_version", "26")], &[("os_version", "27")]]);
    let id = share_with(&rig, "j", needs("27"));
    assert_eq!(placed_on(&rig, &id), Some(host("b")));

    let rig = Rig::reporting([&[("os_version", "27")], &[("os_version", "26")]]);
    let id = share_with(&rig, "j", needs("27"));
    assert_eq!(placed_on(&rig, &id), Some(host("a")));

    let rig = Rig::reporting([&[], &[("os_version", "27")]]);
    let id = share_with(&rig, "j", needs("26"));
    assert_eq!(placed_on(&rig, &id), Some(host("b")));

    let rig = Rig::reporting([&[("os_version", "26")], &[]]);
    let id = share_with(&rig, "j", needs("27"));
    assert_eq!(placed_on(&rig, &id), None);
    assert_eq!(read(&rig, &id).state, JobState::Queued);
}
