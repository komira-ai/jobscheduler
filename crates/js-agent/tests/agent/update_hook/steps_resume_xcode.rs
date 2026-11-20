//! `resume` and an Xcode installed during a maintenance run (hostkeeper's maintenance-updates
//! design, 5.2): the host facts are read by [`host_facts`] over the files of a Mac, as the
//! agent reads them, so the Xcode builds are part of what a changed host restarts the agent
//! for.

use std::cell::RefCell;

use js_agent::update_hook::host_facts;
use js_agent::xcode::{self, hello_value};

use super::maintain_rig::{Probe, Rig, hello, note_text, succeeded};
use crate::support::{Host, mac_host};
use crate::xcodes::with_installed;

/// The facts line of the Mac `host` with the Xcodes `apps` installed.
fn line_of(host: &Host, apps: &[(&str, &str)]) -> String {
    let files = with_installed(host.files.clone(), apps);
    host_facts(&host.platform, &host.commands, &files).unwrap()
}

/// Asserts that an Xcode installed after the drain, while the host is held and cordoned, is not
/// seen until the resume: the drain notes the facts with the old build only, nothing restarts
/// the agent and the cordon stays until the resume; the resume then stops and starts the unit
/// before it lifts the cordon, and the hello an agent started then would make (the host facts
/// and the `xcode` capability over the same files) lists both builds. It catches an Xcode that
/// does not change the facts, so the agent would go on with a hello that lacks it, and a
/// restart that waits for nothing the install changed.
#[test]
fn an_xcode_installed_in_a_maintenance_run_is_seen_only_after_the_resume_restarts_the_agent() {
    let rig = Rig::new("hook-resume-xcode");
    let host = mac_host();
    let apps = RefCell::new(vec![("Xcode-16.2.app", "16C5032a")]);
    let probe = Probe::new(&rig);
    probe.read_facts_with(|| {
        let files = with_installed(host.files.clone(), &apps.borrow());
        host_facts(&host.platform, &host.commands, &files)
    });

    let old = line_of(&host, &[("Xcode-16.2.app", "16C5032a")]);
    assert!(old.ends_with(" xcode=16C5032a"), "{old}");
    assert_eq!(probe.drain(&rig.params), succeeded());
    assert_eq!(rig.hold_text(), Some(note_text(false, Some(&old))));
    let expiry = rig.expiry();
    assert!(expiry.is_some());
    let drained = probe.changes();
    assert_eq!(
        drained.iter().filter(|c| c.starts_with("units.")).count(),
        0
    );

    apps.borrow_mut().push(("Xcode-16.4.app", "16F6"));
    assert_eq!(probe.changes(), drained);
    assert_eq!(rig.hold_text(), Some(note_text(false, Some(&old))));
    assert_eq!(rig.expiry(), expiry);
    assert!(!rig.hold_text().unwrap().contains("16F6"));

    probe.at_pause(|p, taken| {
        if taken == 1 {
            hello(p);
        }
    });
    assert_eq!(probe.resume(&rig.params), succeeded());
    assert_eq!(
        probe.changes()[drained.len()..],
        [
            "units.stop",
            "units.start",
            "hello",
            "hold.release",
            "lease.uncordon",
            "hold.released"
        ]
    );
    assert_eq!(rig.expiry(), None);

    let both = [("Xcode-16.2.app", "16C5032a"), ("Xcode-16.4.app", "16F6")];
    let new = line_of(&host, &both);
    assert!(new.ends_with(" xcode=16C5032a,16F6"), "{new}");
    let files = with_installed(host.files.clone(), &apps.borrow());
    let (found, _) = xcode::probe(&files);
    assert_eq!(hello_value(&found).0.as_deref(), Some("16C5032a,16F6"));
}
