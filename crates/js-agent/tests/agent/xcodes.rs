//! The Xcodes a Mac reports: found by the `ProductBuildVersion` of each app's `version.plist`
//! in `/Applications`, joined into the hello's `xcode` capability, and never changed by the
//! probe.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use js_agent::xcode::{APPLICATIONS, Xcode, hello_value, probe, probe_in};
use js_agent::{FileReader, SystemFiles};
use js_agent_wire::limits::MAX_CAPABILITY_VALUE_LEN;
use serde_json::json;

use crate::support::{FakeFiles, Scratch, code, linux_host, mac_host, run_scripted, welcome};
use crate::xcode_survey::{applications, developer, owner, version_text};

/// The `Contents/version.plist` of an Xcode of `build`, as Xcode writes it.
pub fn version_plist(build: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n<dict>\n\
         \t<key>CFBundleShortVersionString</key>\n\t<string>16.2</string>\n\
         \t<key>ProductBuildVersion</key>\n\t<string>{build}</string>\n\
         </dict>\n</plist>\n"
    )
}

/// `files` with `/Applications` holding the apps `(directory name, build)`, each with its
/// plist.
pub fn with_installed(files: FakeFiles, apps: &[(&str, &str)]) -> FakeFiles {
    let names: Vec<&str> = apps.iter().map(|(name, _)| *name).collect();
    apps.iter().fold(
        files.with_dir(APPLICATIONS, &names),
        |files, (name, build)| {
            files.with(
                &format!("{APPLICATIONS}/{name}/Contents/version.plist"),
                &version_plist(build),
            )
        },
    )
}

/// Only `/Applications`, holding the apps `(directory name, build)`, each with its plist.
pub fn installed(apps: &[(&str, &str)]) -> FakeFiles {
    with_installed(FakeFiles::default(), apps)
}

fn app(name: &str) -> PathBuf {
    Path::new(APPLICATIONS).join(name)
}

fn builds(found: &[Xcode]) -> Vec<&str> {
    found.iter().map(|xcode| xcode.build.as_str()).collect()
}

/// Two installed Xcodes are found with their builds and apps, in the order of the builds
/// whatever the order the directory lists them in. It catches an Xcode that is left out, and
/// a hello that lists them in directory order.
#[test]
fn two_installed_xcodes_are_found_by_build_with_their_apps() {
    let files = installed(&[("Xcode-16.4.app", "16F6"), ("Xcode-16.2.app", "16C5032a")]);
    let (found, warnings) = probe(&files);
    assert_eq!(warnings, Vec::<String>::new());
    assert_eq!(
        found,
        [
            Xcode {
                build: "16C5032a".to_owned(),
                app: app("Xcode-16.2.app")
            },
            Xcode {
                build: "16F6".to_owned(),
                app: app("Xcode-16.4.app")
            },
        ]
    );
}

/// An Xcode whose directory is not `Xcode-<version>.app` is probed by its plist, and the
/// order is the builds', not the names'. It catches a probe that relies on the directory name.
#[test]
fn xcodes_not_named_by_version_are_probed_by_their_plists() {
    let files = installed(&[
        ("Xcode_beta.app", "17A5241e"),
        ("Xcode.app", "16F6"),
        ("Xcode-9.app", "17A100"),
    ]);
    let (found, warnings) = probe(&files);
    assert_eq!(warnings, Vec::<String>::new());
    assert_eq!(builds(&found), ["16F6", "17A100", "17A5241e"]);
    assert_eq!(found[0].app, app("Xcode.app"));
    assert_eq!(found[2].app, app("Xcode_beta.app"));
}

/// An entry that is not an app named `Xcode*.app` is never looked into, even where it would
/// have a plist with a valid build. It catches a probe that reads every application.
#[test]
fn only_xcode_apps_are_read() {
    let files = installed(&[
        ("Xcode-16.2.app", "16C5032a"),
        ("Safari.app", "17A1"),
        ("XcodeNotes.txt", "17A2"),
        ("Xcode.app.zip", "17A3"),
    ]);
    let (found, warnings) = probe(&files);
    assert_eq!(warnings, Vec::<String>::new());
    assert_eq!(builds(&found), ["16C5032a"]);
}

