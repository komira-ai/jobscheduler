//! `--job-retention` and `server.job_retention`: one setting, read the same way from the flag
//! and from the file.

use std::time::Duration;

use js_jobs::Retention;

use crate::config_support::{PoolText, file, load, refusal, server};
use crate::support::{Scratch, args_with, base_args, config};

/// Asserts that the flag and the file key read every form of a window to the same value,
/// that neither given is seven days on both, and that the file refuses a window the flag
/// refuses with a message naming the key that does not repeat the value. It catches a key
/// parsed differently from its flag, one that is read and dropped, and a refusal that names
/// the wrong place.
#[test]
fn job_retention_flag_and_config_key_agree() {
    let scratch = Scratch::new("job_retention_agree");
    let pool = PoolText::static_pool(&scratch, "hosts");
    let from_file = |line: &str| {
        load(&scratch, &file(&server(&scratch, line), &[&pool]))
            .map(|config| config.job_retention())
    };
    let week = Retention::new(Duration::from_secs(604_800)).expect("a window");
    assert_eq!(
        config(base_args(&scratch))
            .map(|config| config.job_retention())
            .ok(),
        Some(week),
        "the flag's default"
    );
    assert_eq!(from_file("").ok(), Some(week), "the key's default");
    for (text, secs) in [
        ("90m", 5400),
        ("36h", 129_600),
        ("3d", 259_200),
        ("7d", 604_800),
    ] {
        let want = Retention::new(Duration::from_secs(secs)).expect("a window");
        let flag = format!("--job-retention={text}");
        let by_flag = config(args_with(&scratch, &[&flag])).map(|config| config.job_retention());
        let by_key = from_file(&format!("job_retention = \"{text}\""));
        assert_eq!(by_flag.ok(), Some(want), "the flag {text}");
        assert_eq!(by_key.ok(), Some(want), "the key {text}");
    }
    let said = |line: &str| refusal(&scratch, &file(&server(&scratch, line), &[&pool]));
    for bad in ["0", "0d", "-1d", "59s", "soon"] {
        assert_eq!(
            said(&format!("job_retention = \"{bad}\"")),
            "server.job_retention: must be a duration of at least one minute, such as 7d, 36h or 90m",
            "{bad}"
        );
    }
    assert_eq!(
        said("job_retention = 7"),
        "server.job_retention: must be a string"
    );
}
