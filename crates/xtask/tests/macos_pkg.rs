//! The macOS installer package: what `pkg stage-macos` lays down, what `pkg check-macos`
//! refuses, the files under `packaging/js-agent-macos` read as they are in the repository
//! (the plists are flags the programs accept, the update hook's declaration is one the hook
//! takes), and the workflow that builds the package. The real `pkgbuild` run is
//! `tests/macos_pkg_real.rs`, on a Mac.

use std::collections::BTreeSet;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use clap::Parser;
#[cfg(target_os = "linux")]
use js_mac_provision::daemon::{AGENT_LABEL, SESSION_LABEL};
use js_mac_provision::daemon::{Daemon, RELEASE_LINK};
use js_testkit::dir::TestDir;
use xtask::macpkg::{
    Entry, Error, IDENTIFIER, Inspected, Kind, StageArgs, entries, postinstall_script, problems,
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

#[cfg(target_os = "linux")]
fn root() -> &'static Path {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))
}

#[cfg(target_os = "linux")]
fn read(path: &str) -> String {
    std::fs::read_to_string(root().join(path)).unwrap_or_else(|e| panic!("{path}: {e}"))
}

fn entry<'a>(all: &'a [Entry], path: &str) -> &'a Entry {
    all.iter()
        .find(|e| e.path == path)
        .unwrap_or_else(|| panic!("{path} is not in the payload: {all:?}"))
}

#[test]
fn the_payload_holds_the_release_the_plists_the_declaration_and_three_directories() {
    let all = entries(VERSION);
    let files: BTreeSet<&str> = all
        .iter()
        .filter(|e| e.kind == Kind::File)
        .map(|e| e.path.as_str())
        .collect();
    let want: BTreeSet<&str> = [
        "usr/local/js-agent/releases/1.2.3/js-agent",
        "usr/local/js-agent/releases/1.2.3/js-mac-session",
        "usr/local/js-agent/releases/1.2.3/js-mac-provision",
        "usr/local/js-agent/releases/1.2.3/LICENSE",
        "usr/local/js-agent/share/com.komira.js-agent.plist",
        "usr/local/js-agent/share/com.komira.js-mac-session.plist",
        "usr/local/js-agent/share/updater.d/js-agent.toml",
    ]
    .into_iter()
    .collect();
    assert_eq!(files, want);
    for dir in [
        "usr/local/js-agent/link",
        "Library/Logs/js-agent",
        "var/db/js-agent-update",
    ] {
        assert_eq!(entry(&all, dir).kind, Kind::Dir, "{dir}");
    }
}

#[test]
fn the_payload_list_and_the_errors_can_be_copied_compared_and_printed() {
    let all = entries(VERSION);
    assert_eq!(all.clone(), all);
    assert_eq!(format!("{:?}", all[0].kind), "Dir");
    assert!(format!("{:?}", all[0]).starts_with("Entry { path: \"Library\""));
    assert_eq!(format!("{:?}", Error::NeedsMac), "NeedsMac");
}

#[test]
fn programs_are_0755_data_is_0644_and_the_hooks_state_directory_is_0700() {
    let all = entries(VERSION);
    let release = "usr/local/js-agent/releases/1.2.3";
    for program in ["js-agent", "js-mac-session", "js-mac-provision"] {
        assert_eq!(entry(&all, &format!("{release}/{program}")).mode, 0o755);
    }
    assert_eq!(entry(&all, &format!("{release}/LICENSE")).mode, 0o644);
    assert_eq!(entry(&all, "var/db/js-agent-update").mode, 0o700);
    assert_eq!(entry(&all, "usr/local/js-agent/link").mode, 0o755);
}

#[test]
fn nothing_in_the_payload_is_writable_by_group_or_others() {
    for e in entries(VERSION) {
        assert_eq!(e.mode & 0o022, 0, "{}: {:o}", e.path, e.mode);
    }
}

