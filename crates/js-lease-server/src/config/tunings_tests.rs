use clap::{CommandFactory, Parser};

use super::*;

/// The three flags alone, as the server's command line flattens them.
#[derive(Debug, Parser)]
struct Only {
    #[command(flatten)]
    tunings: Tunings,
}

/// `args` read as the three flags, or clap's refusal.
fn read(args: &[&str]) -> Result<Tunings, String> {
    let line = std::iter::once("js-lease-server").chain(args.iter().copied());
    Only::try_parse_from(line)
        .map(|only| only.tunings)
        .map_err(|err| err.to_string())
}

/// Behaviour: with no value given the engine is built with js-sched's own defaults, and each
/// value given replaces its own field only, the slack tolerance widened to js-sched's
/// `u64`. Catches a default restated here that drifts from js-sched's, one value written into
/// another's field, and a value given that does not reach the settings.
#[test]
fn each_value_given_replaces_its_own_field_and_the_rest_are_js_scheds() {
    let default = LoadConfig::default();
    assert_eq!(Tunings::default().apply(default), default);
    let balance = Tunings {
        score_balance: Some(3),
        ..Tunings::default()
    };
    assert_eq!(
        balance.apply(default),
        LoadConfig {
            score_balance: 3,
            ..default
        }
    );
    let tolerance = Tunings {
        score_slack_tolerance_ppm: Some(2_500),
        ..Tunings::default()
    };
    assert_eq!(
        tolerance.apply(default),
        LoadConfig {
            score_slack_tol_ppm: 2_500,
            ..default
        }
    );
    let share = Tunings {
        key_share_percent: Some(40),
        ..Tunings::default()
    };
    assert_eq!(
        share.apply(default),
        LoadConfig {
            key_share_percent: 40,
            ..default
        }
    );
    let elsewhere = LoadConfig {
        s_cpu_permille: 250,
        pack_limit_permille: 650,
        ..default
    };
    let all = Tunings {
        score_balance: Some(0),
        score_slack_tolerance_ppm: Some(0),
        key_share_percent: Some(100),
    };
    assert_eq!(
        all.apply(elsewhere),
        LoadConfig {
            score_balance: 0,
            score_slack_tol_ppm: 0,
            key_share_percent: 100,
            ..elsewhere
        },
        "the slack and the pack limit set before are kept"
    );
}

/// Behaviour: every value this crate accepts is one js-sched keeps as given, at both ends of
/// each range, and js-sched moves a value just outside back in. Catches a range here wider
/// than js-sched's, where an accepted value would be run as another without a word.
#[test]
fn the_ranges_are_the_ones_js_sched_keeps() {
    let kept = |tunings: Tunings| {
        let load = tunings.apply(LoadConfig::default());
        assert_eq!(load.normalised(), load, "{tunings:?}");
    };
    for balance in [*BALANCE.start(), *BALANCE.end()] {
        kept(Tunings {
            score_balance: Some(balance),
            ..Tunings::default()
        });
    }
    for tolerance in [*SLACK_TOLERANCE_PPM.start(), *SLACK_TOLERANCE_PPM.end()] {
        kept(Tunings {
            score_slack_tolerance_ppm: Some(tolerance),
            ..Tunings::default()
        });
    }
    for share in [*KEY_SHARE_PERCENT.start(), *KEY_SHARE_PERCENT.end()] {
        kept(Tunings {
            key_share_percent: Some(share),
            ..Tunings::default()
        });
    }
    let outside = LoadConfig {
        score_balance: BALANCE.end() + 1,
        key_share_percent: KEY_SHARE_PERCENT.end() + 1,
        ..LoadConfig::default()
    }
    .normalised();
    assert_eq!(outside.score_balance, *BALANCE.end());
    assert_eq!(outside.key_share_percent, *KEY_SHARE_PERCENT.end());
}

/// Behaviour: each flag reads a whole number at both ends of its range, and absent it is
/// `None`. Catches a flag that is not read, one read into another's field, and an end of a
/// range refused.
#[test]
fn the_flags_read_whole_numbers_at_both_ends_of_their_ranges() {
    assert_eq!(read(&[]), Ok(Tunings::default()));
    let low = read(&[
        "--score-balance=0",
        "--score-slack-tolerance-ppm=0",
        "--key-share-percent=0",
    ]);
    let want = Tunings {
        score_balance: Some(0),
        score_slack_tolerance_ppm: Some(0),
        key_share_percent: Some(0),
    };
    assert_eq!(low, Ok(want));
    let high = read(&[
        "--score-balance",
        "4",
        "--score-slack-tolerance-ppm",
        "1000000",
        "--key-share-percent",
        "100",
    ]);
    let want = Tunings {
        score_balance: Some(4),
        score_slack_tolerance_ppm: Some(1_000_000),
        key_share_percent: Some(100),
    };
    assert_eq!(high, Ok(want));
}

/// Behaviour: a value just outside each range, a negative one, a fraction, a percent sign
/// and an empty value are refused, naming the flag and the value, and none is clamped.
/// Catches a range wider or narrower than the documented one, and a value clamped and run.
#[test]
fn values_outside_the_ranges_or_not_whole_numbers_are_refused() {
    let cases = [
        ("--score-balance", "5", "5 is not in 0..=4"),
        (
            "--score-slack-tolerance-ppm",
            "1000001",
            "1000001 is not in 0..=1000000",
        ),
        ("--key-share-percent", "101", "101 is not in 0..=100"),
        ("--score-balance", "-1", "-1 is not in 0..=4"),
        (
            "--key-share-percent",
            "25%",
            "invalid digit found in string",
        ),
        ("--score-balance", "0.5", "invalid digit found in string"),
        (
            "--score-slack-tolerance-ppm",
            "",
            "cannot parse integer from empty string",
        ),
    ];
    for (flag, value, why) in cases {
        let arg = format!("{flag}={value}");
        let said = read(&[arg.as_str()]).unwrap_err();
        let name = flag
            .trim_start_matches("--")
            .replace('-', "_")
            .to_uppercase();
        let head = format!("invalid value '{value}' for '{flag} <{name}>': {why}");
        assert!(said.contains(&head), "{flag}={value}: {said}");
    }
}

/// Behaviour: the three flags are the server's own, with their long names and no default, so
/// a flag left out is told apart from one given at js-sched's default. Catches a flag renamed
/// or given a default that would hide js-sched's.
#[test]
fn the_flags_have_their_names_and_no_default() {
    let command = Only::command();
    let rows: Vec<(String, usize)> = command
        .get_arguments()
        .filter_map(|arg| {
            let long = arg.get_long()?.to_owned();
            Some((long, arg.get_default_values().len()))
        })
        .collect();
    assert_eq!(
        rows,
        [
            ("score-balance".to_owned(), 0),
            ("score-slack-tolerance-ppm".to_owned(), 0),
            ("key-share-percent".to_owned(), 0),
        ]
    );
}
