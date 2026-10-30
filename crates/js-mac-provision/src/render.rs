//! The text of the anchor: a pure function of the uid range, the endpoints and the anchor
//! name. The same input gives the same bytes, endpoints sorted and repeated ones once.
//!
//! Every rule is `quick`, so the first rule that matches ends the evaluation and nothing
//! later in the main ruleset can reopen what a rule here closed. Every rule carries
//! `user`, so none touches a process outside the range.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use crate::policy::{AnchorName, Endpoint, UidRange};

/// The address range that holds the cloud metadata address, refused whole.
pub const LINK_LOCAL: &str = "169.254.0.0/16";

/// The anchor's rules for `uids`, which may connect over TCP to `endpoints` and over
/// IPv4 loopback, and to nothing else.
///
/// In order:
/// 1. link-local (169.254.0.0/16, the metadata address) refused, so no later edit of the
///    endpoint list can open it;
/// 2. IPv4 loopback to IPv4 loopback passed (pf cannot tell which user owns the other end
///    of a loopback connection, see the README);
/// 3. one pass per endpoint, sorted, with `keep state` so the answers come back;
/// 4. IPv6 refused;
/// 5. everything else over TCP and UDP refused.
///
/// A refused connection is `block return`: the pool user's `connect` fails at once with
/// "connection refused" instead of hanging until its timeout.
///
/// pf matches `user` on TCP and UDP only; ICMP and other protocols are not limited by this
/// anchor.
#[must_use]
pub fn render(uids: UidRange, endpoints: &[Endpoint], anchor: &AnchorName) -> String {
    let user = uids.pf_user();
    let endpoints: BTreeSet<Endpoint> = endpoints.iter().copied().collect();
    let mut text = String::new();
    // Writing to a String cannot fail.
    let _ = writeln!(
        text,
        "# js-mac-provision: egress of pool uids {uids} (anchor {anchor}). Generated; do not edit."
    );
    let _ = writeln!(
        text,
        "block return out quick inet proto {{ tcp, udp }} from any to {LINK_LOCAL} user {user}"
    );
    let _ = writeln!(
        text,
        "pass out quick on lo0 inet proto {{ tcp, udp }} from 127.0.0.0/8 to 127.0.0.0/8 user {user} keep state"
    );
    for e in &endpoints {
        let _ = writeln!(
            text,
            "pass out quick inet proto tcp from any to {} port {} user {user} keep state",
            e.addr(),
            e.port()
        );
    }
    let _ = writeln!(
        text,
        "block return out quick inet6 proto {{ tcp, udp }} from any to any user {user}"
    );
    let _ = writeln!(
        text,
        "block return out quick proto {{ tcp, udp }} from any to any user {user}"
    );
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anchor() -> AnchorName {
        "com.apple/com.komira.js.pool".parse().unwrap()
    }

    fn eps(list: &[&str]) -> Vec<Endpoint> {
        list.iter().map(|t| t.parse().unwrap()).collect()
    }

    #[test]
    fn two_endpoints_render_to_this_exact_anchor() {
        let text = render(
            UidRange::new(600, 100).unwrap(),
            &eps(&["192.0.2.10:443", "198.51.100.7:8443"]),
            &anchor(),
        );
        let golden = "\
# js-mac-provision: egress of pool uids 600-699 (anchor com.apple/com.komira.js.pool). Generated; do not edit.
block return out quick inet proto { tcp, udp } from any to 169.254.0.0/16 user 600:699
pass out quick on lo0 inet proto { tcp, udp } from 127.0.0.0/8 to 127.0.0.0/8 user 600:699 keep state
pass out quick inet proto tcp from any to 192.0.2.10 port 443 user 600:699 keep state
pass out quick inet proto tcp from any to 198.51.100.7 port 8443 user 600:699 keep state
block return out quick inet6 proto { tcp, udp } from any to any user 600:699
block return out quick proto { tcp, udp } from any to any user 600:699
";
        assert_eq!(text, golden);
    }

    #[test]
    fn no_endpoint_leaves_only_loopback() {
        let text = render(UidRange::new(600, 100).unwrap(), &[], &anchor());
        let golden = "\
# js-mac-provision: egress of pool uids 600-699 (anchor com.apple/com.komira.js.pool). Generated; do not edit.
block return out quick inet proto { tcp, udp } from any to 169.254.0.0/16 user 600:699
pass out quick on lo0 inet proto { tcp, udp } from 127.0.0.0/8 to 127.0.0.0/8 user 600:699 keep state
block return out quick inet6 proto { tcp, udp } from any to any user 600:699
block return out quick proto { tcp, udp } from any to any user 600:699
";
        assert_eq!(text, golden);
    }

    #[test]
    fn one_uid_renders_a_plain_user() {
        let text = render(
            UidRange::new(701, 1).unwrap(),
            &eps(&["192.0.2.10:443"]),
            &"a".parse().unwrap(),
        );
        assert!(text.contains("# js-mac-provision: egress of pool uids 701-701 (anchor a)."));
        assert_eq!(text.matches(" user 701").count(), 5);
        assert!(!text.contains("701:701"));
    }

    #[test]
    fn the_order_and_repeats_of_the_endpoints_do_not_change_the_text() {
        let uids = UidRange::new(600, 10).unwrap();
        let a = render(
            uids,
            &eps(&["198.51.100.7:443", "192.0.2.10:8443", "192.0.2.10:443"]),
            &anchor(),
        );
        let b = render(
            uids,
            &eps(&[
                "192.0.2.10:443",
                "192.0.2.10:443",
                "192.0.2.10:8443",
                "198.51.100.7:443",
            ]),
            &anchor(),
        );
        assert_eq!(a, b);
        let ports: Vec<&str> = a.lines().filter(|l| l.contains(" port ")).collect();
        assert_eq!(ports.len(), 3);
        assert!(ports[0].contains("192.0.2.10 port 443"));
        assert!(ports[1].contains("192.0.2.10 port 8443"));
        assert!(ports[2].contains("198.51.100.7 port 443"));
    }

    #[test]
    fn the_metadata_range_is_refused_before_any_pass_and_the_default_comes_last() {
        let text = render(
            UidRange::new(600, 100).unwrap(),
            &eps(&["192.0.2.10:443"]),
            &anchor(),
        );
        let rules: Vec<&str> = text.lines().filter(|l| !l.starts_with('#')).collect();
        let first_pass = rules.iter().position(|l| l.starts_with("pass")).unwrap();
        let link_local = rules.iter().position(|l| l.contains(LINK_LOCAL)).unwrap();
        assert!(link_local < first_pass);
        assert!(rules[link_local].starts_with("block return out quick"));
        let last = rules.last().unwrap();
        assert!(last.starts_with("block return out quick proto"));
        assert!(last.contains("from any to any"));
    }

    #[test]
    fn every_rule_is_quick_and_bound_to_the_uid_range() {
        let text = render(
            UidRange::new(600, 100).unwrap(),
            &eps(&["192.0.2.10:443"]),
            &anchor(),
        );
        for rule in text.lines().filter(|l| !l.starts_with('#')) {
            assert!(rule.contains(" out quick "), "{rule}");
            assert!(rule.contains(" user 600:699"), "{rule}");
        }
    }

    #[test]
    fn the_text_ends_with_one_newline() {
        let text = render(UidRange::new(600, 2).unwrap(), &[], &anchor());
        assert!(text.ends_with("699\n") || text.ends_with("601\n"));
        assert!(!text.ends_with("\n\n"));
    }
}
