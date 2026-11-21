//! The macOS installer package: what `pkg stage-macos` lays down, what `pkg check-macos`
//! refuses, the files under `packaging/js-agent-macos` read as they are in the repository
//! (the plists are flags the programs accept), and the workflow that builds the package. The
//! real `pkgbuild` run is `tests/macos_pkg_real.rs`, on a Mac.

use std::collections::BTreeSet;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use clap::Parser;
use js_mac_provision::daemon::{BIN_DIR, Daemon, PF_POOL_LABEL, pf_pool_plist};
use js_testkit::dir::TestDir;
use xtask::macpkg::{
    Entry, Error, IDENTIFIER, Inspected, Kind, PROGRAMS, Signature, StageArgs, entries, problems,
    stage, verdict,
};

const VERSION: &str = "1.2.3";

#[derive(Parser)]
struct Stage {
    #[command(flatten)]
    args: StageArgs,
}

#[derive(Parser)]
struct Check {
    #[command(flatten)]
    args: xtask::macpkg::CheckArgs,
}

fn entry<'a>(all: &'a [Entry], path: &str) -> &'a Entry {
    all.iter()
        .find(|e| e.path == path)
        .unwrap_or_else(|| panic!("{path} is not in the payload: {all:?}"))
}

#[test]
fn the_payload_holds_the_programs_the_plists_and_two_directories() {
    let all = entries();
    let files: BTreeSet<&str> = all
        .iter()
        .filter(|e| e.kind == Kind::File)
        .map(|e| e.path.as_str())
        .collect();
    let want: BTreeSet<&str> = [
        "Library/Application Support/js-agent/bin/js-agent",
        "Library/Application Support/js-agent/bin/js-mac-session",
        "Library/Application Support/js-agent/bin/js-mac-provision",
        "Library/Application Support/js-agent/bin/js-vmm",
        "Library/Application Support/js-agent/LICENSE",
        "Library/LaunchDaemons/js-agent.plist",
        "Library/LaunchDaemons/js-agent.session.plist",
        "Library/LaunchDaemons/js-agent.pf-pool.plist",
    ]
    .into_iter()
    .collect();
    assert_eq!(files, want);
    for dir in [
        "Library/Application Support/js-agent/log",
        "Library/Application Support/js-agent/update",
    ] {
        assert_eq!(entry(&all, dir).kind, Kind::Dir, "{dir}");
    }
}

#[test]
fn the_payload_list_and_the_errors_can_be_copied_compared_and_printed() {
    let all = entries();
    assert_eq!(all.clone(), all);
    assert_eq!(format!("{:?}", all[0].kind), "Dir");
    assert!(format!("{:?}", all[0]).starts_with("Entry { path: \"Library\""));
    assert_eq!(format!("{:?}", Error::NeedsMac), "NeedsMac");
}

#[test]
fn programs_are_0755_data_is_0644_and_the_hooks_state_directory_is_0700() {
    let all = entries();
    let prefix = "Library/Application Support/js-agent";
    for program in PROGRAMS {
        assert_eq!(entry(&all, &format!("{prefix}/bin/{program}")).mode, 0o755);
    }
    assert_eq!(entry(&all, &format!("{prefix}/LICENSE")).mode, 0o644);
    for plist in [
        "js-agent.plist",
        "js-agent.session.plist",
        "js-agent.pf-pool.plist",
    ] {
        assert_eq!(
            entry(&all, &format!("Library/LaunchDaemons/{plist}")).mode,
            0o644
        );
    }
    assert_eq!(entry(&all, &format!("{prefix}/update")).mode, 0o700);
    assert_eq!(entry(&all, &format!("{prefix}/log")).mode, 0o755);
    for dir in [
        "Library",
        "Library/Application Support",
        "Library/LaunchDaemons",
        prefix,
        &format!("{prefix}/bin"),
    ] {
        assert_eq!(entry(&all, dir).mode, 0o755, "{dir}");
    }
}

#[test]
fn nothing_in_the_payload_is_writable_by_group_or_others() {
    for e in entries() {
        assert_eq!(e.mode & 0o022, 0, "{}: {:o}", e.path, e.mode);
    }
}

#[test]
fn every_entry_has_its_parent_directories_listed_once() {
    let all = entries();
    let paths: Vec<&str> = all.iter().map(|e| e.path.as_str()).collect();
    let unique: BTreeSet<&str> = paths.iter().copied().collect();
    assert_eq!(paths.len(), unique.len(), "a path is listed twice");
    for e in &all {
        let mut at = Path::new(&e.path);
        while let Some(parent) = at.parent().filter(|p| !p.as_os_str().is_empty()) {
            let found = entry(&all, parent.to_str().unwrap());
            assert_eq!(found.kind, Kind::Dir, "{}", parent.display());
            at = parent;
        }
    }
}

