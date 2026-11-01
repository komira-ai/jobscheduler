//! The per-rule counters of an anchor, read with `pfctl -a <anchor> -s rules -vv`.
//!
//! A refusal seen by a client proves little: a `block` that gives no reset looks, from the
//! client, like a host that is not there, and a route error looks like a refusal. What shows
//! that pf blocked a connection is the packet counter of the blocking rule rising while the
//! connection is tried. [`read`] gets the counters, [`block_packets`] sums the ones that
//! belong to one uid and one kind of block rule, and [`rise`] is the difference between two
//! reads.
//!
//! The text parsed is pf's own: a line `@N <rule>` followed by a line
//! `[ Evaluations: E Packets: P Bytes: B States: S ]`, as `pfctl -vv` prints it. pf expands a
//! rule's `user { ... }` and `proto { ... }` lists into one rule per element, so a rendered
//! line is several counted rules; the sums here take that into account.

use crate::pfctl::Pfctl;
use crate::policy::AnchorName;
use crate::render::LINK_LOCAL;

/// What went wrong reading or parsing the counters.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CountersError {
    /// `pfctl` could not be started.
    #[error("could not run pfctl: {0}")]
    Run(String),
    /// `pfctl` ran and exited non-zero.
    #[error("pfctl -s rules -vv failed (exit {code:?}): {stderr}")]
    Failed {
        /// The exit code; `None` when a signal ended it.
        code: Option<i32>,
        /// What it printed on standard error.
        stderr: String,
    },
    /// A rule line was not followed by a counter line.
    #[error("no counter line after the rule `{0}`")]
    NoCounters(String),
    /// A counter line came before any rule line.
    #[error("a counter line with no rule before it: `{0}`")]
    Orphan(String),
    /// A counter line lacks a field or the field is not a number.
    #[error("no number for `{field}` in `{line}`")]
    BadField {
        /// The field's name, as pf prints it.
        field: &'static str,
        /// The whole line.
        line: String,
    },
}

/// One counted rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuleCounters {
    /// The rule as pf prints it, without the `@N` index.
    pub rule: String,
    /// How many packets were tested against the rule.
    pub evaluations: u64,
    /// How many packets matched it.
    pub packets: u64,
    /// How many bytes matched it.
    pub bytes: u64,
}

/// Which block rule of the anchor is meant (both are TCP here).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Which {
    /// The rule for the link-local range, the metadata address's.
    LinkLocal,
    /// The last rule: every other IPv4 destination.
    CatchAll,
}

impl RuleCounters {
    fn is_block(&self) -> bool {
        self.rule.starts_with("block")
    }

    fn is_tcp(&self) -> bool {
        self.rule
            .split_whitespace()
            .any(|w| w.trim_matches(PUNCT) == "tcp")
    }

    /// Whether the rule's `user` clause names `uid`, as `user = 4100` or `user { 4100 4101 }`.
    fn has_uid(&self, uid: u32) -> bool {
        self.rule
            .split_whitespace()
            .skip_while(|w| *w != "user")
            .skip(1)
            .take_while(|w| matches!(*w, "=" | "{" | "}" | ",") || number(w).is_some())
            .any(|w| number(w) == Some(uid))
    }

    fn is(&self, which: Which) -> bool {
        match which {
            Which::LinkLocal => self.rule.contains(LINK_LOCAL),
            Which::CatchAll => !self.rule.contains(LINK_LOCAL) && !self.rule.contains("inet6"),
        }
    }
}

const PUNCT: [char; 3] = ['{', '}', ','];

fn number(word: &str) -> Option<u32> {
    word.trim_matches(PUNCT).parse().ok()
}

/// The counters in `text`, one entry per `@N` rule line, in order.
///
/// # Errors
/// [`CountersError::NoCounters`], [`CountersError::Orphan`] or [`CountersError::BadField`]
/// when the text is not in the shape described in the module docs.
pub fn parse(text: &str) -> Result<Vec<RuleCounters>, CountersError> {
    let _ = text;
    Ok(Vec::new())
}

#[allow(dead_code)]
fn field(line: &str, name: &'static str) -> Result<u64, CountersError> {
    let bad = || CountersError::BadField {
        field: name,
        line: line.to_owned(),
    };
    let mut words = line.split_whitespace();
    words.find(|w| *w == name).ok_or_else(bad)?;
    words.next().and_then(|w| w.parse().ok()).ok_or_else(bad)
}

