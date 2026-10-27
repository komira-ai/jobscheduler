//! The cleanup of a `create` whose outcome is not known, and that it removes only what
//! is the agent's own container of the run.

use js_agent::cli_runtime::CliRuntime;
use js_agent::lifecycle::{ContainerRuntime, label};

use super::super::scripted::Scripted;
use super::super::{HOST, docker, made_at, open, platform, sees_the_engines_pid, settings, spec};
use super::{start, strings};

/// Asserts that a `create` whose outcome is not known leaves nothing behind: the call is
/// killed after the daemon made the container (a nonzero exit), or it exits 0 and prints
/// something that is no id (nothing, a message, an id of 12 digits). In each case the
/// container is removed by the name the agent gave it, `start` is never called, the error
/// is the create's, and a second start of the same run can use the name again. Before
/// this, a labelled container nobody knew stayed on the host, and a restarted agent
/// adopted it as a running job. It catches an orphan after a timeout.
#[test]
fn an_ambiguous_create_is_cleaned_up_by_name() {
    let cases: [(Result<&str, ()>, &str); 4] = [
        (Err(()), "create: /bin/sh: exit status: 1"),
        (Ok(""), "create: unreadable output"),
        (Ok("Unable to find image\n"), "create: unreadable output"),
        (Ok("0123456789ab\n"), "create: unreadable output"),
    ];
    for (outcome, text) in cases {
        let world = docker();
        let mut rt = open(&world);
        world.ambiguous_create(outcome);
        let said = rt.create_and_start(&spec("r1")).unwrap_err();
        assert_eq!(said.0, text, "{outcome:?}");
        assert!(
            world.ids().is_empty(),
            "{outcome:?}: the orphan was removed"
        );
        assert_eq!(
            world.calls_of("rm"),
            [strings(&[
                "rm",
                "--force",
                "--volumes",
                "--",
                &format!("{:064x}", 1)
            ])],
            "{outcome:?}: removed by the id the lookup found"
        );
        assert!(world.calls_of("start").is_empty(), "{outcome:?}");
        start(&mut rt, &world, "r1");
        assert_eq!(world.ids().len(), 1, "the name was free again");
    }
}

/// Asserts what the error says when the orphan cannot be removed: the removal failed and
/// the container is still listed (so the note names the failure), or the removal failed
/// and the listing failed too (the agent cannot say), and that the container then stays
/// visible to the next listing as the agent's own. It catches a cleanup failure that is
/// swallowed.
#[test]
fn a_cleanup_that_fails_is_reported() {
    let world = docker();
    let mut rt = open(&world);
    world.ambiguous_create(Err(()));
    world.fail("rm", 1);
    let said = rt.create_and_start(&spec("r1")).unwrap_err();
    assert_eq!(
        said.0,
        "create: /bin/sh: exit status: 1; the container could not be removed: rm: /bin/sh: exit status: 1"
    );
    assert_eq!(world.ids().len(), 1);

    world.ambiguous_create(Ok(""));
    world.fail("rm", 1);
    world.fail("ps", 1);
    let said = rt.create_and_start(&spec("r2")).unwrap_err();
    assert_eq!(
        said.0,
        "create: unreadable output; the container could not be removed: ps: /bin/sh: exit status: 1"
    );
}

/// A runtime of another agent (another host id) over the same `world`.
pub(super) fn other_agent(world: &Scripted) -> CliRuntime {
    let runner = Box::new(world.clone());
    CliRuntime::open(settings(), OTHER, &platform("linux"), made_at, runner)
        .unwrap()
        .with_pid_seen(sees_the_engines_pid)
}

/// The host id of the other agent.
const OTHER: &str = "h-fedcba9876543210";

/// The name this agent gives the container of `run_id`.
fn our_name(run_id: &str) -> String {
    let probe = docker();
    start(&mut open(&probe), &probe, run_id);
    probe.names().remove(0)
}