#[test]
fn the_payload_holds_no_release_tree_and_no_link() {
    // The package is hostkeeper's: the release is its tree, and no path of the payload is
    // the old release layout.
    for e in entries() {
        assert!(!e.path.starts_with("usr/"), "{}", e.path);
        assert!(!e.path.contains("releases"), "{}", e.path);
    }
}

#[test]
fn the_plists_programs_are_payload_files() {
    let all = entries();
    for daemon in [Daemon::agent(), Daemon::session("x")] {
        let path = daemon.program.trim_start_matches('/');
        assert_eq!(entry(&all, path).kind, Kind::File, "{path}");
        assert_eq!(entry(&all, path).mode, 0o755);
    }
    let pf_pool = program_arguments(&pf_pool_plist());
    let path = pf_pool[0].trim_start_matches('/');
    assert_eq!(entry(&all, path).kind, Kind::File, "{path}");
    assert_eq!(entry(&all, path).mode, 0o755);
    assert!(js_mac_provision::daemon::UPDATE_DIR.ends_with("/js-agent/update"));
    assert_eq!(
        entry(
            &all,
            js_mac_provision::daemon::UPDATE_DIR.trim_start_matches('/')
        )
        .mode,
        0o700
    );
    assert_eq!(
        entry(
            &all,
            js_mac_provision::daemon::LOG_DIR.trim_start_matches('/')
        )
        .kind,
        Kind::Dir
    );
}

// ---- stage ----

struct Sources {
    _dir: TestDir,
    args: StageArgs,
}

fn sources(name: &str) -> Sources {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), name);
    let at = dir.path().to_path_buf();
    let bin = at.join("bin");
    std::fs::create_dir(&bin).unwrap();
    for program in PROGRAMS {
        std::fs::write(bin.join(program), format!("bytes of {program}")).unwrap();
        std::fs::set_permissions(bin.join(program), std::fs::Permissions::from_mode(0o600))
            .unwrap();
    }
    for file in ["LICENSE", "agent.plist", "session.plist", "pf-pool.plist"] {
        std::fs::write(at.join(file), format!("bytes of {file}")).unwrap();
    }
    let args = Stage::parse_from([
        "stage",
        "--version",
        VERSION,
        "--bin-dir",
        bin.to_str().unwrap(),
        "--license",
        at.join("LICENSE").to_str().unwrap(),
        "--agent-plist",
        at.join("agent.plist").to_str().unwrap(),
        "--session-plist",
        at.join("session.plist").to_str().unwrap(),
        "--pf-pool-plist",
        at.join("pf-pool.plist").to_str().unwrap(),
        "--out",
        at.join("out").to_str().unwrap(),
    ])
    .args;
    Sources { _dir: dir, args }
}

fn walk(dir: &Path, base: &Path, into: &mut BTreeSet<String>) {
    for item in std::fs::read_dir(dir).unwrap() {
        let path = item.unwrap().path();
        into.insert(
            path.strip_prefix(base)
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned(),
        );
        if path.is_dir() {
            walk(&path, base, into);
        }
    }
}

fn mode_of(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
}

#[test]
fn staging_lays_down_exactly_the_payload_with_the_sources_bytes_and_the_listed_modes() {
    let s = sources("macpkg-stage");
    let line = stage(&s.args).unwrap();
    assert!(line.starts_with("staged js-agent 1.2.3"), "{line}");
    let root = s.args.out.join("root");
    let mut found = BTreeSet::new();
    walk(&root, &root, &mut found);
    let all = entries();
    let want: BTreeSet<String> = all.iter().map(|e| e.path.clone()).collect();
    assert_eq!(found, want);
    for e in &all {
        assert_eq!(mode_of(&root.join(&e.path)), e.mode, "{}", e.path);
    }
    let bin = root.join("Library/Application Support/js-agent/bin");
    for program in PROGRAMS {
        assert_eq!(
            std::fs::read_to_string(bin.join(program)).unwrap(),
            format!("bytes of {program}")
        );
    }
    for (from, to) in [
        ("LICENSE", "Library/Application Support/js-agent/LICENSE"),
        ("agent.plist", "Library/LaunchDaemons/js-agent.plist"),
        (
            "session.plist",
            "Library/LaunchDaemons/js-agent.session.plist",
        ),
        (
            "pf-pool.plist",
            "Library/LaunchDaemons/js-agent.pf-pool.plist",
        ),
    ] {
        assert_eq!(
            std::fs::read_to_string(root.join(to)).unwrap(),
            format!("bytes of {from}"),
            "{to}"
        );
    }
}

