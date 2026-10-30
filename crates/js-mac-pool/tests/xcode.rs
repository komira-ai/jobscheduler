//! The Xcode catalog: the flag, the four questions in order, the states, and what is
//! advertised and granted. The questions are answered by a script; the survey itself
//! reads only the file system, to resolve each app.

mod common;

use std::collections::BTreeSet;

use common::{Scratch, VERSION_TEXT, fixture_answers};
use js_bundle::{DottedVersion, XcodeComponent};
use js_mac_pool::seatbelt::PathError;
use js_mac_pool::xcode::{
    Catalog, CatalogError, FlagError, MAX_BUILD_BYTES, Question, REASON_BYTES, Reply, State,
    Unanswered, XCRUN, XcodeFlag,
};

fn flag(build: &str, app: &std::path::Path) -> XcodeFlag {
    XcodeFlag::parse(&format!("{build}={}", app.display())).expect("a flag")
}

/// A scratch app: `<name>/Contents/Developer`.
fn app(scratch: &Scratch, name: &str) -> std::path::PathBuf {
    scratch.dir(&format!("{name}/Contents/Developer"));
    scratch.root.join(name)
}

/// What a question looks like in a log: the program's last two components and the args.
fn shown(question: &Question<'_>) -> String {
    let program = if question.program == XCRUN {
        "xcrun".to_owned()
    } else {
        question
            .program
            .rsplit('/')
            .next()
            .unwrap_or_default()
            .to_owned()
    };
    format!("{program} {}", question.args.join(" "))
}

#[test]
fn a_ready_xcode_is_asked_four_questions_in_order_and_then_its_components() {
    let scratch = Scratch::new("xcode-ready");
    let dir = app(&scratch, "Xcode_15.4.app");
    let mut asked: Vec<(String, String)> = Vec::new();
    let catalog = Catalog::survey(&[flag("15F31d", &dir)], |question| {
        asked.push((shown(question), question.developer_dir.as_str().to_owned()));
        fixture_answers(question)
    })
    .expect("catalog");
    let developer_dir = format!("{}/Contents/Developer", dir.display());
    let log: Vec<&str> = asked.iter().map(|(what, _)| what.as_str()).collect();
    assert_eq!(
        log,
        [
            "xcodebuild -version",
            "xcodebuild -license check",
            "xcodebuild -checkFirstLaunchStatus",
            "xcrun --find clang",
            "xcrun --find swiftc",
            "xcrun --find metal",
            "xcrun --find simctl",
        ]
    );
    assert!(asked.iter().all(|(_, dir)| *dir == developer_dir));
    assert_eq!(catalog.advertised(), ["15F31d"]);
    let xcode = &catalog.all()[0];
    assert_eq!(xcode.state, State::Ready);
    assert_eq!(xcode.reason, "");
    let host = catalog.host("15F31d").expect("ready");
    assert_eq!(host.build(), "15F31d");
    assert_eq!(host.version(), DottedVersion::parse("15.4").unwrap());
    assert_eq!(host.app().as_str(), dir.to_str().unwrap());
    assert_eq!(host.developer_dir().as_str(), developer_dir);
    let components: BTreeSet<XcodeComponent> = [
        XcodeComponent::Clang,
        XcodeComponent::Swift,
        XcodeComponent::Simctl,
    ]
    .into();
    assert_eq!(host.components(), &components);
}

#[test]
fn the_xcodebuild_asked_is_the_one_inside_the_app() {
    let scratch = Scratch::new("xcode-program");
    let dir = app(&scratch, "Xcode.app");
    let mut programs = Vec::new();
    Catalog::survey(&[flag("15F31d", &dir)], |question| {
        programs.push(question.program.clone());
        fixture_answers(question)
    })
    .expect("catalog");
    let inside = format!("{}/Contents/Developer/usr/bin/xcodebuild", dir.display());
    assert_eq!(programs[..3], [inside.clone(), inside.clone(), inside]);
    assert_eq!(programs[3], "/usr/bin/xcrun");
}

/// A script that answers `index`-th question of the four with `reply`, and the rest yes.
fn failing_at(index: usize, reply: Unanswered) -> impl FnMut(&Question<'_>) -> Reply {
    let mut n = 0;
    move |question| {
        n += 1;
        if n == index + 1 {
            return Err(reply.clone());
        }
        fixture_answers(question)
    }
}

