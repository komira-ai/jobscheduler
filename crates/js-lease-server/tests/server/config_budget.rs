//! The server-wide spend totals and `live_until` in the configuration file: the keys of
//! `[server]` and `[budget.<kind>]`, the refusals between them, and a go that has expired
//! by the time the file is read. Nothing here waits.

use js_lease::{ManualClock, Timestamp};
use js_lease_server::{Config, Launch, OwnerOnlyFiles, SharedClock};
use js_scaler::{Budgets, Millis, Totals};

use crate::config_support::{PoolText, file, kinds, load, quote, refusal, server};
use crate::support::Scratch;

/// 2027-01-01T00:00:00Z in milliseconds since the epoch.
const NEW_YEAR: u64 = 1_798_761_600_000;

/// A `[server]` table that lets an elastic pool start, with `extra` lines in it.
fn server_with(scratch: &Scratch, extra: &str) -> String {
    let listeners = "listen = \"127.0.0.1:0\"\nagent_listen = \"127.0.0.2:0\"";
    server(scratch, &format!("{listeners}\n{extra}"))
}

/// The agent pin a live pool needs.
const AGENT: &str = "{ x86_64 = { url = \"https://pkgs.example.org/js-agent-x86_64\", sha256 = \"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\" } }";

/// The file's text with a `[budget.lambdalabs]` table of `lines` after the pools.
fn with_budget(text: &str, lines: &str) -> String {
    format!("{text}\n[budget.lambdalabs]\n{lines}\n")
}

/// Loads `text` on a clock that reads `now` milliseconds.
fn load_at(
    scratch: &Scratch,
    text: &str,
    now: u64,
) -> Result<Config, js_lease_server::StartupError> {
    let path = scratch.write("js.toml", text, 0o644);
    let clock = SharedClock::new(ManualClock::new(Timestamp::from_millis(now)));
    Config::from_config_file(&path, &kinds(), &OwnerOnlyFiles, clock)
}

/// Asserts that the three totals of `[server]` and of `[budget.lambdalabs]` reach the plan
/// as cents, each in its own scope: the server's keys do not leak into the kind's table
/// and the reverse. It catches a key read into the wrong scope or in dollars.
#[test]
fn the_totals_reach_the_plan_each_in_its_scope() {
    let scratch = Scratch::new("budget_plan");
    let head = server_with(
        &scratch,
        "max_spend_per_day_total = \"50.00\"\nmax_spend_per_month_total = \"700.50\"\nmax_spend_since_go_total = \"900\"",
    );
    let pool = PoolText::elastic(&scratch);
    let text = with_budget(
        &file(&head, &[&pool]),
        "max_spend_per_month_total = \"300.00\"",
    );
    let config = load(&scratch, &text).unwrap();
    let budgets = &config.elastic_pool().unwrap().budgets;
    let server = Totals {
        per_day: Some(5_000),
        per_month: Some(70_050),
        since_go: Some(90_000),
    };
    let kind = Totals {
        per_day: None,
        per_month: Some(30_000),
        since_go: None,
    };
    assert_eq!(budgets.server, server);
    assert_eq!(budgets.kinds.get("lambdalabs"), Some(&kind));
    assert_eq!(budgets.kinds.len(), 1);

    let plain = load(&scratch, &file(&server_with(&scratch, ""), &[&pool])).unwrap();
    assert_eq!(plain.elastic_pool().unwrap().budgets, Budgets::default());
}

