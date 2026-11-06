//! Lease ids as the helper accepts them, the pool of lease users (its slots, their names
//! and its size), and the uid range.

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

/// The account name a lease user had before the pool: `kbf-lease-<term>-<seq>`, one
/// user per lease. The helper makes no such user any more; it names one only to end
/// the user of a lease a release before the pool created (the boot reset after an
/// update). A hyphen stands for the lease id's dot, which some macOS tools treat
/// specially in an account name.
#[must_use]
pub fn user_name(lease: LeaseId) -> String {
    format!("{USER_PREFIX}{}-{}", lease.term(), lease.sequence())
}

/// One place in the pool of lease users: `0` to the pool's size less one. A slot keeps
/// its user's name ([`pool_user_name`]) and uid ([`UidRange::uid`]) for good; a user
/// that is replaced is replaced under the same slot, with the same name and uid.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Slot(u32);

impl Slot {
    /// The slot with index `index`.
    #[must_use]
    pub const fn new(index: u32) -> Self {
        Self(index)
    }

    /// The slot's index.
    #[must_use]
    pub const fn index(self) -> u32 {
        self.0
    }
}

impl fmt::Display for Slot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::str::FromStr for Slot {
    type Err = String;

    /// The index in decimal, one spelling only (no sign, no leading zero).
    fn from_str(text: &str) -> Result<Self, String> {
        number(text)
            .and_then(|n| u32::try_from(n).ok())
            .map(Self)
            .ok_or_else(|| format!("pool slot {text:?} is not a number"))
    }
}

/// The account name of the pool user in `slot`: `kbf-lease-pool-<slot>`. It starts with
/// [`USER_PREFIX`] (so no VM user can have it) and can never be a name [`user_name`]
/// makes, whose first field is a number.
#[must_use]
pub fn pool_user_name(slot: Slot) -> String {
    format!("{USER_PREFIX}pool-{slot}")
}

/// How many users the pool holds (`--pool-size`): 1 to [`MAX_LEN`], and no more than
/// the uid range holds ([`UidRange::fits`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PoolSize(u32);

/// The pool's size when `--pool-size` is not given.
pub const DEFAULT_POOL_SIZE: u32 = 100;

impl PoolSize {
    /// A pool of `size` users.
    ///
    /// # Errors
    /// `size` is 0 or more than [`MAX_LEN`].
    pub fn new(size: u32) -> Result<Self, String> {
        match (1..=MAX_LEN).contains(&size) {
            true => Ok(Self(size)),
            false => Err(format!("pool size {size} is not 1 to {MAX_LEN}")),
        }
    }

    /// How many users the pool holds.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }

    /// Every slot of the pool, in order.
    pub fn slots(self) -> impl Iterator<Item = Slot> {
        (0..self.0).map(Slot)
    }
}

impl Default for PoolSize {
    fn default() -> Self {
        Self(DEFAULT_POOL_SIZE)
    }
}

impl std::str::FromStr for PoolSize {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, String> {
        let size = number(text)
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| format!("pool size {text:?} is not a number"))?;
        Self::new(size)
    }
}

impl fmt::Display for PoolSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The standing VM user's account name (`--vm-user`): the one account besides the
/// lease users that the helper starts processes as, kills the processes of and rests the
/// Mac at.
///
/// Its name can never be a lease user's: a lease user is `kbf-lease-pool-<slot>` (or,
/// from before the pool, `kbf-lease-<term>-<seq>`), and every user the boot reset,
/// `user-delete` and a pool user's replacement delete is named by a ledger entry, so a
/// name that does not start with [`USER_PREFIX`] is never among them. The name is also
/// refused when it is the daemon's role account (`_kbf`) or root, and it is limited to
/// lower-case letters, digits, `-` and `_` (starting with a letter), at most 31 bytes:
/// nothing the directory service or a shell would read as more than a name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VmUser(String);

/// The longest account name the helper accepts.
const MAX_NAME: usize = 31;
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
/// The most uids a range may hold, and so the largest pool.
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

    /// The uid of the pool user in `slot`: the range's first uid plus the slot's index;
    /// `None` when the range ends before it.
    #[must_use]
    pub fn uid(self, slot: Slot) -> Option<u32> {
        let uid = self.first.checked_add(slot.0)?;
        self.contains(uid).then_some(uid)
    }

    /// Whether a pool of `size` users fits the range: one uid for each slot.
    ///
    /// # Errors
    /// The pool is larger than the range.
    pub fn fits(self, size: PoolSize) -> Result<(), String> {
        let len = self.last - self.first + 1;
        match size.0 <= len {
            true => Ok(()),
            false => Err(format!(
                "a pool of {size} users does not fit the uid range {self}, which holds {len}"
            )),
        }
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

    /// Catches: a slot whose uid moves (off by one, or past the range's end), or a pool
    /// accepted that holds more users than the range has uids.
    #[test]
    fn each_slot_has_one_uid_of_the_range() {
        let range = UidRange::new(600, 603).unwrap();
        assert_eq!(range.uids().collect::<Vec<_>>(), [600, 601, 602, 603]);
        assert_eq!(range.uid(Slot::new(0)), Some(600));
        assert_eq!(range.uid(Slot::new(3)), Some(603));
        assert_eq!(range.uid(Slot::new(4)), None);
        assert_eq!(range.uid(Slot::new(u32::MAX)), None);
        assert_eq!(range.fits(PoolSize::new(4).unwrap()), Ok(()));
        assert_eq!(
            range.fits(PoolSize::new(5).unwrap()),
            Err("a pool of 5 users does not fit the uid range 600-603, which holds 4".to_owned())
        );
    }

    /// Catches: a pool user's name that a per-lease user or a VM user could have, or a
    /// slot or a pool size with a second spelling or out of bounds.
    #[test]
    fn pool_names_slots_and_sizes_have_one_spelling() {
        assert_eq!(pool_user_name(Slot::new(7)), "kbf-lease-pool-7");
        assert!(VmUser::new(&pool_user_name(Slot::new(7))).is_err());
        assert_eq!("12".parse::<Slot>(), Ok(Slot::new(12)));
        assert_eq!(Slot::new(12).to_string(), "12");
        assert_eq!(Slot::new(12).index(), 12);
        for bad in ["", "012", "+1", "a", "4294967296"] {
            assert!(bad.parse::<Slot>().is_err(), "{bad:?} was accepted");
        }
        assert_eq!(PoolSize::default().get(), DEFAULT_POOL_SIZE);
        assert_eq!("100".parse::<PoolSize>(), Ok(PoolSize::default()));
        assert_eq!(PoolSize::new(3).unwrap().to_string(), "3");
        let slots: Vec<Slot> = PoolSize::new(3).unwrap().slots().collect();
        assert_eq!(slots, [Slot::new(0), Slot::new(1), Slot::new(2)]);
        assert!("10000".parse::<PoolSize>().is_ok());
        assert_eq!(
            "0".parse::<PoolSize>(),
            Err("pool size 0 is not 1 to 10000".to_owned())
        );
        assert!("10001".parse::<PoolSize>().is_err());
        assert_eq!(
            "x".parse::<PoolSize>(),
            Err("pool size \"x\" is not a number".to_owned())
        );
        assert!("4294967296".parse::<PoolSize>().is_err());
    }
}