#[test]
fn every_entry_has_its_parent_directories_listed_once() {
    let all = entries(VERSION);
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
fn the_release_link_the_daemons_use_has_a_directory_of_its_own() {
    // The update hook swaps the link by writing next to it, so its parent is a write path of
    // its own; the package ships that directory empty and never the link.
    let all = entries(VERSION);
    let parent = Path::new(RELEASE_LINK).parent().unwrap();
    let parent = parent.strip_prefix("/").unwrap().to_str().unwrap();
    assert_eq!(entry(&all, parent).kind, Kind::Dir);
    assert!(
        all.iter()
            .all(|e| !e.path.starts_with(RELEASE_LINK.trim_start_matches('/')))
    );
}

#[test]
fn the_postinstall_script_only_runs_the_installed_provision_binary() {
    let script = postinstall_script(VERSION);
    assert_eq!(
        script,
        "#!/bin/sh\n\
         # Written by `cargo xtask pkg stage-macos`. The logic is `js-mac-provision postinstall`.\n\
         exec \"${3%/}/usr/local/js-agent/releases/1.2.3/js-mac-provision\" postinstall --version 1.2.3 --volume \"$3\"\n"
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
    for file in ["LICENSE", "agent.plist", "session.plist", "updater.toml"] {
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
        "--declaration",
        at.join("updater.toml").to_str().unwrap(),
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
    let all = entries(VERSION);
    let want: BTreeSet<String> = all.iter().map(|e| e.path.clone()).collect();
    assert_eq!(found, want);
    for e in &all {
        assert_eq!(mode_of(&root.join(&e.path)), e.mode, "{}", e.path);
    }
    let release = root.join("usr/local/js-agent/releases/1.2.3");
    for program in ["js-agent", "js-mac-session", "js-mac-provision"] {
        assert_eq!(
            std::fs::read_to_string(release.join(program)).unwrap(),
            format!("bytes of {program}")
        );
    }
    for (from, to) in [
        ("LICENSE", "usr/local/js-agent/releases/1.2.3/LICENSE"),
        (
            "agent.plist",
            "usr/local/js-agent/share/com.komira.js-agent.plist",
        ),
        (
            "session.plist",
            "usr/local/js-agent/share/com.komira.js-mac-session.plist",
        ),
        (
            "updater.toml",
            "usr/local/js-agent/share/updater.d/js-agent.toml",
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
fn staging_writes_the_postinstall_script_alone_in_the_scripts_directory() {
    let s = sources("macpkg-stage-scripts");
    stage(&s.args).unwrap();
    let scripts = s.args.out.join("scripts");
    let mut found = BTreeSet::new();
    walk(&scripts, &scripts, &mut found);
    assert_eq!(found, BTreeSet::from(["postinstall".to_owned()]));
    assert_eq!(
        std::fs::read_to_string(scripts.join("postinstall")).unwrap(),
        postinstall_script(VERSION)
    );
    assert_eq!(mode_of(&scripts.join("postinstall")), 0o755);
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
        .arg("--declaration")
        .arg(&a.declaration)
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
    assert!(a.out.join("scripts/postinstall").is_file());
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
        bom: bom_of(&entries(VERSION)),
        scripts: vec![(
            "postinstall".to_owned(),
            postinstall_script(VERSION).into_bytes(),
        )],
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
        ["missing from the package: usr/local/js-agent/releases/1.2.3/js-mac-session"]
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
        .replace("js-agent-update\t40700", "js-agent-update\t40755");
    assert_eq!(
        refused(&p),
        ["var/db/js-agent-update: mode is 0755, not 0700"]
    );
}

#[test]
fn an_owner_that_is_not_root_is_named() {
    let mut p = good();
    p.bom.push_str("");
    p.bom = p.bom.replace(
        "releases/1.2.3/LICENSE\t100644\t0/0",
        "releases/1.2.3/LICENSE\t100644\t501/20",
    );
    assert_eq!(
        refused(&p),
        ["usr/local/js-agent/releases/1.2.3/LICENSE: owned by 501/20, not root:wheel"]
    );
}

#[test]
fn root_with_the_admin_group_is_allowed_and_root_with_a_user_group_is_not() {
    let mut p = good();
    p.bom = p
        .bom
        .replace("./Library/Logs\t40755\t0/0", "./Library/Logs\t40755\t0/80");
    assert_eq!(problems(&p, VERSION), Vec::<String>::new());
    p.bom = p
        .bom
        .replace("./Library/Logs\t40755\t0/80", "./Library/Logs\t40755\t0/20");
    assert_eq!(refused(&p), ["Library/Logs: owned by 0/20, not root:wheel"]);
}

#[test]
fn a_link_or_a_file_where_a_directory_belongs_is_refused() {
    let mut p = good();
    p.bom = p.bom.replace(
        "./usr/local/js-agent/link\t40755",
        "./usr/local/js-agent/link\t100755",
    );
    assert_eq!(
        refused(&p),
        ["usr/local/js-agent/link: is a file, not a directory"]
    );
    let mut p = good();
    p.bom
        .push_str("./usr/local/js-agent/link/current\t120755\t0/0\t30\t1\n");
    assert_eq!(
        refused(&p),
        ["usr/local/js-agent/link/current: is not a file or a directory (mode 120755)"]
    );
}

#[test]
fn a_directory_where_a_file_belongs_is_refused() {
    let mut p = good();
    p.bom = p.bom.replace(
        "releases/1.2.3/LICENSE\t100644\t0/0\t12\t3456",
        "releases/1.2.3/LICENSE\t40755\t0/0",
    );
    assert_eq!(
        refused(&p),
        ["usr/local/js-agent/releases/1.2.3/LICENSE: is a directory, not a file"]
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
    p.bom.push_str("./var/db/js-agent-update\t40700\t0/0\n");
    assert_eq!(refused(&p), ["var/db/js-agent-update is listed twice"]);
}

#[test]
fn a_second_script_or_a_changed_postinstall_is_refused() {
    let mut p = good();
    p.scripts
        .push(("preinstall".to_owned(), b"#!/bin/sh\n".to_vec()));
    assert_eq!(
        refused(&p),
        ["the scripts are [\"postinstall\", \"preinstall\"], not [\"postinstall\"]"]
    );
    let mut p = good();
    p.scripts[0].1.extend_from_slice(b"rm -rf /\n");
    assert_eq!(
        refused(&p),
        ["postinstall is not the script of version 1.2.3"]
    );
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
            entries(VERSION).len()
        )
    );
    let mut p = good();
    p.scripts.clear();
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

// ---- the files of packaging/js-agent-macos ----

#[cfg(target_os = "linux")]
const DIR: &str = "packaging/js-agent-macos";

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

#[test]
fn the_agents_plist_names_flags_the_agent_accepts() {
    let plist = Daemon::agent("https://leases.example:7300", "mac").render();
    let words = program_arguments(&plist);
    assert_eq!(words[0], format!("{RELEASE_LINK}/js-agent"));
    let cli = js_agent::Cli::try_parse_from(
        std::iter::once("js-agent".to_owned()).chain(words.into_iter().skip(1)),
    )
    .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(cli.pool, "mac");
    assert_eq!(
        cli.native_helper_socket.as_deref(),
        Some(Path::new("/var/run/js-mac-session/socket"))
    );
}

#[test]
fn the_helpers_plist_names_flags_the_helper_accepts_and_a_socket_the_agent_dials() {
    let plist = Daemon::session("cdhash H\"abc\"", "_jsagent").render();
    let words = program_arguments(&plist);
    assert_eq!(words[0], format!("{RELEASE_LINK}/js-mac-session"));
    let args = js_mac_session::start::Args::try_parse_from(
        std::iter::once("js-mac-session".to_owned()).chain(words.into_iter().skip(1)),
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let agent = program_arguments(&Daemon::agent("https://x:1", "p").render());
    let socket = agent
        .iter()
        .position(|w| w == "--native-helper-socket")
        .map(|at| PathBuf::from(&agent[at + 1]))
        .unwrap();
    assert_eq!(args.socket, socket);
}

// The hook is built on Linux only, and so is the test of what it takes.
#[cfg(target_os = "linux")]
#[test]
fn the_declaration_is_one_the_hook_takes_for_the_unit_the_plist_labels() {
    use js_agent::update_hook::{Declaration, PREPARE_MARGIN_SECS, Params};
    let text = read(&format!("{DIR}/updater.toml"));
    let declaration = Declaration::parse(&text).unwrap();
    assert_eq!(declaration.units, [AGENT_LABEL]);
    assert_ne!(AGENT_LABEL, SESSION_LABEL);

    // The hook params: the keys the hook reads, and the release link the cutover will.
    let table: toml::Table = text.parse().unwrap();
    let mut params = table["hook"]["params"].as_table().unwrap().clone();
    let link = PathBuf::from(params["release_link"].as_str().unwrap());
    assert_eq!(link, Path::new(RELEASE_LINK));
    let inside = declaration
        .write_paths
        .iter()
        .any(|dir| link.starts_with(dir) && link != *dir && *dir != declaration.state_dir);
    assert!(
        inside,
        "the release link is in no write path but the state directory"
    );
    for (key, value) in [
        (
            "updater.artifact_digest",
            format!("sha256:{}", "a".repeat(64)),
        ),
        ("updater.staged_release", "/staged".to_owned()),
        ("updater.target_version", "1.4.0".to_owned()),
    ] {
        params.insert(key.to_owned(), toml::Value::String(value));
    }
    let text: String = params
        .iter()
        .map(|(key, value)| match value {
            toml::Value::Integer(n) => format!("\"{key}\" = {n}\n"),
            toml::Value::String(v) => format!("\"{key}\" = \"{v}\"\n"),
            other => panic!("{key}: {other:?}"),
        })
        .collect();
    let parsed = Params::parse(&text).unwrap();
    assert!(declaration.timeouts.prepare >= parsed.drain_wait_secs + PREPARE_MARGIN_SECS);
}

#[cfg(target_os = "linux")]
#[test]
fn the_declaration_names_the_native_runtime_for_the_unit_the_plist_labels() {
    use js_agent::update_hook::Declaration;
    let declaration = Declaration::parse(&read(&format!("{DIR}/updater.toml"))).unwrap();
    assert_eq!(declaration.runtime, "native");
}

// The hook is built on Linux only, and so is the test of what it takes.
#[cfg(target_os = "linux")]
#[test]
fn the_declaration_paths_are_the_ones_the_package_makes() {
    use js_agent::update_hook::Declaration;
    let declaration = Declaration::parse(&read(&format!("{DIR}/updater.toml"))).unwrap();
    let all = entries(VERSION);
    let state = declaration
        .state_dir
        .strip_prefix("/")
        .unwrap()
        .to_str()
        .unwrap();
    assert_eq!(entry(&all, state).mode, 0o700);
    for dir in &declaration.write_paths {
        let rel = dir.strip_prefix("/").unwrap().to_str().unwrap();
        // The releases directory is the parent of the release the package lays down.
        assert!(
            all.iter()
                .any(|e| e.path == rel || e.path.starts_with(&format!("{rel}/"))),
            "{rel}"
        );
    }
}
