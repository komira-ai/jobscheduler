//! Lease ids as the helper accepts them, the names of lease accounts, and the uid range.

use std::fmt;

use js_types::LeaseId;

/// The prefix of every lease user's name. Nothing outside it is ever created or
/// deleted.
pub const USER_PREFIX: &str = "kbf-lease-";

/// A lease id in its text form, `<term>.<seq>`: what the helper's protocol, its ledger
/// and its logs spell a lease as (see [`parse_lease`]). `js_types::LeaseId` displays as
/// `<term>-<seq>`, so every place the helper writes a lease id goes through this type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Dotted(pub LeaseId);

impl fmt::Display for Dotted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.0.term(), self.0.sequence())
    }
}

/// Parses a lease id in its text form, `<term>.<seq>` ([`Dotted`]'s `Display`).
///
/// Strict: two decimal `u64`s, no sign, no leading zero, nothing else. One lease id has
/// exactly one spelling, so the ledger of used ids cannot be dodged by writing an id
/// differently (`007.1` for `7.1`).
///
/// # Errors
/// The text is not exactly that form.
pub fn parse_lease(text: &str) -> Result<LeaseId, String> {
    let refuse = || format!("lease id {text:?} is not <term>.<seq>");
    let (term, seq) = text.split_once('.').ok_or_else(refuse)?;
    Ok(LeaseId::new(
        number(term).ok_or_else(refuse)?,
        number(seq).ok_or_else(refuse)?,
    ))
}

fn number(digits: &str) -> Option<u64> {
    let canonical = !digits.is_empty()
        && digits.bytes().all(|b| b.is_ascii_digit())
        && (digits == "0" || !digits.starts_with('0'));
    if canonical { digits.parse().ok() } else { None }
}

/// The account name of a lease's user: `kbf-lease-<term>-<seq>`, one account per lease,
/// made when the lease starts and deleted when it ends. A hyphen stands for the lease id's
/// dot, which some macOS tools treat specially in an account name.
#[must_use]
pub fn user_name(lease: LeaseId) -> String {
    format!("{USER_PREFIX}{}-{}", lease.term(), lease.sequence())
}

/// [`user_name`], refused when it is longer than [`MAX_NAME`] bytes (a term and a sequence
/// that are both large numbers make one), so no account is ever made under a name the
/// directory service would cut or refuse.
///
/// # Errors
/// The name is longer than [`MAX_NAME`] bytes.
pub fn account_name(lease: LeaseId) -> Result<String, String> {
    let name = user_name(lease);
    match name.len() <= MAX_NAME {
        true => Ok(name),
        false => Err(format!(
            "lease {} would have the account name {name}, {} bytes; an account name is at most {MAX_NAME}",
            Dotted(lease),
            name.len()
        )),
    }
}

/// The lease whose account name `name` is: the inverse of [`user_name`], one spelling per
/// lease (no sign, no leading zero). `None` for any other name, a pool account's among
/// them ([`parse_pool_user`]).
#[must_use]
pub fn parse_lease_user(name: &str) -> Option<LeaseId> {
    let (term, sequence) = name.strip_prefix(USER_PREFIX)?.split_once('-')?;
    Some(LeaseId::new(number(term)?, number(sequence)?))
}

/// The prefix of the accounts a release that kept a pool of lease users made: they were
/// named `kbf-lease-pool-<n>` and never deleted. The first boot reset of this release that
/// holds Full Disk Access deletes them ([`parse_pool_user`]).
pub const POOL_USER_PREFIX: &str = "kbf-lease-pool-";

/// The number `n` of an account `kbf-lease-pool-<n>` that a release with a pool made;
/// `None` for any other name. It can never be a name [`user_name`] makes, whose first
/// field is a number.
#[must_use]
pub fn parse_pool_user(name: &str) -> Option<u32> {
    let n = name.strip_prefix(POOL_USER_PREFIX)?;
    number(n).and_then(|n| u32::try_from(n).ok())
}

/// The standing VM user's account name (`--vm-user`): the one account besides the lease
/// users that the helper starts processes as, kills the processes of and rests the
/// Mac at.
///
/// Its name can never be a lease user's: a lease user is `kbf-lease-<term>-<seq>`, and
/// every account `user-delete` and the boot reset delete is named by a ledger entry or
/// starts with [`USER_PREFIX`], so a name that does not start with it is never among them.
/// The name is also refused when it is the daemon's role account (`_kbf`) or root, and it
/// is limited to lower-case letters, digits, `-` and `_` (starting with a letter), at most
/// 31 bytes: nothing the directory service or a shell would read as more than a name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VmUser(String);