/// Asserts the refusals, each naming the table and the key and not the value: a zero and a
/// word are not spends; a month total below the day total in `[server]` and in a kind's
/// table; a kind this server does not build; an unknown key in a kind's table; and a kind's
/// entry that is not a table.
#[test]
fn a_bad_total_is_refused_by_name() {
    let scratch = Scratch::new("budget_refusals");
    let pool = PoolText::elastic(&scratch);
    let plain = file(&server_with(&scratch, ""), &[&pool]);
    let spend = "must be a decimal with at most two places, such as \"12.00\"";
    for (extra, key, reason) in [
        (
            "max_spend_per_day_total = \"0\"",
            "max_spend_per_day_total",
            "must be above zero",
        ),
        (
            "max_spend_per_month_total = \"lots\"",
            "max_spend_per_month_total",
            spend,
        ),
        (
            "max_spend_since_go_total = 5",
            "max_spend_since_go_total",
            "must be a string",
        ),
    ] {
        let text = file(&server_with(&scratch, extra), &[&pool]);
        assert_eq!(refusal(&scratch, &text), format!("server.{key}: {reason}"));
    }
    let below = "must not be below max_spend_per_day_total";
    let head = server_with(
        &scratch,
        "max_spend_per_day_total = \"10.00\"\nmax_spend_per_month_total = \"9.99\"",
    );
    assert_eq!(
        refusal(&scratch, &file(&head, &[&pool])),
        format!("server.max_spend_per_month_total: {below}")
    );
    let kind = with_budget(
        &plain,
        "max_spend_per_day_total = \"10.00\"\nmax_spend_per_month_total = \"9.99\"",
    );
    assert_eq!(
        refusal(&scratch, &kind),
        format!("budget.lambdalabs.max_spend_per_month_total: {below}")
    );
    let unknown = format!("{plain}\n[budget.lambda]\nmax_spend_per_day_total = \"1.00\"\n");
    let said = refusal(&scratch, &unknown);
    assert!(
        said.starts_with("budget.lambda: not a provider kind; known: "),
        "{said}"
    );
    let key = with_budget(&plain, "max_spend_per_week_total = \"1.00\"");
    let said = refusal(&scratch, &key);
    assert!(
        said.starts_with("budget.lambdalabs.max_spend_per_week_total: unknown field; known: "),
        "{said}"
    );
    let table = format!("{plain}\n[budget]\nlambdalabs = 5\n");
    assert_eq!(
        refusal(&scratch, &table),
        "budget.lambdalabs: must be a table"
    );
}

/// Asserts that `live_until` reaches the plan as the instant the file wrote, in any offset,
/// and that a value that is not a full instant is refused naming the key. It catches an
/// instant read in the wrong zone, which would end a go hours early or late.
#[test]
fn live_until_reaches_the_plan_as_an_instant() {
    let scratch = Scratch::new("budget_live_until");
    let head = server_with(&scratch, "");
    for written in ["2027-01-01T00:00:00Z", "2027-01-01T05:30:00+05:30"] {
        let pool = PoolText::elastic(&scratch).set("live_until", written);
        let config = load(&scratch, &file(&head, &[&pool])).unwrap();
        let plan = config.elastic_pool().unwrap();
        assert_eq!(plan.live_until, Some(Millis::new(NEW_YEAR)), "{written}");
        assert_eq!(plan.launch, Launch::Plan);
    }
    let pool = PoolText::elastic(&scratch).set("live_until", "2027-01-01T00:00:00");
    let said = refusal(&scratch, &file(&head, &[&pool]));
    assert!(
        said.starts_with("pool \"gpu\": live_until: must be a date and time with an offset"),
        "{said}"
    );
    let none = load(&scratch, &file(&head, &[&PoolText::elastic(&scratch)])).unwrap();
    assert_eq!(none.elastic_pool().unwrap().live_until, None);
}

/// Asserts that a live pool whose `live_until` has passed when the file is read starts in
/// plan and says why in the warnings, that one a millisecond before it starts live with no
/// warning, and that the instant itself is past: the same comparison the scaler's step makes.
/// It catches a live pool that starts launching on a go that already ended.
#[test]
fn a_go_that_has_expired_at_load_starts_in_plan() {
    let scratch = Scratch::new("budget_expired");
    let dial = "advertise_url = \"https://sched.example.org:7301\"";
    let head = server_with(&scratch, dial);
    let pool = PoolText::elastic(&scratch)
        .set("launch", &quote("live"))
        .set("agent", AGENT)
        .set("live_until", "2027-01-01T00:00:00Z");
    let text = file(&head, &[&pool]);

    let before = load_at(&scratch, &text, NEW_YEAR - 1).unwrap();
    assert_eq!(before.elastic_pool().unwrap().launch, Launch::Live);
    assert!(
        before
            .warnings()
            .iter()
            .all(|each| !each.contains("live_until"))
    );

    for now in [NEW_YEAR, NEW_YEAR + 1] {
        let after = load_at(&scratch, &text, now).unwrap();
        assert_eq!(after.elastic_pool().unwrap().launch, Launch::Plan, "{now}");
        let said: Vec<_> = after
            .warnings()
            .iter()
            .filter(|each| each.contains("live_until"))
            .collect();
        assert_eq!(said.len(), 1, "{now}");
        assert!(
            said[0].starts_with("pool \"gpu\": live_until has passed"),
            "{}",
            said[0]
        );
    }
}
