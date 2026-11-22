//! Only the files of `KEEPERS` may start a program that keeps its starter's privacy (TCC)
//! responsibility (`js_mac_session::disclaim::Program::keep_responsibility` and its reason,
//! `disclaim::Kept`): every other start of `js-agent` and `js-mac-session` stays disclaimed.
//! The check that says so finds a keep outside them, in each form it can be written in, lets
//! one in a listed file pass, and refuses a listed file it cannot reach.
//!
//! The cases build small trees in a scratch directory (see `spawn_tree`), plus one copy of
//! the real sources with one keep added. Nothing in the repository is edited.

use xtask::spawn_sites::{KEEPERS, SPAWNER};

mod spawn_tree;
use spawn_tree::{Tree, at};

/// A probe module of the agent, like `facts.rs`, that starts its program keeping the
/// responsibility: the start a probe must never make (it would hold the agent's grant).
const PROBE: &str = r#"use js_mac_session::disclaim::{Kept, Program};
pub fn ran() {
    let mut probe = Program::new("/usr/bin/sw_vers", [""; 0]);
    probe.keep_responsibility(Kept::Job);
    let _ = probe.output();
}
"#;

/// The agent's library with the probe module.
const AGENT: &str = "mod facts;\n";

/// A keep in a probe module is found, both the call and the reason's import and use, with
/// their lines; the program's start through the spawner is not a site. Catches a check with
/// no rule for the keep, under which a probe, a firewall tool or the VM launcher could hold
/// the agent's Full Disk Access unseen.
#[test]
fn a_keep_responsibility_call_outside_the_keepers_is_found() {
    let tree = Tree::new("keep-outside");
    tree.put("crates/js-agent/src/lib.rs", AGENT);
    tree.put("crates/js-agent/src/facts.rs", PROBE);
    assert_eq!(
        tree.sites(),
        at(
            "crates/js-agent/src/facts.rs",
            &[
                (1, "Kept outside KEEPERS"),
                (4, "Kept outside KEEPERS"),
                (4, "keep_responsibility outside KEEPERS"),
            ]
        )
    );
}

/// The same module, listed as a keeper, passes: a job's or a supervisor's start may keep the
/// responsibility. A module that is not listed still fails beside it, and a listed file is
/// still held to the spawner rule. Catches a rule that refuses every keep (no supervisor
/// could then start a job that reads its build trees), or one that a single listed file
/// turns off for the whole tree or for starts that bypass the spawner.
#[test]
fn a_keep_in_a_listed_file_passes() {
    let tree = Tree::new("keep-listed");
    tree.put("crates/js-agent/src/lib.rs", "mod facts;\nmod supervise;\n");
    tree.put("crates/js-agent/src/supervise.rs", PROBE);
    let listed = ["crates/js-agent/src/supervise.rs"];
    tree.put("crates/js-agent/src/facts.rs", "");
    assert_eq!(tree.sites_keeping(&listed), Vec::<String>::new());
    tree.put("crates/js-agent/src/facts.rs", PROBE);
    assert_eq!(
        tree.sites_keeping(&listed),
        at(
            "crates/js-agent/src/facts.rs",
            &[
                (1, "Kept outside KEEPERS"),
                (4, "Kept outside KEEPERS"),
                (4, "keep_responsibility outside KEEPERS"),
            ]
        )
    );
    tree.put(
        "crates/js-agent/src/supervise.rs",
        "fn f() { std::process::Command::new(\"x\"); }\n",
    );
    tree.put("crates/js-agent/src/facts.rs", "");
    assert_eq!(
        tree.sites_keeping(&listed),
        at(
            "crates/js-agent/src/supervise.rs",
            &[(1, "Command::new"), (1, "process::Command")]
        )
    );
}

/// The reason imported under another name, from the module renamed, through a glob of the
/// reason's variants, or named by its full path, is found by the original name, as a renamed
/// `libc` is; and the call by its path, or in a macro's tokens, too. Catches a keep hidden
/// behind a rename, which the words `Kept` and `keep_responsibility` would not show.
#[test]
fn a_renamed_import_of_the_kept_type_is_found() {
    let tree = Tree::new("keep-renamed");
    tree.put("crates/js-agent/src/lib.rs", AGENT);
    tree.put(
        "crates/js-agent/src/facts.rs",
        r#"use js_mac_session::disclaim::Kept as Why;
use js_mac_session::disclaim::{self as d, Kept::*};
fn f(p: &mut d::Program) {
    d::Program::keep_responsibility(p, Why::Job);
    let _ = d::Kept::Supervisor;
    let _ = js_mac_session::disclaim::Kept::Job;
    log!(p.keep_responsibility(Supervisor));
}
"#,
    );
    assert_eq!(
        tree.sites(),
        at(
            "crates/js-agent/src/facts.rs",
            &[
                (1, "Kept outside KEEPERS"),
                (2, "Kept outside KEEPERS"),
                (4, "keep_responsibility outside KEEPERS"),
                (5, "Kept outside KEEPERS"),
                (6, "Kept outside KEEPERS"),
                (7, "keep_responsibility outside KEEPERS"),
            ]
        )
    );
}

