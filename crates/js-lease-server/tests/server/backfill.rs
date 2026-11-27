//! `--backfill-bounds-enforced` and `server.backfill_bounds_enforced`, `--backfill-margin` and
//! `server.backfill_margin`: whether the placement engine trusts a job's wall limit for
//! backfill, and the margin it keeps. These tests read the flags and the keys and their
//! refusals into the engine's setting ([`Config::backfill`]); what the setting does to a
//! placement is tested in js-sched (`tests/backfill.rs`, `tests/whole_node_estimates.rs`).

use std::ffi::OsString;
use std::time::Duration;

use js_lease_server::{Cli, Config};
use js_sched::Backfill;

use crate::config_support::{PoolText, file, load, refusal, server};
use crate::support::{Scratch, args_with, base_args, config};

const HOUR: Duration = Duration::from_secs(3_600);
const MONTH: Duration = Duration::from_secs(30 * 24 * 3_600);

fn parsed(args: Vec<OsString>) -> Result<Config, String> {
    let cli = Cli::try_parse_from(args).map_err(|err| err.to_string())?;
    Config::from_cli(cli).map_err(|err| err.to_string())
}

/// Asserts the flags: absent, bounds are enforced up to the most a job's wall limit may be
/// (30 days, or `--max-job-wall`) with a minute's margin, as before the flags; `false`
/// enforces nothing; a margin given is kept with either switch. Catches a default that turns
/// backfill off, a flag that is read and dropped, and a ceiling that does not follow
/// `--max-job-wall`.
#[test]
fn the_flags_set_the_switch_and_the_margin() {
    let scratch = Scratch::new("backfill_flags");
    let backfill = |extra: &[&str]| config(args_with(&scratch, extra)).unwrap().backfill();
    assert_eq!(
        config(base_args(&scratch)).unwrap().backfill(),
        Backfill::enforced(MONTH)
    );
    assert_eq!(
        backfill(&["--max-job-wall", "12h"]),
        Backfill::enforced(12 * HOUR)
    );
    assert_eq!(
        backfill(&["--backfill-bounds-enforced", "true"]),
        Backfill::enforced(MONTH)
    );
    assert_eq!(
        backfill(&["--backfill-bounds-enforced", "false"]),
        Backfill::default()
    );
    let margin = Duration::from_secs(90);
    assert_eq!(
        backfill(&["--backfill-margin", "90s"]),
        Backfill::enforced(MONTH).with_margin(margin)
    );
    assert_eq!(
        backfill(&[
            "--backfill-bounds-enforced",
            "false",
            "--backfill-margin",
            "90s"
        ]),
        Backfill::default().with_margin(margin)
    );
}

/// Asserts the refusals of the flags, each by its whole message where the server writes it:
/// a margin of zero or not a duration, a switch other than `true` or `false`, and either flag
/// without `--agent-pool-config`. Catches a zero margin accepted (a late lease would read as
/// ending now again), a switch read from any word, and a flag accepted where nothing reads it.
#[test]
fn the_flags_refuse_what_they_cannot_read() {
    let scratch = Scratch::new("backfill_flags_refused");
    for bad in ["0", "soon", ""] {
        let said = parsed(args_with(&scratch, &["--backfill-margin", bad]))
            .err()
            .unwrap();
        assert!(
            said.contains("'--backfill-margin <BACKFILL_MARGIN>'"),
            "{bad}: {said}"
        );
    }
    let said = parsed(args_with(&scratch, &["--backfill-bounds-enforced", "yes"]))
        .err()
        .unwrap();
    assert!(said.contains("--backfill-bounds-enforced"), "{said}");
    for (flag, value) in [
        ("--backfill-margin", "1m"),
        ("--backfill-bounds-enforced", "true"),
    ] {
        let mut bare = crate::support::without_agent_pool(&scratch);
        bare.extend([flag, value].map(OsString::from));
        let said = parsed(bare).err().unwrap();
        assert_eq!(said, format!("{flag}: requires --agent-pool-config"));
    }
}

/// Asserts the keys: absent, bounds are enforced up to the shorter of `max_lease_wall` (the
/// test file sets `"2h"`) and `max_job_wall` with a minute's margin; `false` enforces
/// nothing; a margin is kept; a `max_job_wall` under `max_lease_wall` is the ceiling. And the
/// refusals, each by its whole message. Catches a key read and dropped, a ceiling taken from
/// the longer wall, and a key of the wrong type accepted.
#[test]
fn the_keys_set_the_switch_and_the_margin() {
    let scratch = Scratch::new("backfill_keys");
    let pool = PoolText::joined("hosts");
    let text = |lines: &str| file(&server(&scratch, lines), &[&pool]);
    let backfill = |lines: &str| load(&scratch, &text(lines)).unwrap().backfill();
    assert_eq!(backfill(""), Backfill::enforced(2 * HOUR));
    assert_eq!(backfill("max_job_wall = \"1h\""), Backfill::enforced(HOUR));
    assert_eq!(
        backfill("backfill_bounds_enforced = true"),
        Backfill::enforced(2 * HOUR)
    );
    assert_eq!(
        backfill("backfill_bounds_enforced = false"),
        Backfill::default()
    );
    let margin = Duration::from_secs(120);
    assert_eq!(
        backfill("backfill_margin = \"2m\""),
        Backfill::enforced(2 * HOUR).with_margin(margin)
    );
    assert_eq!(
        backfill("backfill_bounds_enforced = false\nbackfill_margin = \"2m\""),
        Backfill::default().with_margin(margin)
    );
    let said = |lines: &str| refusal(&scratch, &text(lines));
    assert_eq!(
        said("backfill_bounds_enforced = \"no\""),
        "server.backfill_bounds_enforced: must be true or false"
    );
    for bad in ["\"0\"", "\"soon\""] {
        assert_eq!(
            said(&format!("backfill_margin = {bad}")),
            "server.backfill_margin: must be a positive duration such as 600s, 10m or 1h",
            "{bad}"
        );
    }
    assert_eq!(
        said("backfill_margin = 60"),
        "server.backfill_margin: must be a string"
    );
}
