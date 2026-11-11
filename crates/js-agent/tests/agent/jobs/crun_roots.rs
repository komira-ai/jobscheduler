//! The mount roots of a crun agent: from its flags to the plan its runtime is opened from.

use clap::Parser as _;
use js_agent::Cli;
use js_agent::cli_runtime::Settings;
use js_agent::lifecycle::Local;
use js_testkit::dir::TestDir;

use crate::crun_host::Fixture;

/// Asserts that a crun agent given a `--mount-root` and a `--mount-root-ro` carries both, by
/// name and canonical path and each with its access, from its flags into the plan its runtime
/// is opened from (`CrunPlan::mount_roots`), where the runtime opens them and finds the root
/// a host path lies beneath; and that one given none carries none. Catches roots the
/// admission holds a start to but the runtime never opens (every lookup would then be
/// refused), and a read-only root that reaches the runtime as read-write.
#[test]
fn the_mount_roots_reach_the_crun_plan_with_their_access() {
    let (uid, gid) = (
        rustix::process::getuid().as_raw(),
        rustix::process::getgid().as_raw(),
    );
    let fixture = Fixture::with_ids("crun-roots", uid, gid);
    let roots = TestDir::under(std::env::temp_dir(), "crun-roots-dirs");
    for root in ["data", "tools"] {
        std::fs::create_dir(roots.path().join(root)).expect("a root");
    }
    let path = |name: &str| roots.path().join(name).display().to_string();
    let plan_of = |extra: &[String]| {
        let mut args = vec!["js-agent", "--server", "http://127.0.0.1:1", "--pool", "p"];
        args.extend(["--runtime", "crun", "--job-user", Fixture::USER]);
        let (state, scratch) = (fixture.state.display(), fixture.scratch.display());
        let given = [
            format!("--state-root={state}"),
            format!("--scratch-root={scratch}"),
        ];
        let all: Vec<&str> = args
            .into_iter()
            .chain(given.iter().map(String::as_str))
            .chain(extra.iter().map(String::as_str))
            .collect();
        let cli = Cli::try_parse_from(all).expect("a command line");
        let settings =
            Settings::for_crun(fixture.crun.clone(), Fixture::USER, (uid, gid), Vec::new());
        let euid = rustix::process::geteuid().as_raw();
        let local = Local::from_cli_crun(&cli, euid, settings).expect("the flags pass");
        let settings = local.container.expect("containers are enabled");
        settings.crun().expect("crun settings").mount_roots.clone()
    };
    let given = plan_of(&[
        format!("--mount-root=data={}", path("data")),
        format!("--mount-root-ro=tools={}", path("tools")),
    ]);
    let canonical = |name: &str| std::fs::canonicalize(roots.path().join(name)).expect("there");
    let named: Vec<(String, std::path::PathBuf)> = given
        .named()
        .map(|(name, root)| (name.to_owned(), root.to_owned()))
        .collect();
    assert_eq!(
        named,
        [
            ("data".to_owned(), canonical("data")),
            ("tools".to_owned(), canonical("tools")),
        ]
    );
    let beneath = |root: &str| {
        let host = canonical(root).join("x").display().to_string();
        given
            .beneath(&host)
            .map(|found| (found.name.to_owned(), found.read_only))
    };
    assert_eq!(beneath("data"), Some(("data".to_owned(), false)));
    assert_eq!(beneath("tools"), Some(("tools".to_owned(), true)));
    assert_eq!(plan_of(&[]).named().count(), 0);
}