#[test]
fn staging_writes_no_scripts() {
    let s = sources("macpkg-stage-scripts");
    stage(&s.args).unwrap();
    let mut found = BTreeSet::new();
    walk(&s.args.out, &s.args.out, &mut found);
    assert_eq!(found.iter().filter(|p| p.contains("script")).count(), 0);
    assert!(!s.args.out.join("scripts").exists());
    assert!(s.args.out.join("root").is_dir());
}

#[test]
fn staging_refuses_an_output_directory_that_exists_and_a_source_that_does_not() {
    let s = sources("macpkg-stage-refuse");
    std::fs::create_dir(&s.args.out).unwrap();
    let err = stage(&s.args).unwrap_err();
    assert!(err.to_string().contains("exists"), "{err}");

    let s = sources("macpkg-stage-missing");
    std::fs::remove_file(s.args.bin_dir.join("js-mac-session")).unwrap();
    let err = stage(&s.args).unwrap_err();
    assert!(matches!(err, Error::Io(_)), "{err}");
    assert!(err.to_string().contains("js-mac-session"), "{err}");
}

#[test]
fn the_program_stages_the_package_when_asked_to() {
    let s = sources("macpkg-stage-binary");
    let a = &s.args;
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args(["pkg", "stage-macos", "--version", VERSION])
        .arg("--bin-dir")
        .arg(&a.bin_dir)
        .arg("--license")
        .arg(&a.license)
        .arg("--agent-plist")
        .arg(&a.agent_plist)
        .arg("--session-plist")
        .arg(&a.session_plist)
        .arg("--pf-pool-plist")
        .arg(&a.pf_pool_plist)
        .arg("--out")
        .arg(&a.out)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let said = String::from_utf8(out.stdout).unwrap();
    assert!(said.starts_with("staged js-agent 1.2.3"), "{said}");
    assert!(a.out.join("root").is_dir());
    assert!(!a.out.join("scripts").exists());
}

#[test]
fn the_program_refuses_a_package_it_cannot_read_with_status_1() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args(["pkg", "check-macos", "--pkg", "/nonexistent/js-agent.pkg"])
        .args(["--version", VERSION])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).starts_with("xtask: "));
    assert!(out.stdout.is_empty());
}

/// On a Mac the verb fails on a directory without `js-vmm` (codesign finds nothing to sign);
/// elsewhere it fails before looking, since only a Mac has codesign. Either way: status 1, the
/// reason on standard error, nothing on standard output.
#[test]
fn the_program_refuses_to_entitle_a_directory_without_js_vmm_with_status_1() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args(["pkg", "entitle-macos", "--bin-dir", "/nonexistent/bin"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).starts_with("xtask: "));
    assert!(out.stdout.is_empty());
}

// ---- check ----

fn bom_of(all: &[Entry]) -> String {
    let mut text = String::from(".\t40755\t0/0\n");
    for e in all {
        match e.kind {
            Kind::Dir => text.push_str(&format!("./{}\t{:o}\t0/0\n", e.path, 0o040_000 | e.mode)),
            Kind::File => text.push_str(&format!(
                "./{}\t{:o}\t0/0\t12\t3456\n",
                e.path,
                0o100_000 | e.mode
            )),
        }
    }
    text
}

/// What `codesign` prints of a program as the workflow leaves it: `js-vmm` signed ad hoc with
/// the hardened runtime and the virtualization entitlement, the others as the linker signed them.
fn good_signature(program: &str) -> Signature {
    let (flags, entitlements) = if program == "js-vmm" {
        (
            "0x10002(adhoc,runtime)",
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><plist version=\"1.0\"><dict><key>com.apple.security.virtualization</key><true/></dict></plist>",
        )
    } else {
        ("0x20002(adhoc,linker-signed)", "")
    };
    Signature {
        program: program.to_owned(),
        verified: Ok(()),
        display: format!(
            "Executable=/x/{program}\nCodeDirectory v=20400 size=1 flags={flags} hashes=1+0 location=embedded\n"
        ),
        entitlements: entitlements.to_owned(),
    }
}

