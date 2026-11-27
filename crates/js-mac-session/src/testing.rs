//! What the spawner's tests start as a process: this test binary, run through a link
//! named for js-testkit's act verbs.

use std::path::PathBuf;

// This test binary answers as js-testkit's `kbf-act` when it is run through a link of
// that name: the program the tests start.
js_testkit::act_test_hook!();

/// The path of a link to this test binary named `kbf-act`, made once: run through it,
/// the binary runs js-testkit's act steps (`js_testkit::act`) instead of the tests.
pub(crate) fn act() -> String {
    static LINK: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    LINK.get_or_init(|| {
        // Beside the test binary, one link for every build of it (a directory made per run
        // would never be removed: nothing runs when a process ends). The link is made
        // anew by renaming a new one over the old, so a run that is using the old one
        // never finds it missing.
        let exe = std::env::current_exe().unwrap();
        let dir = exe.parent().unwrap().join("kbf-mac-session-act");
        std::fs::create_dir_all(&dir).unwrap();
        let link = dir.join(js_testkit::act::NAME);
        let new = dir.join(format!("new-{}", std::process::id()));
        let _ = std::fs::remove_file(&new);
        js_testkit::act::link_self(&new).unwrap();
        std::fs::rename(&new, &link).unwrap();
        link
    })
    .display()
    .to_string()
}

/// `act()` and then `steps`, split at spaces: the argument vector of a process that
/// runs those act steps.
pub(crate) fn act_argv(steps: &str) -> Vec<String> {
    std::iter::once(act())
        .chain(steps.split(' ').map(str::to_owned))
        .collect()
}