/// The longest account name the helper accepts.
pub const MAX_NAME: usize = 31;
/// Names the VM user may never have besides the lease users' prefix: root, and the
/// daemon's role account.
const RESERVED: [&str; 2] = ["root", "_kbf"];

impl VmUser {
    /// The VM user named `name`.
    ///
    /// # Errors
    /// The name is empty, too long, holds a character outside `[a-z0-9_-]`, does not
    /// start with a letter, starts with the lease users' prefix, or is reserved.
    pub fn new(name: &str) -> Result<Self, String> {
        let refuse = |why: &str| Err(format!("VM user name {name:?} {why}"));
        if name.is_empty() || name.len() > MAX_NAME {
            return refuse(&format!("is not 1 to {MAX_NAME} bytes"));
        }
        if !name.starts_with(|c: char| c.is_ascii_lowercase())
            || !name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
        {
            return refuse("must start with a lower-case letter and hold only [a-z0-9_-]");
        }
        if name.starts_with(USER_PREFIX) {
            return refuse(&format!(
                "starts with {USER_PREFIX}, which only lease users have"
            ));
        }
        if RESERVED.contains(&name) {
            return refuse("is reserved");
        }
        Ok(Self(name.to_owned()))
    }

    /// The account name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.0
    }

    /// Whether `uid` may be this account's: an ordinary user's uid
    /// ([`MIN_UID`] to [`MAX_UID`]) that is not in the lease range, so a verb for the
    /// VM user never touches a lease user's, a system account's or root's processes.
    #[must_use]
    pub fn admits_uid(range: UidRange, uid: u32) -> bool {
        (MIN_UID..=MAX_UID).contains(&uid) && !range.contains(uid)
    }
}

impl std::str::FromStr for VmUser {
    type Err = String;

    fn from_str(name: &str) -> Result<Self, String> {
        Self::new(name)
    }
}

/// The uids lease users get, `first..=last`. The helper never touches a uid outside it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UidRange {
    first: u32,
    last: u32,
}

/// The lowest uid a range may start at: macOS keeps the uids below 500 for system
/// accounts, and gives people uids from 501.
pub const MIN_UID: u32 = 500;
/// The highest uid a range may end at, below the `nobody`-style uids near `u32::MAX`
/// and Linux's 65534.
pub const MAX_UID: u32 = 60_000;
/// The most uids a range may hold.
pub const MAX_LEN: u32 = 10_000;

impl UidRange {
    /// A range of lease uids.
    ///
    /// # Errors
    /// `first > last`, or the range leaves `MIN_UID..=MAX_UID` or holds more than
    /// [`MAX_LEN`] uids.
    pub fn new(first: u32, last: u32) -> Result<Self, String> {
        if first > last {
            return Err(format!("uid range {first}-{last} is empty"));
        }
        if first < MIN_UID || last > MAX_UID {
            return Err(format!(
                "uid range {first}-{last} leaves {MIN_UID}-{MAX_UID}"
            ));
        }
        if last - first >= MAX_LEN {
            return Err(format!(
                "uid range {first}-{last} holds more than {MAX_LEN} uids"
            ));
        }
        Ok(Self { first, last })
    }

    /// Whether `uid` is a lease uid.
    #[must_use]
    pub fn contains(self, uid: u32) -> bool {
        (self.first..=self.last).contains(&uid)
    }

    /// Every uid of the range, in order.
    pub fn uids(self) -> impl Iterator<Item = u32> {
        self.first..=self.last
    }

    /// Every uid of the range once, starting after `after` (the uid handed out last)
    /// and wrapping, so a uid is used again as late as possible. From the first uid when
    /// `after` is `None` or outside the range.
    pub fn after(self, after: Option<u32>) -> impl Iterator<Item = u32> {
        let len = self.last - self.first + 1;
        let start = match after {
            Some(uid) if self.contains(uid) => uid - self.first + 1,
            _ => 0,
        };
        (0..len).map(move |i| self.first + (start + i) % len)
    }
}