fn good() -> Inspected {
    Inspected {
        bom: bom_of(&entries()),
        scripts: Vec::new(),
        package_info: format!(
            "<pkg-info format-version=\"2\" identifier=\"{IDENTIFIER}\" version=\"{VERSION}\" install-location=\"/\"/>"
        ),
        signatures: PROGRAMS.iter().map(|p| good_signature(p)).collect(),
    }
}

fn refused(inspected: &Inspected) -> Vec<String> {
    let found = problems(inspected, VERSION);
    assert!(!found.is_empty());
    found
}

#[test]
fn the_package_that_stage_describes_has_no_problem() {
    assert_eq!(problems(&good(), VERSION), Vec::<String>::new());
}

#[test]
fn a_file_missing_from_the_bill_of_materials_is_named() {
    let mut p = good();
    p.bom = p
        .bom
        .lines()
        .filter(|l| !l.contains("js-mac-session\t"))
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(
        refused(&p),
        ["missing from the package: Library/Application Support/js-agent/bin/js-mac-session"]
    );
}

#[test]
fn a_file_the_package_should_not_hold_is_named() {
    let mut p = good();
    p.bom.push_str("./etc/sudoers.d/js\t100644\t0/0\t3\t4\n");
    assert_eq!(refused(&p), ["not part of the package: etc/sudoers.d/js"]);
}

#[test]
fn a_wrong_mode_is_named_with_both_modes() {
    let mut p = good();
    p.bom = p
        .bom
        .replace("js-agent/update\t40700", "js-agent/update\t40755");
    assert_eq!(
        refused(&p),
        ["Library/Application Support/js-agent/update: mode is 0755, not 0700"]
    );
}

#[test]
fn an_owner_that_is_not_root_is_named() {
    let mut p = good();
    p.bom = p.bom.replace(
        "js-agent/LICENSE\t100644\t0/0",
        "js-agent/LICENSE\t100644\t501/20",
    );
    assert_eq!(
        refused(&p),
        ["Library/Application Support/js-agent/LICENSE: owned by 501/20, not root:wheel"]
    );
}

#[test]
fn root_with_the_admin_group_is_allowed_and_root_with_a_user_group_is_not() {
    let mut p = good();
    p.bom = p.bom.replace(
        "./Library/LaunchDaemons\t40755\t0/0",
        "./Library/LaunchDaemons\t40755\t0/80",
    );
    assert_eq!(problems(&p, VERSION), Vec::<String>::new());
    p.bom = p.bom.replace(
        "./Library/LaunchDaemons\t40755\t0/80",
        "./Library/LaunchDaemons\t40755\t0/20",
    );
    assert_eq!(
        refused(&p),
        ["Library/LaunchDaemons: owned by 0/20, not root:wheel"]
    );
}

#[test]
fn a_link_or_a_file_where_a_directory_belongs_is_refused() {
    let mut p = good();
    p.bom = p.bom.replace(
        "./Library/Application Support/js-agent/bin\t40755",
        "./Library/Application Support/js-agent/bin\t100755",
    );
    assert_eq!(
        refused(&p),
        ["Library/Application Support/js-agent/bin: is a file, not a directory"]
    );
    let mut p = good();
    p.bom
        .push_str("./Library/Application Support/js-agent/bin/current\t120755\t0/0\t30\t1\n");
    assert_eq!(
        refused(&p),
        [
            "Library/Application Support/js-agent/bin/current: is not a file or a directory (mode 120755)"
        ]
    );
}

#[test]
fn a_directory_where_a_file_belongs_is_refused() {
    let mut p = good();
    p.bom = p.bom.replace(
        "js-agent/LICENSE\t100644\t0/0\t12\t3456",
        "js-agent/LICENSE\t40755\t0/0",
    );
    assert_eq!(
        refused(&p),
        ["Library/Application Support/js-agent/LICENSE: is a directory, not a file"]
    );
}

#[test]
fn a_line_that_is_not_lsbom_output_and_a_path_listed_twice_are_refused() {
    let mut p = good();
    p.bom.push_str("garbage\n");
    assert_eq!(
        refused(&p),
        ["cannot read the bill of materials line \"garbage\""]
    );
    let mut p = good();
    p.bom
        .push_str("./Library/Application Support/js-agent/update\t40700\t0/0\n");
    assert_eq!(
        refused(&p),
        ["Library/Application Support/js-agent/update is listed twice"]
    );
}

