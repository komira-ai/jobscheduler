//! Placement of a job that names an Xcode build: kept apart from `jobs_tests.rs`, which is
//! near its line limit, over the same rig.

use super::*;

/// Catches: a job that names an Xcode build (the capability `xcode`, as a submit records it)
/// placed on a host whose agent lists other builds, or none, or one that matches only on the
/// whole joined value. Of `a`, which reports the builds `16C5032a,16E140` in the one value a
/// hello holds, and `b`, which reports only `15F31d`, a job for `16E140` is placed on `a`; and
/// when `a` lists only another build and `b` none, it stays queued.
#[test]
fn a_job_that_names_an_xcode_is_placed_only_on_a_host_that_lists_the_build() {
    let needs_16e140 = |spec: &mut JobSpec| {
        let caps = &mut spec.constraints.capabilities;
        caps.insert("xcode".to_owned(), "16E140".to_owned());
    };
    let rig = Rig::reporting([&[("xcode", "15F31d")], &[("xcode", "16C5032a,16E140")]]);
    let id = share_with(&rig, "j", needs_16e140);
    assert_eq!(placed_on(&rig, &id), Some(host("b")));

    let rig = Rig::reporting([&[("xcode", "16C5032a,16E140")], &[("xcode", "15F31d")]]);
    let id = share_with(&rig, "j", needs_16e140);
    assert_eq!(placed_on(&rig, &id), Some(host("a")));

    let rig = Rig::reporting([&[("xcode", "16C5032a,15F31d")], &[]]);
    let id = share_with(&rig, "j", needs_16e140);
    assert_eq!(placed_on(&rig, &id), None);
    assert_eq!(read(&rig, &id).state, JobState::Queued);
}