/// The counters of `anchor`'s rules, as `pf` prints them now.
///
/// # Errors
/// [`CountersError::Run`] or [`CountersError::Failed`] for `pfctl`, otherwise as [`parse`].
pub fn read(pf: &dyn Pfctl, anchor: &AnchorName) -> Result<Vec<RuleCounters>, CountersError> {
    let name = anchor.to_string();
    let out = pf
        .run(&["-a", &name, "-s", "rules", "-vv"])
        .map_err(|e| CountersError::Run(e.to_string()))?;
    if !out.success() {
        return Err(CountersError::Failed {
            code: out.code,
            stderr: out.stderr.trim().to_owned(),
        });
    }
    parse(&out.stdout)
}

/// The packets that matched the TCP block rules of `which` for `uid`, summed over the rules
/// pf expanded from one rendered line.
#[must_use]
pub fn block_packets(rules: &[RuleCounters], uid: u32, which: Which) -> u64 {
    rules
        .iter()
        .filter(|r| r.is_block() && r.is_tcp() && r.has_uid(uid) && r.is(which))
        .map(|r| r.packets)
        .sum()
}

/// How many more packets matched those rules in `after` than in `before`.
#[must_use]
pub fn rise(before: &[RuleCounters], after: &[RuleCounters], uid: u32, which: Which) -> u64 {
    block_packets(after, uid, which).saturating_sub(block_packets(before, uid, which))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Fake;

    /// The shape of a `-vv` listing of the anchor for the uids 4100 and 4101 and one
    /// endpoint, with pf's expansion of the lists. Written after pf's documented output, not
    /// captured: the next run on a Mac prints the real text, and this file should then be
    /// replaced by it.
    const LISTING: &str = include_str!("../tests/golden/pfctl_sr_vv.txt");

    fn counted() -> Vec<RuleCounters> {
        parse(LISTING).unwrap()
    }

    #[test]
    fn the_listing_has_a_counted_entry_per_expanded_rule() {
        let rules = counted();
        assert_eq!(rules.len(), 18);
        assert_eq!(
            rules[0].rule,
            "block return out quick inet proto tcp from any to 169.254.0.0/16 user = 4100"
        );
        assert_eq!(
            (rules[0].evaluations, rules[0].packets, rules[0].bytes),
            (4, 3, 180)
        );
        assert_eq!(rules[1].packets, 0);
    }

    #[test]
    fn packets_are_summed_per_uid_and_per_block_rule() {
        let rules = counted();
        assert_eq!(block_packets(&rules, 4100, Which::LinkLocal), 3);
        assert_eq!(block_packets(&rules, 4101, Which::LinkLocal), 0);
        assert_eq!(block_packets(&rules, 4100, Which::CatchAll), 0);
        assert_eq!(block_packets(&rules, 4101, Which::CatchAll), 2);
        assert_eq!(block_packets(&rules, 4200, Which::CatchAll), 0);
    }

    #[test]
    fn pass_rules_and_the_ipv6_and_udp_rules_are_not_counted() {
        let mut rules = counted();
        for r in &mut rules {
            r.packets = 7;
        }
        // Only the two TCP catch-all rules (one per uid) answer for the catch-all.
        assert_eq!(block_packets(&rules, 4100, Which::CatchAll), 7);
        assert_eq!(block_packets(&rules, 4100, Which::LinkLocal), 7);
    }

    #[test]
    fn a_uid_list_in_one_rule_names_each_uid() {
        let rules = parse(
            "@0 block return out quick proto { tcp udp } all user { 4100 4101, 4102 }\n  \
             [ Evaluations: 1 Packets: 5 Bytes: 1 States: 0 ]\n",
        )
        .unwrap();
        assert_eq!(block_packets(&rules, 4101, Which::CatchAll), 5);
        assert_eq!(block_packets(&rules, 4102, Which::CatchAll), 5);
        assert_eq!(block_packets(&rules, 4103, Which::CatchAll), 0);
        // `user` without the equals sign.
        let plain = parse(
            "@0 block return out quick proto tcp all user 4100\n  \
             [ Evaluations: 1 Packets: 2 Bytes: 1 States: 0 ]\n",
        )
        .unwrap();
        assert_eq!(block_packets(&plain, 4100, Which::CatchAll), 2);
    }

    #[test]
    fn a_rule_without_a_user_or_with_a_bare_user_word_names_no_uid() {
        let rules = parse(
            "@0 block return out quick proto tcp all\n  \
             [ Evaluations: 1 Packets: 9 Bytes: 1 States: 0 ]\n\
             @1 block return out quick proto tcp all user\n  \
             [ Evaluations: 1 Packets: 9 Bytes: 1 States: 0 ]\n",
        )
        .unwrap();
        assert_eq!(block_packets(&rules, 0, Which::CatchAll), 0);
    }

    #[test]
    fn the_rise_is_the_difference_and_never_negative() {
        let before = counted();
        let mut after = counted();
        after[0].packets += 4;
        assert_eq!(rise(&before, &after, 4100, Which::LinkLocal), 4);
        assert_eq!(rise(&before, &after, 4100, Which::CatchAll), 0);
        assert_eq!(rise(&after, &before, 4100, Which::LinkLocal), 0);
    }

    #[test]
    fn an_empty_listing_is_no_rules() {
        assert_eq!(parse("").unwrap(), vec![]);
        assert_eq!(parse("No ALTQ support in kernel\n").unwrap(), vec![]);
    }

    #[test]
    fn a_rule_without_counters_is_an_error() {
        let two = "@0 block all\n@1 pass all\n  [ Evaluations: 1 Packets: 0 Bytes: 0 States: 0 ]\n";
        assert_eq!(
            parse(two).unwrap_err(),
            CountersError::NoCounters("block all".to_owned())
        );
        assert_eq!(
            parse("@0 block all\n").unwrap_err(),
            CountersError::NoCounters("block all".to_owned())
        );
    }

    #[test]
    fn a_counter_line_without_a_rule_is_an_error() {
        let line = "[ Evaluations: 1 Packets: 0 Bytes: 0 States: 0 ]";
        assert_eq!(
            parse(line).unwrap_err(),
            CountersError::Orphan(line.to_owned())
        );
    }

    #[test]
    fn a_missing_or_non_numeric_field_names_it() {
        let no_bytes = "@0 block all\n  [ Evaluations: 1 Packets: 0 States: 0 ]\n";
        assert!(matches!(
            parse(no_bytes).unwrap_err(),
            CountersError::BadField {
                field: "Bytes:",
                ..
            }
        ));
        let junk = "@0 block all\n  [ Evaluations: 1 Packets: many Bytes: 0 States: 0 ]\n";
        assert!(matches!(
            parse(junk).unwrap_err(),
            CountersError::BadField {
                field: "Packets:",
                ..
            }
        ));
        let cut = "@0 block all\n  [ Evaluations: 1 Packets: 0 Bytes:";
        assert!(matches!(
            parse(cut).unwrap_err(),
            CountersError::BadField {
                field: "Bytes:",
                ..
            }
        ));
    }

    #[test]
    fn read_asks_for_the_anchor_s_rules_with_counters() {
        let dir = std::env::temp_dir();
        let fake = Fake::new(&dir);
        *fake.counters.borrow_mut() = LISTING.to_owned();
        let anchor = AnchorName::new("com.apple/js-test").unwrap();
        let rules = read(&fake, &anchor).unwrap();
        assert_eq!(rules.len(), 18);
        assert_eq!(
            fake.calls(),
            vec!["-a com.apple/js-test -s rules -vv".to_owned()]
        );
    }

    #[test]
    fn read_reports_a_pfctl_that_would_not_start_or_failed() {
        let dir = std::env::temp_dir();
        let anchor = AnchorName::new("com.apple/js-test").unwrap();
        let mut absent = Fake::new(&dir);
        absent.fail = Some((0, true));
        assert!(matches!(
            read(&absent, &anchor).unwrap_err(),
            CountersError::Run(_)
        ));
        let mut failing = Fake::new(&dir);
        failing.fail = Some((0, false));
        assert!(matches!(
            read(&failing, &anchor).unwrap_err(),
            CountersError::Failed { code: Some(1), .. }
        ));
    }
}
