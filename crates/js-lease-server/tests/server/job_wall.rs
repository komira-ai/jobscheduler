//! `--max-job-wall` and `server.max_job_wall`: one setting, read the same way from the flag
//! and from the file.

use std::time::Duration;

use crate::config_support::{PoolText, file, load, refusal, server};
use crate::support::{Scratch, args_with, base_args, config};

/// Asserts that the flag and the file key read every form of a ceiling to the same value,
/// that neither given is the job service's default of 30 days on both (and that the file's
/// `max_lease_wall` does not change it), and that the file refuses a ceiling the flag
/// refuses with a message naming the key that does not repeat the value. It catches a key
/// parsed differently from its flag, one that is read and dropped, a ceiling that follows
/// `max_lease_wall`, and a refusal that names the wrong place.
#[test]
fn max_job_wall_flag_and_config_key_agree() {
    let scratch = Scratch::new("max_job_wall_agree");
    let pool = PoolText::static_pool(&scratch, "hosts");
    let from_file = |line: &str| {
        load(&scratch, &file(&server(&scratch, line), &[&pool])).map(|config| config.max_job_wall())
    };
    let month = Duration::from_secs(30 * 24 * 3600);
    assert_eq!(
        config(base_args(&scratch))
            .map(|config| config.max_job_wall())
            .ok(),
        Some(month),
        "the flag's default"
    );
    // The file's `[server]` table sets `max_lease_wall = "2h"`: the job ceiling does not
    // follow it.
    assert_eq!(from_file("").ok(), Some(month), "the key's default");
    for (text, secs) in [("90m", 5400), ("12h", 43_200), ("3d", 259_200), ("1s", 1)] {
        let want = Some(Duration::from_secs(secs));
        let flag = format!("--max-job-wall={text}");
        let by_flag = config(args_with(&scratch, &[&flag])).map(|config| config.max_job_wall());
        let by_key = from_file(&format!("max_job_wall = \"{text}\""));
        assert_eq!(by_flag.ok(), want, "the flag {text}");
        assert_eq!(by_key.ok(), want, "the key {text}");
    }
    let said = |line: &str| refusal(&scratch, &file(&server(&scratch, line), &[&pool]));
    for bad in ["0", "0d", "-1d", "999ms", "soon"] {
        assert_eq!(
            said(&format!("max_job_wall = \"{bad}\"")),
            "server.max_job_wall: must be a duration of at least one second, such as 30d, 12h or 90m",
            "{bad}"
        );
    }
    assert_eq!(
        said("max_job_wall = 7"),
        "server.max_job_wall: must be a string"
    );
}