#[test]
fn the_first_question_that_is_not_a_yes_decides_the_state_and_stops_the_survey() {
    let refused = |text: &str| Unanswered::Refused(text.to_owned());
    let table = [
        (0, refused("no xcodebuild"), State::Failed),
        (1, refused("license"), State::LicenseNotAccepted),
        (2, refused("first launch"), State::FirstLaunchNotRun),
        (3, refused("no clang"), State::Failed),
        (0, Unanswered::NoAnswer("hung".to_owned()), State::Failed),
        (1, Unanswered::NoAnswer("hung".to_owned()), State::Failed),
        (2, Unanswered::NoAnswer("hung".to_owned()), State::Failed),
        (3, Unanswered::NoAnswer("hung".to_owned()), State::Failed),
    ];
    for (index, reply, state) in table {
        let scratch = Scratch::new("xcode-fail");
        let dir = app(&scratch, "Xcode.app");
        let why = match &reply {
            Unanswered::Refused(text) | Unanswered::NoAnswer(text) => text.clone(),
        };
        let mut count = 0;
        let mut script = failing_at(index, reply.clone());
        let catalog = Catalog::survey(&[flag("15F31d", &dir)], |question| {
            count += 1;
            script(question)
        })
        .expect("catalog");
        let xcode = &catalog.all()[0];
        assert_eq!(xcode.state, state, "{index} {reply:?}");
        assert_eq!(xcode.reason, why);
        assert!(xcode.host.is_none());
        assert_eq!(count, index + 1, "no question after the first failure");
        assert!(catalog.advertised().is_empty());
        assert_eq!(catalog.host("15F31d"), None);
    }
}

#[test]
fn an_answer_that_is_not_an_xcode_version_fails_the_xcode() {
    let table = [
        ("", "no build in xcodebuild -version"),
        ("Xcode 15.4\n", "no build in xcodebuild -version"),
        (
            "Xcode 15.4\nBuild version \n",
            "no build in xcodebuild -version",
        ),
        (
            "Xcode 15.4\nBuild version 15F 31d\n",
            "no build in xcodebuild -version",
        ),
        (
            "Build version 15F31d\n",
            "no version in xcodebuild -version",
        ),
        (
            "Xcode 15\nBuild version 15F31d\n",
            "no version in xcodebuild -version",
        ),
        (
            "Xcode\nBuild version 15F31d\n",
            "no version in xcodebuild -version",
        ),
        (
            "Xcode 15.4\nBuild version 15F99z\n",
            "flagged as build 15F31d, but xcodebuild says 15F99z",
        ),
    ];
    for (shown_text, reason) in table {
        let scratch = Scratch::new("xcode-version");
        let dir = app(&scratch, "Xcode.app");
        let catalog = Catalog::survey(&[flag("15F31d", &dir)], |question| match question.args {
            ["-version"] => Ok(shown_text.to_owned()),
            _ => fixture_answers(question),
        })
        .expect("catalog");
        let xcode = &catalog.all()[0];
        assert_eq!(xcode.state, State::Failed, "{shown_text:?}");
        assert!(
            xcode.reason.starts_with(reason),
            "{shown_text:?}: {}",
            xcode.reason
        );
        assert!(xcode.host.is_none());
    }
    assert!(VERSION_TEXT.contains("15F31d"));
}

#[test]
fn a_beta_version_and_extra_words_are_read_by_their_first_token() {
    let scratch = Scratch::new("xcode-beta");
    let dir = app(&scratch, "Xcode-beta.app");
    let catalog = Catalog::survey(&[flag("16A5171c", &dir)], |question| match question.args {
        ["-version"] => Ok("Xcode 16.0 Beta 3\nBuild version 16A5171c\n".to_owned()),
        _ => fixture_answers(question),
    })
    .expect("catalog");
    let host = catalog.host("16A5171c").expect("ready");
    assert_eq!(host.version(), DottedVersion::parse("16.0").unwrap());
}

