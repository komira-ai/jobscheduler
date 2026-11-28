//! The server's `silence_limit` (`[server]` of the config file): its default, what the file
//! sets, and the values it refuses.

use std::time::Duration;

use super::{DEFAULT_SILENCE_LIMIT, JobLimits};
use crate::config::clients_rig::ClientsRig;

/// The silence limit a file with the `[server]` lines `lines` gives the job pass, or the refusal.
fn limit_of(rig: &ClientsRig, lines: &str) -> Result<Duration, String> {
    let config = rig.config_of_file(&rig.file(lines, ""));
    config
        .map(|config| config.agents.job_limits.silence_limit)
        .map_err(|refused| format!("{refused}"))
}

// Catches a default other than ten minutes, a flag-started server or a file without the key
// that reads another, a key the file sets that does not reach the job pass, and a pool's limits
// read in place of the server's.
#[test]
fn the_silence_limit_is_ten_minutes_unless_the_file_sets_it() {
    assert_eq!(DEFAULT_SILENCE_LIMIT, Duration::from_secs(10 * 60));
    assert_eq!(JobLimits::DEFAULT.silence_limit, DEFAULT_SILENCE_LIMIT);
    let rig = ClientsRig::new("silence_limit_set");
    let flags = rig.config_of_flags(&[]).unwrap();
    assert_eq!(flags.agents.job_limits.silence_limit, DEFAULT_SILENCE_LIMIT);
    assert_eq!(limit_of(&rig, ""), Ok(DEFAULT_SILENCE_LIMIT));
    let set = limit_of(&rig, "silence_limit = \"2h\"");
    assert_eq!(set, Ok(Duration::from_secs(2 * 3600)));
    let file = |lines: &str| rig.config_of_file(&rig.file(lines, "")).unwrap();
    let set = file("silence_limit = \"45m\"").agents.job_limits;
    let unset = file("").agents.job_limits;
    let pool = unset.with_silence_limit(Duration::from_secs(45 * 60));
    assert_eq!(set, pool, "a pool's limits moved with the server's key");
}

// Catches a server embedded in a simulation that cannot move the limit, or that moves another
// limit with it: the builder sets the silence limit of a server started on flags and nothing else.
#[test]
fn an_embedding_server_sets_its_own_silence_limit() {
    let rig = ClientsRig::new("silence_limit_builder");
    let flags = rig.config_of_flags(&[]).unwrap();
    let before = flags.agents.job_limits;
    let hour = Duration::from_secs(3600);
    let set = flags.with_silence_limit(hour).agents.job_limits;
    assert_eq!(set, before.with_silence_limit(hour));
    assert_eq!(set.silence_limit, hour);
}

// Catches a zero limit (every host silent at once), a text that is not a duration, and a number
// where a duration's text belongs, each accepted rather than refused at start naming the key.
#[test]
fn a_zero_or_mistyped_silence_limit_is_refused_naming_the_key() {
    let rig = ClientsRig::new("silence_limit_refused");
    let duration = "server.silence_limit: must be a positive duration such as 600s, 10m or 1h";
    let cases = [
        ("silence_limit = \"0s\"", duration),
        ("silence_limit = \"soon\"", duration),
        (
            "silence_limit = 600",
            "server.silence_limit: must be a string",
        ),
    ];
    for (line, said) in cases {
        let refused = limit_of(&rig, line).unwrap_err();
        assert!(refused.contains(said), "{line}: {refused}");
    }
}
