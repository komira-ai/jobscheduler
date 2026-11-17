//! `--warm-sandboxes` reaches the crun runtime the agent opens, over the crun node of
//! `crun_e2e`.

use clap::Parser as _;
use js_agent::Cli;

use super::crun_e2e::{NODE_IN_USE, Rig, crun_flags};
use crate::support::code;

/// Asserts that `--warm-sandboxes` reaches the crun runtime: an agent started without the flag
/// opens its runtime from crun settings that keep 2 warm sandboxes per image, and one started
/// with `--warm-sandboxes 0` or `--warm-sandboxes 64` from settings that keep 0 or 64. It
/// catches a flag that is parsed and never handed to the runtime, and a default other than 2.
#[test]
fn the_warm_sandbox_count_reaches_the_crun_runtime() {
    let _own = NODE_IN_USE.lock().unwrap_or_else(|e| e.into_inner());
    for (extra, want) in [
        (&[][..], 2),
        (&["--warm-sandboxes", "0"][..], 0),
        (&["--warm-sandboxes", "64"][..], 64),
    ] {
        let rig = Rig::new("jobs_crun_warm_sandboxes", extra);
        let seen = rig.machine.warm_sandboxes.lock().expect("warm").clone();
        assert_eq!(seen, [want], "{extra:?}");
        let ran = rig.agent.stop();
        assert_eq!(ran.status(), code(0), "{}", ran.err);
    }
    assert_eq!(js_agent::crun::DEFAULT_WARM_SANDBOXES, 2);
}

/// Asserts that a count above 64, a negative one and one that is not a number are refused
/// naming `--warm-sandboxes` before anything starts. It catches a missing upper bound.
#[test]
fn a_warm_sandbox_count_out_of_range_or_not_a_number_is_refused() {
    let flags = crun_flags("h1", &[]);
    for bad in ["65", "-1", "abc"] {
        let mut args = vec![
            "js-agent".to_owned(),
            "--server".to_owned(),
            "http://127.0.0.1:1".to_owned(),
        ];
        args.extend(["--pool", "p"].map(str::to_owned));
        args.push(format!("--warm-sandboxes={bad}"));
        args.extend(flags.iter().cloned());
        let why = Cli::try_parse_from(&args).expect_err(bad).to_string();
        assert!(why.contains("--warm-sandboxes"), "{bad}: {why}");
    }
}