#[test]
fn any_script_at_all_is_refused() {
    for names in [vec!["postinstall"], vec!["preinstall", "postinstall"]] {
        let mut p = good();
        p.scripts = names.iter().map(ToString::to_string).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(
            refused(&p),
            [format!(
                "the package holds scripts, and must hold none: {sorted:?}"
            )]
        );
    }
}

#[test]
fn a_package_info_of_another_identifier_or_version_is_refused() {
    let mut p = good();
    p.package_info = p.package_info.replace(IDENTIFIER, "com.example.other");
    assert_eq!(
        refused(&p),
        ["PackageInfo does not name the identifier com.komira.js-agent"]
    );
    let mut p = good();
    p.package_info = p
        .package_info
        .replace("version=\"1.2.3\"", "version=\"1.2.4\"");
    assert_eq!(refused(&p), ["PackageInfo does not name the version 1.2.3"]);
}

#[cfg(not(target_os = "macos"))]
#[test]
fn reading_a_package_needs_a_mac() {
    let args = Check::parse_from(["check", "--pkg", "/x.pkg", "--version", VERSION]).args;
    assert_eq!(xtask::macpkg::check(&args), Err(Error::NeedsMac));
}

// ---- signatures ----

#[test]
fn js_vmm_alone_carries_an_entitlement_and_it_is_the_virtualization_one() {
    use xtask::macpkg::{VIRTUALIZATION, entitlements_of};
    assert_eq!(VIRTUALIZATION, "com.apple.security.virtualization");
    for program in PROGRAMS {
        let want: &[&str] = if program == "js-vmm" {
            &[VIRTUALIZATION]
        } else {
            &[]
        };
        assert_eq!(entitlements_of(program), want, "{program}");
    }
}

#[test]
fn the_entitlements_file_sets_each_key_to_true_and_nothing_else() {
    use xtask::macpkg::entitlements_plist;
    for keys in [
        &[][..],
        &["com.apple.security.virtualization"][..],
        &["a.b", "c.d"][..],
    ] {
        let text = entitlements_plist(keys);
        let dict = plist::Value::from_reader_xml(text.as_bytes())
            .unwrap()
            .into_dictionary()
            .unwrap();
        let read: Vec<(String, plist::Value)> = dict.into_iter().collect();
        let want: Vec<(String, plist::Value)> = keys
            .iter()
            .map(|k| ((*k).to_owned(), plist::Value::Boolean(true)))
            .collect();
        assert_eq!(read, want, "{text}");
    }
}

/// A change to one program's signature.
type Change = Box<dyn Fn(&mut Signature)>;

/// Each way a program's signature can leave what the package needs, with the one line that
/// names it, in full.
#[test]
fn each_way_a_programs_signature_is_wrong_is_named_in_full() {
    let entitled = |xml: &str| {
        format!("<?xml version=\"1.0\"?><plist version=\"1.0\"><dict>{xml}</dict></plist>")
    };
    let virt = "<key>com.apple.security.virtualization</key>";
    let cases: Vec<(&str, Change, String)> = vec![
        (
            "js-vmm",
            Box::new(|s| s.verified = Err("a sealed resource is missing\n".to_owned())),
            "bin/js-vmm: codesign --verify --strict refuses it: a sealed resource is missing"
                .to_owned(),
        ),
        (
            "js-agent",
            Box::new(|s| s.verified = Err("code object is not signed at all".to_owned())),
            "bin/js-agent: codesign --verify --strict refuses it: code object is not signed at all"
                .to_owned(),
        ),
        (
            "js-vmm",
            Box::new(|s| s.entitlements = String::new()),
            "bin/js-vmm: carries the entitlements {}, not {\"com.apple.security.virtualization\": \"true\"}"
                .to_owned(),
        ),
        (
            "js-vmm",
            Box::new(move |s| s.entitlements = entitled(&format!("{virt}<false/>"))),
            "bin/js-vmm: carries the entitlements {\"com.apple.security.virtualization\": \"Boolean(false)\"}, not {\"com.apple.security.virtualization\": \"true\"}"
                .to_owned(),
        ),
        (
            "js-vmm",
            Box::new(move |s| {
                s.entitlements = entitled(&format!(
                    "{virt}<true/><key>com.apple.security.get-task-allow</key><true/>"
                ));
            }),
            "bin/js-vmm: carries the entitlements {\"com.apple.security.get-task-allow\": \"true\", \"com.apple.security.virtualization\": \"true\"}, not {\"com.apple.security.virtualization\": \"true\"}"
                .to_owned(),
        ),
        (
            "js-mac-session",
            Box::new(move |s| s.entitlements = entitled(&format!("{virt}<true/>"))),
            "bin/js-mac-session: carries the entitlements {\"com.apple.security.virtualization\": \"true\"}, not {}"
                .to_owned(),
        ),
        (
            "js-vmm",
            Box::new(|s| s.display = s.display.replace("(adhoc,runtime)", "(adhoc)")),
            "bin/js-vmm: is not signed with the hardened runtime".to_owned(),
        ),
        (
            "js-vmm",
            Box::new(|s| s.display = "code object is not signed at all\n".to_owned()),
            "bin/js-vmm: is not signed with the hardened runtime".to_owned(),
        ),
        (
            "js-vmm",
            Box::new(|s| s.entitlements = "<plist><dict><key>x</key>".to_owned()),
            "bin/js-vmm: cannot read its entitlements: ".to_owned(),
        ),
        (
            "js-vmm",
            Box::new(|s| {
                s.entitlements = "<?xml version=\"1.0\"?><plist version=\"1.0\"><array/></plist>"
                    .to_owned();
            }),
            "bin/js-vmm: cannot read its entitlements: it is not a dictionary".to_owned(),
        ),
    ];
    for (program, change, says) in cases {
        let mut p = good();
        let at = p
            .signatures
            .iter()
            .position(|s| s.program == program)
            .unwrap();
        change(&mut p.signatures[at]);
        let found = refused(&p);
        assert_eq!(found.len(), 1, "{program}: {found:?}");
        if says.ends_with(": ") {
            // The parser's own words follow; the line is ours up to them.
            assert!(found[0].starts_with(&says), "{found:?}");
        } else {
            assert_eq!(found[0], says);
        }
    }
}

