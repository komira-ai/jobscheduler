//! The macOS installer package: what `pkg stage-macos` lays down, what `pkg check-macos`
//! refuses, the files under `packaging/js-agent-macos` read as they are in the repository
//! (the plists are flags the programs accept), and the workflow that builds the package. The
//! real `pkgbuild` run is `tests/macos_pkg_real.rs`, on a Mac.

use std::collections::BTreeSet;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use clap::Parser;
use js_mac_provision::daemon::{BIN_DIR, Daemon};
use js_testkit::dir::TestDir;
use xtask::macpkg::{
    Entry, Error, IDENTIFIER, Inspected, Kind, StageArgs, entries, problems, stage, verdict,
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
        "Library/Application Support/js-agent/LICENSE",
        "Library/LaunchDaemons/js-agent.plist",
        "Library/LaunchDaemons/js-agent.session.plist",
    ]
    .into_iter()
    .collect();
    assert_eq!(files, want);
    let daemons: Vec<&str> = files
        .iter()
        .copied()
        .filter(|p| p.starts_with("Library/LaunchDaemons/"))
        .collect();
    assert_eq!(daemons.len(), 2, "{daemons:?}");
    assert!(
        !files.contains("Library/LaunchDaemons/js-agent.pf-pool.plist"),
        "the agent loads the pf anchor itself; no daemon of the package does"
    );
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
    for program in ["js-agent", "js-mac-session", "js-mac-provision"] {
        assert_eq!(entry(&all, &format!("{prefix}/bin/{program}")).mode, 0o755);
    }
    assert_eq!(entry(&all, &format!("{prefix}/LICENSE")).mode, 0o644);
    for plist in ["js-agent.plist", "js-agent.session.plist"] {
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
    for program in ["js-agent", "js-mac-session", "js-mac-provision"] {
        std::fs::write(bin.join(program), format!("bytes of {program}")).unwrap();
        std::fs::set_permissions(bin.join(program), std::fs::Permissions::from_mode(0o600))
            .unwrap();
    }
    for file in ["LICENSE", "agent.plist", "session.plist"] {
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
    for program in ["js-agent", "js-mac-session", "js-mac-provision"] {
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

fn good() -> Inspected {
    Inspected {
        bom: bom_of(&entries()),
        scripts: Vec::new(),
        package_info: format!(
            "<pkg-info format-version=\"2\" identifier=\"{IDENTIFIER}\" version=\"{VERSION}\" install-location=\"/\"/>"
        ),
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

/// Asserts that a package whose listing carries the retired pf daemon's plist is refused,
/// naming that path. Catches a package built with the pf job's plist still in its payload, which
/// would load the job the agent replaced.
#[test]
fn a_package_that_carries_the_retired_pf_plist_is_refused() {
    let mut p = good();
    p.bom
        .push_str("./Library/LaunchDaemons/js-agent.pf-pool.plist\t100644\t0/0\t12\t3456\n");
    let err = verdict(&p, VERSION, Path::new("/x/js-agent.pkg")).unwrap_err();
    assert_eq!(
        err.to_string(),
        "the package is not the js-agent package: not part of the package: Library/LaunchDaemons/js-agent.pf-pool.plist"
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