/// An app whose plist is absent, not a plist, without a `ProductBuildVersion` or with a build
/// that is not a build id (a comma would split the capability) is left out with a warning that
/// names the app and why, and the others are still found. It catches a probe that fails whole
/// for one bad app, and a build that could break the capability value.
#[test]
fn an_app_whose_plist_cannot_be_used_is_skipped_with_a_warning() {
    let files = installed(&[("Xcode-16.2.app", "16C5032a"), ("Xcode-comma.app", "16C,5")])
        .with_dir(
            APPLICATIONS,
            &[
                "Xcode-16.2.app",
                "Xcode-comma.app",
                "Xcode-garbled.app",
                "Xcode-gone.app",
                "Xcode-nokey.app",
            ],
        )
        .with(
            "/Applications/Xcode-garbled.app/Contents/version.plist",
            "<plist version=\"1.0\"><dict><key>",
        )
        .with(
            "/Applications/Xcode-nokey.app/Contents/version.plist",
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\"><dict>\
         <key>CFBundleName</key><string>Xcode</string></dict></plist>",
        );
    let (found, warnings) = probe(&files);
    assert_eq!(builds(&found), ["16C5032a"]);
    assert_eq!(warnings.len(), 4, "{warnings:?}");
    assert_eq!(
        warnings[0],
        "/Applications/Xcode-comma.app: ProductBuildVersion \"16C,5\" is not a build id; not \
         reported as an Xcode"
    );
    assert!(
        warnings[1]
            .starts_with("/Applications/Xcode-garbled.app: version.plist cannot be parsed (")
            && warnings[1].ends_with("); not reported as an Xcode"),
        "{}",
        warnings[1]
    );
    assert_eq!(
        warnings[2],
        "/Applications/Xcode-gone.app: version.plist cannot be read \
         (/Applications/Xcode-gone.app/Contents/version.plist: No such file or directory); not \
         reported as an Xcode"
    );
    assert_eq!(
        warnings[3],
        "/Applications/Xcode-nokey.app: version.plist has no ProductBuildVersion; not reported \
         as an Xcode"
    );
}

/// A plist whose top level is not a dictionary names no build. It catches a probe that
/// takes a build from anywhere in the file.
#[test]
fn a_plist_that_is_not_a_dictionary_has_no_build() {
    let files = FakeFiles::default()
        .with_dir(APPLICATIONS, &["Xcode.app"])
        .with(
            "/Applications/Xcode.app/Contents/version.plist",
            "<plist version=\"1.0\"><array><string>16F6</string></array></plist>",
        );
    let (found, warnings) = probe(&files);
    assert!(found.is_empty());
    assert_eq!(
        warnings,
        [
            "/Applications/Xcode.app: version.plist has no ProductBuildVersion; not reported as \
             an Xcode"
        ]
    );
}

/// Two apps of one build (a link `Xcode.app` to `Xcode-16.2.app`) are one Xcode, the first by
/// name, with no warning. It catches a build listed twice in the hello.
#[test]
fn one_build_installed_twice_is_one_xcode() {
    let files = installed(&[("Xcode.app", "16C5032a"), ("Xcode-16.2.app", "16C5032a")]);
    let (found, warnings) = probe(&files);
    assert_eq!(warnings, Vec::<String>::new());
    assert_eq!(
        found,
        [Xcode {
            build: "16C5032a".to_owned(),
            app: app("Xcode-16.2.app")
        }]
    );
}

/// A Mac without `/Applications` has no Xcode and no warning; one whose `/Applications` cannot
/// be listed has none and a warning that says why. It catches a warning on every Mac without
/// Xcode, and a listing error taken for the absence of Xcode.
#[test]
fn a_missing_applications_directory_is_quiet_and_an_unlistable_one_warns() {
    assert_eq!(probe(&FakeFiles::default()), (Vec::new(), Vec::new()));
    let denied = FakeFiles::default().with_unlistable_dir(APPLICATIONS);
    assert_eq!(
        probe(&denied),
        (
            Vec::new(),
            vec![
                "/Applications: cannot be listed (permission denied); reporting no Xcode"
                    .to_owned()
            ]
        )
    );
}

/// The hello value is the builds joined with a comma; with more builds than fit 128 bytes it
/// stops before the limit, keeps the lowest builds and warns with how many it left out; with
/// no builds there is no value. It catches a hello the server would refuse for a long value.
#[test]
fn the_hello_value_stops_before_the_capability_limit_and_warns() {
    let xcode = |build: String| Xcode {
        app: app(&format!("Xcode-{build}.app")),
        build,
    };
    let two = [xcode("16C5032a".to_owned()), xcode("16F6".to_owned())];
    assert_eq!(hello_value(&two), (Some("16C5032a,16F6".to_owned()), None));
    assert_eq!(hello_value(&[]), (None, None));

    let many: Vec<Xcode> = (0..20).map(|n| xcode(format!("16A{n:05}"))).collect();
    let (value, warning) = hello_value(&many);
    let value = value.unwrap();
    let kept: Vec<String> = (0..14).map(|n| format!("16A{n:05}")).collect();
    assert_eq!(value, kept.join(","));
    assert!(value.len() <= MAX_CAPABILITY_VALUE_LEN);
    assert_eq!(
        warning.as_deref(),
        Some("xcode: 6 of 20 builds do not fit in 128 bytes and are left out of the hello")
    );
}