#[test]
fn a_program_whose_signature_was_not_read_is_refused() {
    for program in PROGRAMS {
        let mut p = good();
        p.signatures.retain(|s| s.program != program);
        assert_eq!(
            refused(&p),
            [format!("bin/{program}: its signature was not read")]
        );
    }
    // A signature read for something that is not a program of the package is no stand-in.
    let mut p = good();
    p.signatures[3].program = "vfkit".to_owned();
    assert_eq!(refused(&p), ["bin/js-vmm: its signature was not read"]);
}

#[test]
fn the_flags_are_read_from_the_flags_field_alone() {
    // `runtime` in the path is not the flag; the flags field is the one that counts.
    let mut p = good();
    p.signatures[3].display =
        "Executable=/x/(adhoc,runtime)/js-vmm\nCodeDirectory flags=0x2(adhoc) hashes=1\n"
            .to_owned();
    assert_eq!(
        refused(&p),
        ["bin/js-vmm: is not signed with the hardened runtime"]
    );
    // A program with no entitlement needs no hardened runtime.
    let mut p = good();
    p.signatures[0].display = "flags=0x2(adhoc)\n".to_owned();
    assert_eq!(problems(&p, VERSION), Vec::<String>::new());
}

#[test]
fn signatures_can_be_copied_compared_and_printed_and_unsigned_says_what_is_wrong() {
    let s = good_signature("js-vmm");
    assert_eq!(s.clone(), s);
    assert!(format!("{s:?}").starts_with("Signature { program: \"js-vmm\""));
    assert_eq!(
        Error::Unsigned(vec!["a".to_owned(), "b".to_owned()]).to_string(),
        "the signed program is not as the package needs it: a; b"
    );
    assert_eq!(
        Error::NeedsMac.to_string(),
        "pkg check-macos and pkg entitle-macos run pkgutil, lsbom and codesign, which only a Mac has"
    );
}

#[cfg(not(target_os = "macos"))]
#[test]
fn signing_js_vmm_needs_a_mac() {
    #[derive(Parser)]
    struct Entitle {
        #[command(flatten)]
        args: xtask::macpkg::EntitleArgs,
    }
    let args = Entitle::parse_from(["entitle", "--bin-dir", "/x"]).args;
    assert_eq!(xtask::macpkg::entitle(&args), Err(Error::NeedsMac));
}

#[test]
fn the_verdict_is_a_line_for_the_package_and_the_problems_for_anything_else() {
    let pkg = Path::new("/x/js-agent.pkg");
    let line = verdict(&good(), VERSION, pkg).unwrap();
    assert_eq!(
        line,
        format!(
            "/x/js-agent.pkg: js-agent 1.2.3 with {} entries",
            entries().len()
        )
    );
    let mut p = good();
    p.scripts.push("preinstall".to_owned());
    let err = verdict(&p, VERSION, pkg).unwrap_err();
    assert_eq!(err, Error::Refused(problems(&p, VERSION)));
    assert!(
        err.to_string()
            .starts_with("the package is not the js-agent package: ")
    );
}