/// Asserts that a failed `create` of a run never removes another agent's container of
/// the same run on the same runtime: the other agent's job is still there afterwards, no
/// `rm` was made, and both agents can run the same run id side by side. It catches a
/// cleanup that removes by a name any agent can have.
#[test]
fn a_failed_create_leaves_another_agents_container_of_the_same_run() {
    let world = docker();
    let their = start(&mut other_agent(&world), &world, "r1");
    let mut rt = open(&world);
    world.fail("create", 1);
    let said = rt.create_and_start(&spec("r1")).unwrap_err();
    assert_eq!(said.0, "create: /bin/sh: exit status: 1");
    assert_eq!(world.ids(), std::slice::from_ref(&their.0));
    assert!(world.calls_of("rm").is_empty());
    let mine = rt.create_and_start(&spec("r1")).unwrap();
    assert_eq!(world.ids().len(), 2, "the names do not collide");
    assert!(world.ids().contains(&their.0) && world.ids().contains(&mine.0));
}

/// Asserts that a container with the name the agent would give a run, which is not the
/// agent's container of that run, is never removed by the cleanup of a failed `create`
/// (the name is taken, so the `create` fails): an operator's container without our
/// labels, another agent's container of the run, the agent's own container of another
/// run, and one with only the agent's label. No `rm` is made. It catches a removal by
/// name alone.
#[test]
fn a_container_with_our_name_but_not_our_labels_is_never_removed() {
    let name = our_name("r1");
    let cases: [(&str, Vec<(&str, &str)>); 4] = [
        ("an operator's", vec![("team", "ops")]),
        (
            "another agent's",
            vec![(label::RUN, "r1"), (label::AGENT, OTHER)],
        ),
        (
            "ours of another run",
            vec![(label::RUN, "r2"), (label::AGENT, HOST)],
        ),
        ("ours without a run", vec![(label::AGENT, HOST)]),
    ];
    for (whose, labels) in cases {
        let world = docker();
        let decoy = world.leak_named(&name, &labels);
        let mut rt = open(&world);
        let said = rt.create_and_start(&spec("r1")).unwrap_err();
        assert_eq!(said.0, "create: /bin/sh: exit status: 1", "{whose}");
        assert_eq!(world.ids(), [decoy], "{whose} container was removed");
        assert!(world.calls_of("rm").is_empty(), "{whose}");
    }
}

/// Asserts that the cleanup looks the container up by its exact name and both labels,
/// and removes what it finds by its 64-digit id: the `ps` filters are the anchored name
/// (a dot of the run id escaped), the run label with its value and the agent label with
/// its value. An answer line that is not an id is not passed to `rm`. It catches a
/// name filter that matches a longer name, and a removal by name.
#[test]
fn the_cleanup_finds_the_container_by_name_and_both_labels() {
    let world = docker();
    let mut rt = open(&world);
    world.ambiguous_create(Err(()));
    rt.create_and_start(&spec("a.b")).unwrap_err();
    let name = our_name("a.b");
    let ps = world.calls_of("ps").remove(0);
    assert_eq!(
        ps,
        strings(&[
            "ps",
            "-a",
            "-q",
            "--no-trunc",
            "--filter",
            &format!("name=^{}$", name.replace('.', "\\.")),
            "--filter",
            "label=jobscheduler.dev/run-id=a.b",
            "--filter",
            &format!("label=jobscheduler.dev/agent={HOST}"),
        ])
    );
    let rm = world.calls_of("rm");
    assert_eq!(rm.len(), 1);
    assert_eq!(
        rm[0][..4],
        strings(&["rm", "--force", "--volumes", "--"])[..]
    );
    assert_eq!(rm[0][4].len(), 64, "removed by id, not by name");

    let world = docker();
    let mut rt = open(&world);
    world.ambiguous_create(Err(()));
    world.say("ps", "not an id\n\n");
    rt.create_and_start(&spec("r1")).unwrap_err();
    assert!(world.calls_of("rm").is_empty());
}