impl std::str::FromStr for UidRange {
    type Err = String;

    /// `<first>-<last>`, as in `600-699`.
    fn from_str(text: &str) -> Result<Self, String> {
        let refuse = || format!("uid range {text:?} is not <first>-<last>");
        let (first, last) = text.split_once('-').ok_or_else(refuse)?;
        let first = number(first).and_then(|n| u32::try_from(n).ok());
        let last = number(last).and_then(|n| u32::try_from(n).ok());
        Self::new(first.ok_or_else(refuse)?, last.ok_or_else(refuse)?)
    }
}

impl fmt::Display for UidRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.first, self.last)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches: a parser that accepts a second spelling of an id (leading zero, sign,
    /// spaces), which would let one lease id get two users past the ledger.
    #[test]
    fn a_lease_id_has_one_spelling() {
        assert_eq!(parse_lease("7.1"), Ok(LeaseId::new(7, 1)));
        assert_eq!(parse_lease("0.0"), Ok(LeaseId::new(0, 0)));
        assert_eq!(
            parse_lease("18446744073709551615.10"),
            Ok(LeaseId::new(u64::MAX, 10))
        );
        for bad in [
            "",
            "7",
            "7.",
            ".1",
            "07.1",
            "7.01",
            "+7.1",
            "7.-1",
            " 7.1",
            "7.1 ",
            "7.1.2",
            "7_1",
            "a.1",
            "18446744073709551616.1",
        ] {
            assert!(parse_lease(bad).is_err(), "{bad:?} was accepted");
        }
    }

    /// Catches: a VM user name that a lease user could have (the boot reset would then
    /// delete it), the daemon's or root's account, or a spelling the directory service
    /// or a command line would read as more than a name.
    #[test]
    fn a_vm_user_name_is_never_a_lease_user_s() {
        assert_eq!(VmUser::new("kbf-vm").unwrap().name(), "kbf-vm");
        assert_eq!("vm1_x".parse::<VmUser>().unwrap().name(), "vm1_x");
        assert!(VmUser::new(&"a".repeat(31)).is_ok());
        for bad in [
            "",
            "kbf-lease-1-1",
            "kbf-lease-",
            "root",
            "_kbf",
            "Kbf-vm",
            "1vm",
            "-vm",
            "kbf vm",
            "kbf.vm",
            "kbf/vm",
            "kbf-vm\n",
            "kbf-vm\0",
            "kbf-vm:",
            &"a".repeat(32),
        ] {
            assert!(VmUser::new(bad).is_err(), "{bad:?} was accepted");
        }
        // A lease user's own name, for any lease, is refused.
        assert!(VmUser::new(&user_name(LeaseId::new(3, 41))).is_err());
        // So is a pool account's.
        assert!(VmUser::new("kbf-lease-pool-7").is_err());
    }

    /// Catches: a VM uid that is root's, a system account's, or one a lease user can
    /// hold (a verb for the VM user would then kill a lease's processes).
    #[test]
    fn a_vm_uid_is_an_ordinary_uid_outside_the_lease_range() {
        let range = UidRange::new(600, 699).unwrap();
        assert!(VmUser::admits_uid(range, 501));
        assert!(VmUser::admits_uid(range, 599));
        assert!(VmUser::admits_uid(range, 700));
        assert!(VmUser::admits_uid(range, MAX_UID));
        for bad in [0, 1, 499, 600, 650, 699, MAX_UID + 1, u32::MAX] {
            assert!(!VmUser::admits_uid(range, bad), "{bad} was admitted");
        }
    }

    /// Catches: a name that drops a field or keeps the dot.
    #[test]
    fn the_user_name_carries_both_fields() {
        assert_eq!(user_name(LeaseId::new(3, 41)), "kbf-lease-3-41");
    }

    /// Catches: a name longer than 31 bytes accepted for an account (the 31st byte is
    /// the last the directory service keeps), or a name that fits refused.
    #[test]
    fn an_account_name_is_at_most_31_bytes() {
        // 10 bytes of prefix, 13 digits, a hyphen and 7 digits: 31.
        let longest = LeaseId::new(1_099_511_627_775, 1_234_567);
        let name = account_name(longest).unwrap();
        assert_eq!(name, "kbf-lease-1099511627775-1234567");
        assert_eq!(name.len(), 31);
        let one_more = LeaseId::new(1_099_511_627_775, 12_345_678);
        assert_eq!(
            account_name(one_more),
            Err("lease 1099511627775.12345678 would have the account name \
                 kbf-lease-1099511627775-12345678, 32 bytes; an account name is at most 31"
                .to_owned())
        );
        let widest = account_name(LeaseId::new(u64::MAX, u64::MAX)).unwrap_err();
        assert!(widest.contains("51 bytes"), "{widest}");
    }

    /// Catches: a parser of lease account names that reads a pool account, a name with
    /// a second spelling of a number, or an extra field as a lease's, or one that does
    /// not invert `user_name`.
    #[test]
    fn a_lease_account_name_is_read_back_to_its_lease() {
        for lease in [
            LeaseId::new(3, 41),
            LeaseId::new(0, 0),
            LeaseId::new(u64::MAX, u64::MAX),
        ] {
            assert_eq!(parse_lease_user(&user_name(lease)), Some(lease));
        }
        for other in [
            "",
            "kbf-lease-",
            "kbf-lease-3",
            "kbf-lease-3-",
            "kbf-lease--4",
            "kbf-lease-03-4",
            "kbf-lease-3-04",
            "kbf-lease-3-4-5",
            "kbf-lease-pool-4",
            "kbf-lease-+3-4",
            "kbf-lease-3-18446744073709551616",
            "kbf-vm",
            "other-3-4",
        ] {
            assert_eq!(parse_lease_user(other), None, "{other:?}");
        }
    }

    /// Catches: a pool account's number read from a name that is not one, or from a
    /// lease account's name; or a number with a second spelling.
    #[test]
    fn a_pool_account_name_is_read_back_to_its_number() {
        assert_eq!(parse_pool_user("kbf-lease-pool-0"), Some(0));
        assert_eq!(parse_pool_user("kbf-lease-pool-99"), Some(99));
        for other in [
            "",
            "kbf-lease-pool-",
            "kbf-lease-pool-x",
            "kbf-lease-pool-01",
            "kbf-lease-pool--1",
            "kbf-lease-pool-4294967296",
            "kbf-lease-3-4",
            "kbf-lease-pool",
            "pool-3",
        ] {
            assert_eq!(parse_pool_user(other), None, "{other:?}");
        }
    }

    /// Catches: a range check off by one at either end, or one that accepts system
    /// uids.
    #[test]
    fn a_range_holds_its_ends_and_refuses_system_uids() {
        let range: UidRange = "600-699".parse().unwrap();
        assert_eq!(range.to_string(), "600-699");
        assert!(range.contains(600));
        assert!(range.contains(699));
        assert!(!range.contains(599));
        assert!(!range.contains(700));
        assert!("600-600".parse::<UidRange>().is_ok());
        for bad in [
            "699-600",
            "499-600",
            "501-60001",
            "600",
            "600-",
            "-600",
            "a-b",
            "600-0699",
            "500-10500",
            "600-4294967296",
        ] {
            assert!(bad.parse::<UidRange>().is_err(), "{bad:?} was accepted");
        }
        assert!("500-10499".parse::<UidRange>().is_ok());
    }

    /// Catches: an order of uids that restarts at the first uid (a uid then comes back
    /// as soon as it is free, not as late as the range allows), skips or repeats a uid,
    /// or starts anywhere when the last uid given is not in the range.
    #[test]
    fn uids_come_round_after_the_last_one_handed_out() {
        let range = UidRange::new(600, 603).unwrap();
        let order = |after| range.after(after).collect::<Vec<_>>();
        assert_eq!(order(None), [600, 601, 602, 603]);
        assert_eq!(order(Some(600)), [601, 602, 603, 600]);
        assert_eq!(order(Some(602)), [603, 600, 601, 602]);
        assert_eq!(order(Some(603)), [600, 601, 602, 603]);
        // Outside the range: from the first uid.
        assert_eq!(order(Some(599)), [600, 601, 602, 603]);
        assert_eq!(order(Some(700)), [600, 601, 602, 603]);
        assert_eq!(range.uids().collect::<Vec<_>>(), [600, 601, 602, 603]);
        let one = UidRange::new(600, 600).unwrap();
        assert_eq!(one.after(Some(600)).collect::<Vec<_>>(), [600]);
    }
}