#[test]
fn every_part_of_an_lsbom_line_is_needed() {
    let mut p = good();
    for line in [
        "./x\tzz\t0/0",
        "./x\t100644",
        "./x\t100644\t0-0",
        "./x\t100644\ta/b",
        "./x\t100644\t0/b",
    ] {
        p.bom.push_str(line);
        p.bom.push('\n');
    }
    assert_eq!(refused(&p).len(), 5);
}

// ---- the plists of js-mac-provision ----

/// The `<string>` items of a plist's `ProgramArguments`, XML characters unescaped.
fn program_arguments(plist: &str) -> Vec<String> {
    let start = plist.find("<key>ProgramArguments</key>").unwrap();
    let array = &plist[start..];
    let array = &array[..array.find("</array>").unwrap()];
    array
        .lines()
        .filter_map(|l| l.trim().strip_prefix("<string>")?.strip_suffix("</string>"))
        .map(|s| {
            s.replace("&amp;", "&")
                .replace("&lt;", "<")
                .replace("&gt;", ">")
        })
        .collect()
}

/// Every flag the agent's plist gives, and every flag of the Mac flags file it names, is one the macOS build of the agent has: a flag of every
/// build (this build's parser knows it, and it is no Linux build's flag) or a flag of the macOS
/// build's runtime (`platform::MACOS_ONLY`), the helper's socket among them. It runs on every
/// build, so a Linux pull request that renames a flag the plist names fails here, and not only
/// in the macOS beta stage. Catches a plist that names a flag the Mac agent refuses at start.
#[test]
fn the_agents_plist_names_only_flags_of_the_macos_agent() {
    use clap::CommandFactory as _;
    use js_agent::platform::{LINUX_ONLY, MACOS_ONLY};
    let plist = Daemon::agent().render();
    let mut words: Vec<String> = program_arguments(&plist).into_iter().skip(1).collect();
    words.extend(js_agent::flags_file::words(MAC_FLAGS).unwrap());
    let command = js_agent::Cli::command();
    let flags: Vec<&str> = words
        .iter()
        .filter_map(|word| word.strip_prefix("--"))
        .map(|word| word.split('=').next().unwrap_or(word))
        .collect();
    assert!(flags.contains(&"native-helper-socket"), "{flags:?}");
    assert!(flags.contains(&"native-network"), "{flags:?}");
    for flag in flags {
        let common = command
            .get_arguments()
            .any(|arg| arg.get_long() == Some(flag))
            && !LINUX_ONLY.contains(&flag);
        assert!(MACOS_ONLY.contains(&flag) || common, "--{flag}");
    }
}

/// The agent's plist parses as a command line of the macOS build of the agent (the native
/// flags exist in that build only), with the pool and the helper's socket it names.
#[cfg(target_os = "macos")]
#[test]
fn the_agents_plist_names_flags_the_agent_accepts() {
    let plist = Daemon::agent().render();
    let words = program_arguments(&plist);
    assert_eq!(words[0], format!("{BIN_DIR}/js-agent"));
    // The host's own flags, the server and pool among them, are in the flags file the plist names.
    let cli = js_agent::Cli::try_parse_from(
        std::iter::once("js-agent".to_owned())
            .chain(words.into_iter().skip(1))
            .chain([
                "--server".to_owned(),
                "https://leases.example:7300".to_owned(),
                "--pool".to_owned(),
                "mac".to_owned(),
            ]),
    )
    .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(
        cli.flags_file.as_deref(),
        Some(Path::new("/etc/js-agent/flags"))
    );
    assert_eq!(
        cli.os.native_helper_socket.as_deref(),
        Some(Path::new("/var/run/js-mac-session/socket"))
    );
}

/// What a Mac's `/etc/js-agent/flags` holds, one word per line, with documentation addresses.
const MAC_FLAGS: &str = "# the Mac build host group\n\
    --server=https://leases.example:7300\n\
    --pool=mac\n\
    --ca-cert=/etc/js-agent/ca.pem\n\
    --host-id-from-facts=/var/lib/hostkeeper/facts.json\n\
    --native-network=on\n\
    --native-pf-uid-start=600\n\
    --native-pf-uid-count=100\n\
    --native-pf-endpoint=192.0.2.10:443\n\
    --native-pf-endpoint=192.0.2.11:443\n";