#[test]
fn an_app_that_does_not_resolve_fails_without_a_question() {
    let scratch = Scratch::new("xcode-missing");
    let missing = scratch.root.join("Xcode_gone.app");
    let mut asked = 0;
    let catalog = Catalog::survey(&[flag("15F31d", &missing)], |question| {
        asked += 1;
        fixture_answers(question)
    })
    .expect("catalog");
    let xcode = &catalog.all()[0];
    assert_eq!(xcode.state, State::Failed);
    assert!(xcode.reason.contains("Xcode_gone.app"), "{}", xcode.reason);
    assert_eq!(asked, 0);
}

#[test]
fn a_linked_app_is_granted_by_its_real_path() {
    let scratch = Scratch::new("xcode-link");
    let real = app(&scratch, "Xcode_15.4.app");
    let link = scratch.root.join("Xcode.app");
    std::os::unix::fs::symlink(&real, &link).expect("symlink");
    let catalog = Catalog::survey(&[flag("15F31d", &link)], fixture_answers).expect("catalog");
    let host = catalog.host("15F31d").expect("ready");
    assert_eq!(host.app().as_str(), real.to_str().unwrap());
    assert_eq!(
        host.developer_dir().as_str(),
        format!("{}/Contents/Developer", real.display())
    );
}

/// A long run of directories under the scratch root whose real path is `len` bytes.
#[cfg(target_os = "linux")]
fn long_app(scratch: &Scratch, len: usize) -> std::path::PathBuf {
    let mut dir = scratch.root.clone();
    while len - dir.as_os_str().len() > 200 {
        dir.push("d".repeat(199));
    }
    let rest = len - dir.as_os_str().len() - 1;
    dir.push("e".repeat(rest));
    std::fs::create_dir_all(&dir).expect("long directory");
    dir
}

#[cfg(target_os = "linux")]
#[test]
fn an_app_whose_developer_dir_is_too_long_fails() {
    let scratch = Scratch::new("xcode-long");
    let dir = long_app(&scratch, 1020);
    assert_eq!(dir.as_os_str().len(), 1020);
    let catalog = Catalog::survey(&[flag("15F31d", &dir)], fixture_answers).expect("catalog");
    assert_eq!(catalog.all()[0].state, State::Failed);
    assert!(
        catalog.all()[0].reason.contains("too long"),
        "{}",
        catalog.all()[0].reason
    );
}

#[cfg(target_os = "linux")]
#[test]
fn an_app_whose_real_path_is_not_text_fails() {
    use std::os::unix::ffi::OsStrExt as _;
    let scratch = Scratch::new("xcode-bytes");
    let link = scratch.root.join("Xcode.app");
    let target = scratch
        .root
        .join(std::ffi::OsStr::from_bytes(b"real\xff.app"));
    std::fs::create_dir_all(target.join("Contents/Developer")).expect("directory");
    std::os::unix::fs::symlink(&target, &link).expect("symlink");
    let catalog = Catalog::survey(&[flag("15F31d", &link)], fixture_answers).expect("catalog");
    assert_eq!(catalog.all()[0].state, State::Failed);
    assert!(
        catalog.all()[0].reason.contains("UTF-8"),
        "{}",
        catalog.all()[0].reason
    );
}

#[test]
fn only_ready_xcodes_are_advertised_and_granted_in_the_order_of_the_flags() {
    let scratch = Scratch::new("xcode-two");
    let first = app(&scratch, "Xcode_15.4.app");
    let broken = app(&scratch, "Xcode_16.0.app");
    let third = app(&scratch, "Xcode_14.3.app");
    let flags = [
        flag("15F31d", &first),
        flag("16A242d", &broken),
        flag("14E222b", &third),
    ];
    let catalog = Catalog::survey(&flags, |question| {
        let developer_dir = question.developer_dir.as_str();
        match question.args {
            ["-version"] if developer_dir.contains("16.0") => {
                Ok("Xcode 16.0\nBuild version 16A242d\n".to_owned())
            }
            ["-version"] if developer_dir.contains("14.3") => {
                Ok("Xcode 14.3\nBuild version 14E222b\n".to_owned())
            }
            ["-license", "check"] if developer_dir.contains("16.0") => {
                Err(Unanswered::Refused("license".to_owned()))
            }
            _ => fixture_answers(question),
        }
    })
    .expect("catalog");
    assert_eq!(catalog.advertised(), ["15F31d", "14E222b"]);
    assert_eq!(catalog.all().len(), 3);
    assert_eq!(catalog.all()[1].state, State::LicenseNotAccepted);
    assert!(catalog.host("16A242d").is_none());
    assert!(catalog.host("14E222b").is_some());
    assert!(catalog.host("nope").is_none());
}

