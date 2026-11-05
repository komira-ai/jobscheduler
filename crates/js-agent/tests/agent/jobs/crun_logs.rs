//! `--log-file-cap` through the agent: the command line, the crun settings it builds and the
//! runtime opened from them, over the crun node of `crun_e2e`.

use clap::Parser as _;
use js_agent::Cli;

use super::crun_e2e::{NODE_IN_USE, Rig, crun_flags};
use crate::support::code;

/// Asserts that `--log-file-cap` reaches the crun runtime: an agent started with
/// `--log-file-cap 2097152` opens its runtime from crun settings whose live-log capture cap is
/// 2,097,152 bytes (the runtime caps a run with a sink at it, `LogPlan::live`), one started
/// without the flag from settings with the default 64 MiB, and a value below 1 MiB or above
/// 1 GiB is refused naming the flag before anything starts. It catches a flag that is parsed
/// and never handed to the runtime, a default other than the design's, and a cap out of range.
#[test]
fn the_log_file_cap_reaches_the_crun_runtime() {
    let _own = NODE_IN_USE.lock().unwrap_or_else(|e| e.into_inner());
    for (extra, want) in [
        (&["--log-file-cap", "2097152"][..], 2_097_152),
        (&[][..], js_agent::logs::DEFAULT_FILE_CAP),
    ] {
        let rig = Rig::new("jobs_crun_log_cap", extra);
        let caps = rig.machine.log_file_caps.lock().expect("caps").clone();
        assert_eq!(caps, [want], "{extra:?}");
        let ran = rig.agent.stop();
        assert_eq!(ran.status(), code(0), "{}", ran.err);
    }
    assert_eq!(js_agent::logs::DEFAULT_FILE_CAP, 64 * 1024 * 1024);
    let flags = crun_flags("h1", &[]);
    for bad in ["1048575", "1073741825"] {
        let mut args = vec![
            "js-agent".to_owned(),
            "--server".to_owned(),
            "http://127.0.0.1:1".to_owned(),
        ];
        args.extend(["--pool", "p", "--log-file-cap", bad].map(str::to_owned));
        args.extend(flags.iter().cloned());
        let why = Cli::try_parse_from(&args).expect_err(bad).to_string();
        assert!(why.contains("--log-file-cap"), "{why}");
    }
}