/// The agent's plist, once the flags file is expanded in place of `--flags-file`,
/// is a command line `js_agent::Cli` accepts: the file gives the server, the pool and the
/// native network, and the plist's own words give the rest. It catches a plist word the agent
/// would refuse next to the file's, or a file word the plist makes a duplicate of.
#[cfg(target_os = "macos")]
#[test]
fn the_agents_plist_names_flags_the_agent_accepts_with_a_flags_file_expanded() {
    let plist = Daemon::agent().render();
    let words = program_arguments(&plist);
    assert_eq!(words[0], format!("{BIN_DIR}/js-agent"));
    let mut args = vec!["js-agent".to_owned()];
    let mut rest = words.into_iter().skip(1);
    while let Some(word) = rest.next() {
        if word == "--flags-file" {
            assert_eq!(rest.next().as_deref(), Some("/etc/js-agent/flags"));
            args.extend(js_agent::flags_file::words(MAC_FLAGS).unwrap());
        } else {
            args.push(word);
        }
    }
    let cli = js_agent::Cli::try_parse_from(args).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(cli.pool, "mac");
    assert_eq!(
        cli.host.host_id_from_facts.as_deref(),
        Some(Path::new("/var/lib/hostkeeper/facts.json"))
    );
    assert_eq!(
        cli.os.native_helper_socket.as_deref(),
        Some(Path::new("/var/run/js-mac-session/socket"))
    );
}

#[test]
fn the_helpers_plist_names_flags_the_helper_accepts_and_a_socket_the_agent_dials() {
    let plist = Daemon::session("cdhash H\"abc\"").render();
    let words = program_arguments(&plist);
    assert_eq!(words[0], format!("{BIN_DIR}/js-mac-session"));
    let args = js_mac_session::start::Args::try_parse_from(
        std::iter::once("js-mac-session".to_owned()).chain(words.into_iter().skip(1)),
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let agent = program_arguments(&Daemon::agent().render());
    let socket = agent
        .iter()
        .position(|w| w == "--native-helper-socket")
        .map(|at| PathBuf::from(&agent[at + 1]))
        .unwrap();
    assert_eq!(args.socket, socket);
}

/// The pf daemon's plist parses as a command line of the `js-mac-provision` the package carries,
/// names the same flags file as the agent's plist, and reads from the Mac flags file the anchor of
/// the pool the agent's `--native-pf-*` flags name; its label is the plist's file name in the
/// payload. Catches a plist whose daemon would load another anchor than the agent checks, or a
/// verb or flag the program refuses.
#[test]
fn the_pf_daemons_plist_installs_the_anchor_the_agents_flags_name() {
    use js_mac_provision::agent_flags::{AgentPf, FileFacts, read};
    use js_mac_provision::cli::{Cli, Verb};
    let words = program_arguments(&pf_pool_plist());
    assert_eq!(words[0], format!("{BIN_DIR}/js-mac-provision"));
    let cli = Cli::try_parse_from(words.iter().cloned()).unwrap_or_else(|e| panic!("{e}"));
    let Verb::Install(install) = &cli.verb else {
        panic!("not install: {:?}", cli.verb);
    };
    let agent = program_arguments(&Daemon::agent().render());
    let agent_file = agent
        .iter()
        .position(|w| w == "--flags-file")
        .map(|at| PathBuf::from(&agent[at + 1]))
        .unwrap();
    assert_eq!(install.flags_file.as_ref(), Some(&agent_file));
    let facts = FileFacts {
        regular: true,
        mode: 0o644,
        uid: 0,
        bytes: MAC_FLAGS.as_bytes().to_vec(),
    };
    let AgentPf::On(policy) = read(&agent_file, 501, &move |_| Ok(facts.clone())).unwrap() else {
        panic!("the Mac flags name no anchor");
    };
    assert_eq!(policy.uids.first(), 600);
    assert_eq!(policy.uids.last(), 699);
    assert_eq!(
        policy
            .endpoints
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        ["192.0.2.10:443", "192.0.2.11:443"]
    );
    assert_eq!(policy.anchor.as_str(), cli.anchor);
    assert_eq!(policy.state_dir, cli.state_dir);
    assert_eq!(policy.pfctl, cli.pfctl);
    assert!(
        entries()
            .iter()
            .any(|e| e.path == format!("Library/LaunchDaemons/{PF_POOL_LABEL}.plist"))
    );
}