/// What only resembles a keep is not a site: a type of the agent's own that is also called
/// `Kept` (the agent has two), a field or a function called `kept` or `keep`, and a word in a
/// string or a comment. Catches a rule that flags the agent's own names, which would leave
/// the real tree red.
#[test]
fn names_that_only_resemble_a_keep_are_not_sites() {
    let tree = Tree::new("keep-alike");
    tree.put("crates/js-agent/src/lib.rs", AGENT);
    tree.put(
        "crates/js-agent/src/facts.rs",
        r#"struct Kept { kept: bool }
// keep_responsibility(Kept::Job)
fn keep(k: &Kept) -> &str { let _ = k.kept; "keep_responsibility disclaim::Kept" }
"#,
    );
    assert_eq!(tree.sites(), Vec::<String>::new());
}

/// A listed file that no module of the checked crates is, whether it is missing or merely not
/// declared, stops the check with its name, and so does the spawner listed as one (it is not
/// read). Catches a list that names a moved or renamed supervisor file, under which the rule
/// would hold for a file nothing builds, never for the one that starts jobs.
#[test]
fn a_keeper_the_walk_does_not_reach_is_an_error() {
    let tree = Tree::new("keep-unreached");
    tree.put("crates/js-agent/src/stray.rs", PROBE);
    for listed in [
        "crates/js-agent/src/supervise.rs",
        "crates/js-agent/src/stray.rs",
        SPAWNER,
    ] {
        assert_eq!(
            tree.error_keeping(&[listed]),
            format!(
                "{listed}: listed in KEEPERS, but no module of the checked crates is this file"
            )
        );
    }
}

/// The real sources, copied, with a keep added to the agent's host probes: the check reaches
/// that module of the real tree and names the file and line, while the real list of keepers
/// holds no probe. Catches a rule that only toy trees exercise, or a real list that lets a
/// probe keep the responsibility.
#[test]
fn a_keep_added_to_the_real_agent_is_found() {
    let tree = Tree::real("keep-real");
    let last = tree.append(
        "crates/js-agent/src/facts.rs",
        "\nfn stray(p: &mut js_mac_session::disclaim::Program) {\n    p.keep_responsibility(js_mac_session::disclaim::Kept::Job);\n}\n",
    );
    assert_eq!(
        tree.sites(),
        at(
            "crates/js-agent/src/facts.rs",
            &[
                (last + 3, "Kept outside KEEPERS"),
                (last + 3, "keep_responsibility outside KEEPERS"),
            ]
        )
    );
}

/// The real list is the supervisor's two starts and nothing else: the copied real sources pass
/// with the keeps of those two files, and a keep added to another file of the supervisor's own
/// module is found, with its line. Catches a list that lets the whole module, or a third file
/// of it, keep the responsibility.
#[test]
fn only_the_two_starts_of_the_supervisor_may_keep() {
    let supervisor = "crates/js-agent/src/mac_supervisor";
    assert_eq!(
        KEEPERS,
        [
            // Proof branch only: the development verb is the third keeper here.
            "crates/js-agent/src/dev_exec.rs",
            format!("{supervisor}/launch.rs").as_str(),
            format!("{supervisor}/run.rs").as_str(),
        ]
    );
    let tree = Tree::real("keep-supervisor");
    assert_eq!(tree.sites(), Vec::<String>::new());
    let record = format!("{supervisor}/record.rs");
    let last = tree.append(
        &record,
        "\nfn stray(p: &mut js_mac_session::disclaim::Program) {\n    p.keep_responsibility(js_mac_session::disclaim::Kept::Job);\n}\n",
    );
    assert_eq!(
        tree.sites(),
        at(
            &record,
            &[
                (last + 3, "Kept outside KEEPERS"),
                (last + 3, "keep_responsibility outside KEEPERS"),
            ]
        )
    );
}
