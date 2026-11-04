//! The helper names a caller configures.

use super::*;

/// Catches the helper names a caller configures not reaching the helpers: with another
/// binary path and other mode arguments, the job's argv, the bind of the host binary,
/// and the mount points of the helper root carry exactly those and none of the defaults.
#[test]
fn configured_helper_names_are_the_only_names_in_the_helpers() {
    let modes = helper::HelperModes {
        give: "give".to_owned(),
        whole: "--all".to_owned(),
        unpack: "unpack".to_owned(),
        supervise: "supervise".to_owned(),
        binary: "/other/bin/host".to_owned(),
    };
    let give = helper::give_job(&modes, Path::new("/d"), "1:1", None, &[Path::new("/a")]);
    assert_eq!(
        give.args,
        ["/other/bin/host", "give", "1:1", "-", "--all", "/t/0"]
    );
    let unpack = helper::unpack_job(
        &modes,
        Path::new("/d"),
        Path::new("/s"),
        Path::new("/b"),
        &[],
    );
    assert_eq!(unpack.args[..2], ["/other/bin/host", "unpack"]);
    let ids = DaemonIds {
        uid: 990,
        gid: 991,
        sub_uid: 100_000,
        sub_gid: 200_000,
    };
    let config = helper::config(Path::new("/r"), &ids, Mapping::Setup, "/x/h", &give);
    let text = serde_json::to_string(&config).expect("json");
    assert!(
        text.contains(r#""destination":"/other/bin/host""#),
        "{text}"
    );
    assert!(!text.contains("/js/daemon"), "{text}");
    let dir = scratch("modes-root");
    helper::prepare_rootfs(&dir, &modes.binary).expect("made");
    assert!(dir.join("other/bin/host").is_file());
    assert!(!dir.join("js").exists());
}