#[test]
fn a_reason_is_cut_to_a_budget_at_a_character_boundary() {
    let scratch = Scratch::new("xcode-cut");
    let dir = app(&scratch, "Xcode.app");
    let table = [
        ("x".repeat(REASON_BYTES + 100), REASON_BYTES),
        ("\u{20ac}".repeat(200), 498),
        ("  short  ".to_owned(), 5),
    ];
    for (text, len) in table {
        let catalog = Catalog::survey(&[flag("15F31d", &dir)], |question| match question.args {
            ["-license", "check"] => Err(Unanswered::Refused(text.clone())),
            _ => fixture_answers(question),
        })
        .expect("catalog");
        assert_eq!(catalog.all()[0].reason.len(), len);
    }
}

#[test]
fn a_catalog_refuses_two_flags_of_one_build_and_too_many_flags() {
    let scratch = Scratch::new("xcode-dup");
    let one = app(&scratch, "Xcode_15.4.app");
    let two = app(&scratch, "Xcode.app");
    let flags = [flag("15F31d", &one), flag("15F31d", &two)];
    assert_eq!(
        Catalog::survey(&flags, fixture_answers),
        Err(CatalogError::DuplicateBuild("15F31d".to_owned()))
    );
    let many: Vec<XcodeFlag> = (0..65).map(|i| flag(&format!("B{i}"), &one)).collect();
    assert_eq!(
        Catalog::survey(&many, fixture_answers),
        Err(CatalogError::TooMany)
    );
    let none = Catalog::survey(&[], fixture_answers).expect("empty");
    assert!(none.all().is_empty());
    assert!(none.advertised().is_empty());
    assert_eq!(CatalogError::TooMany.to_string(), "too many --xcode flags");
    assert_eq!(
        CatalogError::DuplicateBuild("X".to_owned()).to_string(),
        "build X is flagged twice"
    );
}

#[test]
fn a_flag_is_a_build_and_an_absolute_path() {
    let ok = XcodeFlag::parse("15F31d=/Applications/Xcode.app").expect("a flag");
    assert_eq!(ok.build(), "15F31d");
    assert_eq!(ok.app().as_str(), "/Applications/Xcode.app");
    let with_equals = XcodeFlag::parse("A1=/Applications/X=Y.app").expect("a flag");
    assert_eq!(with_equals.app().as_str(), "/Applications/X=Y.app");
    let longest = format!("{}=/Applications/Xcode.app", "b".repeat(MAX_BUILD_BYTES));
    assert!(XcodeFlag::parse(&longest).is_ok());

    let too_long = format!(
        "{}=/Applications/Xcode.app",
        "b".repeat(MAX_BUILD_BYTES + 1)
    );
    let table: [(&str, FlagError); 8] = [
        ("15F31d", FlagError::NoEquals),
        ("/Applications/Xcode.app", FlagError::NoEquals),
        ("=/Applications/Xcode.app", FlagError::BadBuild),
        ("a b=/Applications/Xcode.app", FlagError::BadBuild),
        ("a\u{e9}=/Applications/Xcode.app", FlagError::BadBuild),
        (too_long.as_str(), FlagError::BadBuild),
        ("15F31d=Xcode.app", FlagError::Path(PathError::NotAbsolute)),
        (
            "15F31d=/Applications/../Xcode.app",
            FlagError::Path(PathError::NotNormal),
        ),
    ];
    for (text, expected) in table {
        assert_eq!(XcodeFlag::parse(text), Err(expected), "{text:?}");
    }
    assert_eq!(FlagError::NoEquals.to_string(), "expected BUILD=PATH");
    assert_eq!(
        FlagError::BadBuild.to_string(),
        "the build is not 1 to 32 ASCII letters and digits"
    );
    assert_eq!(
        FlagError::Path(PathError::Root).to_string(),
        "the path is not accepted: path is the root directory"
    );
}