/// A Mac says hello with the build of its Xcode that answers the survey in the capability
/// `xcode`, and logs the warning of an app it could not read, still saying hello. It
/// catches a hello without the Xcode and an unreadable app that stops the agent.
#[test]
fn a_mac_says_hello_with_the_builds_of_its_xcodes() {
    let scratch = Scratch::new("xcode-hello");
    let apps = [
        ("Xcode-16.2.app", "16C5032a"),
        ("Xcode-broken.app", "16C,5"),
    ];
    let (dir, files) = applications(&scratch, &apps);
    let dev = developer(&dir, "Xcode-16.2.app");
    let mut host = mac_host();
    host.applications = dir.clone();
    host.xcode_owner = owner();
    host.files = files;
    host.commands = host
        .commands
        .with(
            &format!("{dev}/usr/bin/xcodebuild -version"),
            Ok(&version_text("16.2", "16C5032a")),
        )
        .with(&format!("{dev}/usr/bin/xcodebuild -license check"), Ok(""))
        .with(
            &format!("{dev}/usr/bin/xcodebuild -checkFirstLaunchStatus"),
            Ok(""),
        )
        .with(
            "/usr/bin/xcrun --find clang",
            Ok(&format!("{dev}/usr/bin/clang\n")),
        );
    let (ran, sent, _) = run_scripted(&[], &host, vec![welcome(1)], 1);
    assert_eq!(ran.status(), code(0), "{}", ran.err);
    assert_eq!(
        sent.borrow()[0].1["capabilities"],
        json!({ "os": "macos", "arch": "arm64", "os_version": "26", "xcode": "16C5032a" })
    );
    let broken = format!(
        "{}/Xcode-broken.app: ProductBuildVersion \"16C,5\" is not a build id",
        dir.display()
    );
    assert!(ran.err.contains(&broken), "{}", ran.err);
}

/// A Mac with no Xcode says hello without an `xcode` key. It catches an empty value, which
/// the server would take as a build.
#[test]
fn a_mac_without_xcode_has_no_xcode_capability() {
    let (ran, sent, _) = run_scripted(&[], &mac_host(), vec![welcome(1)], 1);
    assert_eq!(ran.status(), code(0), "{}", ran.err);
    assert_eq!(sent.borrow()[0].1["capabilities"].get("xcode"), None);
}

/// A Linux host is never asked for its `/Applications`: with Xcode apps in that directory its
/// hello still has no `xcode` key. It catches a probe on every host.
#[test]
fn a_linux_host_is_not_asked_for_its_applications() {
    let mut host = linux_host();
    host.files = with_installed(host.files.clone(), &[("Xcode-16.2.app", "16C5032a")]);
    let (ran, sent, _) = run_scripted(&[], &host, vec![welcome(1)], 1);
    assert_eq!(ran.status(), code(0), "{}", ran.err);
    assert_eq!(sent.borrow()[0].1["capabilities"].get("xcode"), None);
}

/// Every file and directory under `dir` with its bytes (a file) and modification time.
fn snapshot(dir: &Path) -> Vec<(PathBuf, Option<Vec<u8>>, SystemTime)> {
    let mut out = Vec::new();
    let mut names: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    names.sort();
    for path in names {
        let meta = fs::symlink_metadata(&path).unwrap();
        let bytes = meta.is_file().then(|| fs::read(&path).unwrap());
        out.push((path.clone(), bytes, meta.modified().unwrap()));
        if meta.is_dir() {
            out.extend(snapshot(&path));
        }
    }
    out
}

/// The probe over a real directory finds both Xcodes, the one named by version and the one
/// that is not, and leaves the directories, the plists' bytes and every modification time as
/// they were. It catches a probe that writes, renames or removes anything of an installed
/// Xcode.
#[test]
fn the_probe_of_a_real_directory_changes_nothing() {
    let scratch = Scratch::new("xcode-probe");
    let apps = scratch.path("Applications");
    for (name, build) in [("Xcode-16.2.app", "16C5032a"), ("Xcode.app", "16F6")] {
        fs::create_dir_all(apps.join(name).join("Contents")).unwrap();
        scratch.write(
            &format!("Applications/{name}/Contents/version.plist"),
            &version_plist(build),
            0o644,
        );
    }
    let before = snapshot(&apps);
    let (found, warnings) = probe_in(&SystemFiles, &apps);
    assert_eq!(warnings, Vec::<String>::new());
    assert_eq!(
        found,
        [
            Xcode {
                build: "16C5032a".to_owned(),
                app: apps.join("Xcode-16.2.app")
            },
            Xcode {
                build: "16F6".to_owned(),
                app: apps.join("Xcode.app")
            },
        ]
    );
    assert_eq!(snapshot(&apps), before);
    assert!(SystemFiles.entries(&scratch.path("absent")).is_err());
}
