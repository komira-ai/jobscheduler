//! Tests of the one-time join tokens.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Barrier};
use std::thread;

use js_lease::{ManualClock, Timestamp};
use js_registry::SequentialSessions;

use super::*;

const T0: u64 = 5_000_000;

fn instance(id: &str) -> InstanceIdentity {
    InstanceIdentity {
        provider: "lambdalabs".to_string(),
        instance_id: id.to_string(),
    }
}

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/join_tokens_unit")
        .join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir.join("leases.toml.joins")
}

fn tokens(path: &Path) -> (JoinTokens, ManualClock) {
    let clock = ManualClock::new(Timestamp::from_millis(T0));
    let shared = SharedClock::new(clock.clone());
    let random = Box::new(SequentialSessions::new());
    (
        JoinTokens::open(path.to_path_buf(), shared, random).unwrap(),
        clock,
    )
}

fn credential(minted: &Minted) -> Credential {
    Credential::of_token(&minted.token)
}

struct NoRandom;

impl SessionSource for NoRandom {
    fn fresh(&self) -> Option<[u8; 32]> {
        None
    }
}

/// A token admits the host that names its instance, once: the same host again is
/// accepted (an agent that says hello after a lost connection), another host id is
/// refused as used, and heartbeats are served for the first host only. It catches a
/// token that registers a second host, and one that locks its own host out.
#[test]
fn a_token_registers_one_host() {
    let path = scratch("one_host");
    let (joins, _) = tokens(&path);
    let minted = joins.mint(instance("i-1"), DEFAULT_TTL).unwrap();
    let who = joins.identify(&minted.token).unwrap();
    assert_eq!(who, credential(&minted));
    assert_eq!(joins.serves(&who, "host-a"), Err(JoinRefusal::NotAdmitted));
    assert_eq!(
        joins.admit(&who, "host-a", Some(&instance("i-1"))),
        Ok(Admission::Fresh)
    );
    assert_eq!(
        joins.admit(&who, "host-a", Some(&instance("i-1"))),
        Ok(Admission::Again)
    );
    assert_eq!(
        joins.admit(&who, "host-b", Some(&instance("i-1"))),
        Err(JoinRefusal::Used)
    );
    assert_eq!(joins.serves(&who, "host-a"), Ok(()));
    assert_eq!(joins.serves(&who, "host-b"), Err(JoinRefusal::NotAdmitted));
    assert!(joins.identify("not-a-token").is_none());
}

/// A token is good while the clock is before its expiry and refused from the expiry
/// on, and a token that was used keeps serving its host after the expiry. It catches
/// an off-by-one at the boundary and an expiry that cuts off a registered host.
#[test]
fn a_token_is_good_until_its_expiry() {
    let path = scratch("expiry");
    let (joins, clock) = tokens(&path);
    let ttl = Duration::from_secs(60);
    let first = joins.mint(instance("i-1"), ttl).unwrap();
    let second = joins.mint(instance("i-2"), ttl).unwrap();
    assert_eq!(first.expires_at_ms, T0 + 60_000);
    let (a, b) = (credential(&first), credential(&second));
    clock.set(Timestamp::from_millis(T0 + 59_999));
    assert_eq!(
        joins.admit(&a, "host-a", Some(&instance("i-1"))),
        Ok(Admission::Fresh)
    );
    clock.set(Timestamp::from_millis(T0 + 60_000));
    assert_eq!(
        joins.admit(&b, "host-b", Some(&instance("i-2"))),
        Err(JoinRefusal::Expired)
    );
    assert_eq!(
        joins.admit(&a, "host-a", Some(&instance("i-1"))),
        Ok(Admission::Again)
    );
    assert_eq!(joins.serves(&a, "host-a"), Ok(()));
}

/// A hello that names another instance, or none, is refused and leaves the token
/// unused, so the right instance can still use it. It catches a token that is burnt
/// by a wrong claim (a denial of service on the launch) and one that admits any
/// instance.
#[test]
fn a_wrong_instance_is_refused_and_does_not_consume() {
    let path = scratch("wrong_instance");
    let (joins, _) = tokens(&path);
    let minted = joins.mint(instance("i-1"), DEFAULT_TTL).unwrap();
    let who = credential(&minted);
    let other_provider = InstanceIdentity {
        provider: "other".to_string(),
        instance_id: "i-1".to_string(),
    };
    for claim in [Some(instance("i-2")), Some(other_provider), None] {
        assert_eq!(
            joins.admit(&who, "host-a", claim.as_ref()),
            Err(JoinRefusal::WrongInstance)
        );
    }
    assert_eq!(
        joins.admit(&who, "host-a", Some(&instance("i-1"))),
        Ok(Admission::Fresh)
    );
}

/// Of several hosts that present one unused token at the same moment, exactly one is
/// admitted and the rest are refused as used. It catches a check and a use that are
/// not made under one lock, which would let two hosts register with one token.
#[test]
fn of_racing_hosts_one_wins() {
    let path = scratch("race");
    let (joins, _) = tokens(&path);
    let joins = Arc::new(joins);
    let who = credential(&joins.mint(instance("i-1"), DEFAULT_TTL).unwrap());
    let start = Arc::new(Barrier::new(8));
    let racers: Vec<_> = (0..8)
        .map(|n| {
            let (joins, who, start) = (joins.clone(), who.clone(), start.clone());
            thread::spawn(move || {
                start.wait();
                joins.admit(&who, &format!("host-{n}"), Some(&instance("i-1")))
            })
        })
        .collect();
    let results: Vec<_> = racers.into_iter().map(|r| r.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    let others = results.iter().filter(|r| r.is_err());
    assert!(others.clone().all(|r| *r == Err(JoinRefusal::Used)));
    assert_eq!(others.count(), 7);
}

/// A token that is not in the set at the time of use is refused as expired (it was
/// dropped after its time).
#[test]
fn an_unknown_credential_is_refused_as_expired() {
    let path = scratch("unknown");
    let (joins, _) = tokens(&path);
    let who = Credential::of_token("never-minted");
    assert_eq!(
        joins.admit(&who, "host-a", Some(&instance("i-1"))),
        Err(JoinRefusal::Expired)
    );
}

/// What is minted and used survives a restart: the unused token still admits, the
/// used one still refuses another host and still serves its own, and the file holds
/// the digests and no token. It catches a restart that gives back a used token, one
/// that drops an unused token, and a file that carries the secret.
#[test]
fn tokens_survive_a_restart_and_the_file_holds_no_token() {
    let path = scratch("restart");
    let (joins, _) = tokens(&path);
    let used = joins.mint(instance("i-1"), DEFAULT_TTL).unwrap();
    let unused = joins.mint(instance("i-2"), DEFAULT_TTL).unwrap();
    let (used_who, unused_who) = (credential(&used), credential(&unused));
    joins
        .admit(&used_who, "host-a", Some(&instance("i-1")))
        .unwrap();
    drop(joins);

    let text = fs::read_to_string(&path).unwrap();
    assert!(!text.contains(&used.token), "{text}");
    assert!(!text.contains(&unused.token), "{text}");
    assert!(text.contains(&used_who.to_hex()), "{text}");
    assert!(text.contains("used_by = \"host-a\""), "{text}");
    let mode = fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o077, 0, "the file is owner-only");

    let (again, _) = tokens(&path);
    assert!(again.identify(&used.token).is_some());
    assert_eq!(
        again.admit(&used_who, "host-b", Some(&instance("i-1"))),
        Err(JoinRefusal::Used)
    );
    assert_eq!(again.serves(&used_who, "host-a"), Ok(()));
    assert_eq!(
        again.admit(&unused_who, "host-c", Some(&instance("i-2"))),
        Ok(Admission::Fresh)
    );
}

/// A second token for an instance replaces its unused one, a used one stays, a token
/// past its time is dropped by the next mint, and the number that wait is bounded. It
/// catches two live ways into one instance, a mint that revokes a registered host's
/// credential, a file that grows with every launch that never booted, and an
/// unbounded pile.
#[test]
fn minting_replaces_drops_and_is_bounded() {
    let path = scratch("minting");
    let (joins, clock) = tokens(&path);
    let first = joins.mint(instance("i-1"), DEFAULT_TTL).unwrap();
    let second = joins.mint(instance("i-1"), DEFAULT_TTL).unwrap();
    assert!(joins.identify(&first.token).is_none());
    assert!(joins.identify(&second.token).is_some());

    let who = credential(&second);
    joins.admit(&who, "host-a", Some(&instance("i-1"))).unwrap();
    let third = joins.mint(instance("i-1"), DEFAULT_TTL).unwrap();
    assert!(
        joins.identify(&second.token).is_some(),
        "a used token stays"
    );
    assert!(joins.identify(&third.token).is_some());

    clock.set(Timestamp::from_millis(T0 + 31 * 60 * 1000));
    let fourth = joins.mint(instance("i-9"), DEFAULT_TTL).unwrap();
    assert!(
        joins.identify(&third.token).is_none(),
        "an expired token goes"
    );
    assert!(joins.identify(&fourth.token).is_some());

    for n in 0..MAX_WAITING - 1 {
        joins
            .mint(instance(&format!("w-{n}")), DEFAULT_TTL)
            .unwrap();
    }
    assert_eq!(
        joins.mint(instance("one-more"), DEFAULT_TTL).err(),
        Some(MintError::Full)
    );
    assert!(
        joins.identify(&fourth.token).is_some(),
        "a refusal drops nothing"
    );
    let again = joins.mint(instance("w-0"), DEFAULT_TTL);
    assert!(again.is_ok(), "a replacement needs no room");
}

/// With no random bytes nothing is minted and nothing is dropped.
#[test]
fn no_random_bytes_make_no_token() {
    let path = scratch("no_random");
    let clock = SharedClock::new(ManualClock::new(Timestamp::from_millis(T0)));
    let joins = JoinTokens::open(path, clock, Box::new(NoRandom)).unwrap();
    assert_eq!(
        joins.mint(instance("i-1"), DEFAULT_TTL).err(),
        Some(MintError::NoRandom)
    );
}

/// A file that cannot be written ends a mint and a use with the file and the memory
/// as they were: the token is not made, and an unused token is not consumed. It
/// catches a use that is allowed although a restart would give the token back.
#[test]
fn a_failed_write_changes_nothing() {
    let path = scratch("unwritable");
    let (joins, _) = tokens(&path);
    let minted = joins.mint(instance("i-1"), DEFAULT_TTL).unwrap();
    let who = credential(&minted);
    // Make the file a directory, so that the rename of the new file over it fails.
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    assert_eq!(
        joins.admit(&who, "host-a", Some(&instance("i-1"))),
        Err(JoinRefusal::Unsaved)
    );
    assert_eq!(joins.serves(&who, "host-a"), Err(JoinRefusal::NotAdmitted));
    assert_eq!(
        joins.mint(instance("i-2"), DEFAULT_TTL).err(),
        Some(MintError::Unsaved)
    );
    assert!(joins.identify(&minted.token).is_some());
    assert_eq!(joins.lock().len(), 1);
}

/// The file reads back as written, and a file that is not valid is a refusal that
/// names the file and the fault. It catches a start with no tokens after a damaged
/// file, which would give back every used token.
#[test]
fn a_damaged_file_is_refused() {
    let hash = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let row = |hash: &str| {
        format!(
            "[[join]]\nhash = \"{hash}\"\nprovider = \"p\"\ninstance_id = \"i\"\nexpires_at_ms = 1\n"
        )
    };
    let good = parse(&format!("version = 1\n{}", row(hash))).unwrap();
    assert_eq!(good.len(), 1);
    assert_eq!(parse(&render(&good).unwrap()).unwrap(), good);
    assert_eq!(parse("version = 1\n").unwrap(), Entries::new());
    let twice = parse(&format!("version = 1\n{}{}", row(hash), row(hash)));
    assert_eq!(twice.unwrap_err(), "a token hash is listed twice");
    let short = parse(&format!("version = 1\n{}", row("abc")));
    assert!(short.unwrap_err().contains("64 lower-case hex"));
    assert!(parse("version = [").is_err());

    let path = scratch("damaged");
    fs::write(&path, [0xff, 0xfe]).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let text = load(&path).err().unwrap().to_string();
    assert!(text.contains("is not a valid join token file"), "{text}");
    assert!(text.contains("it is not text"), "{text}");
    fs::write(&path, "version = [").unwrap();
    assert!(load(&path).is_err());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(load(&path).is_err(), "a file others can read is refused");
    let under_a_file = path.join("below");
    assert!(
        load(&under_a_file).is_err(),
        "a path that cannot be looked at"
    );
    let missing = path.with_file_name("missing.joins");
    assert_eq!(load(&missing).unwrap(), Entries::new());
}

const CREDENTIAL_MS: u64 = 24 * 60 * 60 * 1000;
const STEP_MS: u64 = 60 * 60 * 1000;
const SPREAD_MS: u64 = 30 * 60 * 1000;

/// The `credential_expires_at_ms` the file at `path` holds for its one used token.
fn held_expiry(path: &Path) -> u64 {
    let text = fs::read_to_string(path).unwrap();
    let line = text
        .lines()
        .find(|line| line.starts_with("credential_expires_at_ms"))
        .unwrap();
    line.split("= ").nth(1).unwrap().parse().unwrap()
}

/// A used token that nobody serves ends at the bound: one millisecond before, hello and
/// heartbeat still pass; at it, both are refused as expired; and the next mint drops the
/// entry from the file. It catches a used token that is a credential for ever and one
/// that ends early.
#[test]
fn a_used_token_ends_at_the_bound_and_the_next_mint_drops_it() {
    let path = scratch("bound");
    let (joins, clock) = tokens(&path);
    let minted = joins.mint(instance("i-1"), DEFAULT_TTL).unwrap();
    let who = credential(&minted);
    joins.admit(&who, "host-a", Some(&instance("i-1"))).unwrap();
    assert_eq!(held_expiry(&path), T0 + CREDENTIAL_MS);

    clock.set(Timestamp::from_millis(T0 + CREDENTIAL_MS));
    assert_eq!(
        joins.admit(&who, "host-a", Some(&instance("i-1"))),
        Err(JoinRefusal::Expired)
    );
    assert_eq!(joins.serves(&who, "host-a"), Err(JoinRefusal::Expired));
    assert_eq!(
        joins.admit(&who, "host-b", Some(&instance("i-1"))),
        Err(JoinRefusal::Expired)
    );
    assert!(fs::read_to_string(&path).unwrap().contains(&who.to_hex()));

    joins.mint(instance("i-2"), DEFAULT_TTL).unwrap();
    let text = fs::read_to_string(&path).unwrap();
    assert!(!text.contains(&who.to_hex()), "{text}");
    assert!(joins.identify(&minted.token).is_none());
}

/// A hello and a heartbeat the token serves move the bound: one millisecond before it,
/// a heartbeat passes and the token then lasts a full bound from there, a hello does
/// the same, and the new time is in the file. It catches a bound that is not renewed
/// (a host that is up is cut off at the bound).
#[test]
fn serving_a_heartbeat_or_a_hello_renews_the_bound() {
    let path = scratch("renew");
    let (joins, clock) = tokens(&path);
    let who = credential(&joins.mint(instance("i-1"), DEFAULT_TTL).unwrap());
    joins.admit(&who, "host-a", Some(&instance("i-1"))).unwrap();

    let before = T0 + CREDENTIAL_MS - 1;
    clock.set(Timestamp::from_millis(before));
    assert_eq!(joins.serves(&who, "host-a"), Ok(()));
    assert_eq!(held_expiry(&path), before + CREDENTIAL_MS);
    clock.set(Timestamp::from_millis(before + CREDENTIAL_MS - 1));
    assert_eq!(joins.serves(&who, "host-a"), Ok(()));
    assert_eq!(
        joins.admit(&who, "host-a", Some(&instance("i-1"))),
        Ok(Admission::Again)
    );
    let again = before + CREDENTIAL_MS - 1;
    assert_eq!(held_expiry(&path), again + CREDENTIAL_MS);
    clock.set(Timestamp::from_millis(again + CREDENTIAL_MS));
    assert_eq!(joins.serves(&who, "host-a"), Err(JoinRefusal::Expired));
}

/// The bound is written only when it gains a step: just under it nothing is written,
/// at it the file moves. It catches a heartbeat that writes and syncs the file every
/// time, and a step that never lets the file move.
#[test]
fn a_renewal_is_written_only_when_it_gains_a_step() {
    let path = scratch("step");
    let (joins, clock) = tokens(&path);
    let who = credential(&joins.mint(instance("i-1"), DEFAULT_TTL).unwrap());
    joins.admit(&who, "host-a", Some(&instance("i-1"))).unwrap();
    let step = refresh_step_ms(&who.to_hex());
    clock.set(Timestamp::from_millis(T0 + step - 1));
    joins.serves(&who, "host-a").unwrap();
    assert_eq!(held_expiry(&path), T0 + CREDENTIAL_MS);
    clock.set(Timestamp::from_millis(T0 + step));
    joins.serves(&who, "host-a").unwrap();
    assert_eq!(held_expiry(&path), T0 + step + CREDENTIAL_MS);
}

/// A renewal the file does not take is not kept in memory either: the heartbeat is
/// served, and the token still ends at the bound the file holds. It catches a time that
/// lives in memory alone, which a restart would take back.
#[test]
fn a_renewal_the_file_refuses_is_not_kept() {
    let path = scratch("renew_unwritable");
    let (joins, clock) = tokens(&path);
    let who = credential(&joins.mint(instance("i-1"), DEFAULT_TTL).unwrap());
    joins.admit(&who, "host-a", Some(&instance("i-1"))).unwrap();
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    clock.set(Timestamp::from_millis(T0 + STEP_MS + SPREAD_MS));
    assert_eq!(joins.serves(&who, "host-a"), Ok(()));
    clock.set(Timestamp::from_millis(T0 + CREDENTIAL_MS));
    assert_eq!(joins.serves(&who, "host-a"), Err(JoinRefusal::Expired));
}

/// A use that is given back leaves the token unused for its instance: another host
/// id can use it, as can the same one. A release by a host that does not hold the
/// token, or of a token nobody holds, changes nothing. It catches a refused hello that
/// burns the token and a release that frees another host's token.
#[test]
fn a_released_use_leaves_the_token_unused() {
    let path = scratch("release");
    let (joins, _) = tokens(&path);
    let who = credential(&joins.mint(instance("i-1"), DEFAULT_TTL).unwrap());
    assert_eq!(
        joins.admit(&who, "host-a", Some(&instance("i-1"))),
        Ok(Admission::Fresh)
    );
    joins.release(&who, "host-b");
    joins.release(&Credential::of_token("never-minted"), "host-a");
    assert_eq!(joins.serves(&who, "host-a"), Ok(()));
    joins.release(&who, "host-a");
    assert_eq!(joins.serves(&who, "host-a"), Err(JoinRefusal::NotAdmitted));
    let text = fs::read_to_string(&path).unwrap();
    assert!(!text.contains("used_by"), "{text}");
    assert!(!text.contains("credential_expires_at_ms"), "{text}");
    assert_eq!(
        joins.admit(&who, "host-b", Some(&instance("i-1"))),
        Ok(Admission::Fresh)
    );
}

/// The credential time of the used token `who`, as the server holds it.
fn held_in_memory(joins: &JoinTokens, who: &Credential) -> u64 {
    let entries = joins.lock();
    entries[&who.to_hex()]
        .used
        .as_ref()
        .unwrap()
        .credential_expires_at_ms
}

/// A release of a use that a later hello of the same host followed does nothing: hello
/// A is admitted fresh, hello B of the same host is admitted again (the registry may have
/// accepted B and refused A), and A's release then leaves the token used by the host, so
/// that another host id holding a copy is refused as used and the host still heartbeats.
/// It catches a release that frees a token a registered host holds.
#[test]
fn a_release_after_a_hello_served_again_keeps_the_use() {
    let path = scratch("release_after_again");
    let (joins, _) = tokens(&path);
    let who = credential(&joins.mint(instance("i-1"), DEFAULT_TTL).unwrap());
    let here = instance("i-1");
    assert_eq!(
        joins.admit(&who, "host-a", Some(&here)),
        Ok(Admission::Fresh)
    );
    assert_eq!(
        joins.admit(&who, "host-a", Some(&here)),
        Ok(Admission::Again)
    );
    joins.release(&who, "host-a");
    assert_eq!(joins.serves(&who, "host-a"), Ok(()));
    assert_eq!(
        joins.admit(&who, "host-b", Some(&here)),
        Err(JoinRefusal::Used)
    );
    assert!(fs::read_to_string(&path).unwrap().contains("used_by"));
}

/// Tokens first used at the same moment come due for a write at different times: every
/// step lies in [step, step + spread), the steps are not all one value, and of the tokens
/// with the least and the greatest step, a heartbeat at the least step moves the first
/// and not the second, and one at the greatest moves both. It catches a step that is the
/// same for every token (a stampede of whole-file writes) and one outside its bounds.
#[test]
fn renewals_of_tokens_used_together_come_due_apart() {
    let path = scratch("spread");
    let (joins, clock) = tokens(&path);
    let mut all = Vec::new();
    for n in 0..8 {
        let id = format!("i-{n}");
        let who = credential(&joins.mint(instance(&id), DEFAULT_TTL).unwrap());
        let host = format!("host-{n}");
        joins.admit(&who, &host, Some(&instance(&id))).unwrap();
        all.push((refresh_step_ms(&who.to_hex()), who, host));
    }
    assert!(
        all.iter()
            .all(|(step, _, _)| (STEP_MS..STEP_MS + SPREAD_MS).contains(step))
    );
    all.sort_by_key(|(step, _, _)| *step);
    let (least, first, first_host) = &all[0];
    let (greatest, last, last_host) = &all[all.len() - 1];
    assert!(least < greatest, "every token has the step {least}");
    clock.set(Timestamp::from_millis(T0 + least));
    joins.serves(first, first_host).unwrap();
    joins.serves(last, last_host).unwrap();
    assert_eq!(held_in_memory(&joins, first), T0 + least + CREDENTIAL_MS);
    assert_eq!(held_in_memory(&joins, last), T0 + CREDENTIAL_MS);
    clock.set(Timestamp::from_millis(T0 + greatest));
    joins.serves(last, last_host).unwrap();
    assert_eq!(held_in_memory(&joins, last), T0 + greatest + CREDENTIAL_MS);
}

/// A release the file refuses leaves the token tied to the host, in memory as in the
/// file. It catches a release that frees the token in memory while the file still
/// holds the use.
#[test]
fn a_release_the_file_refuses_keeps_the_use() {
    let path = scratch("release_unwritable");
    let (joins, _) = tokens(&path);
    let who = credential(&joins.mint(instance("i-1"), DEFAULT_TTL).unwrap());
    joins.admit(&who, "host-a", Some(&instance("i-1"))).unwrap();
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    joins.release(&who, "host-a");
    assert_eq!(joins.serves(&who, "host-a"), Ok(()));
}

/// A row with a use and no credential time, or the time and no use, is not a valid
/// file. It catches a file that reads a used token as a credential with no end.
#[test]
fn a_half_used_row_is_refused() {
    let hash = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    for extra in ["used_by = \"h\"\n", "credential_expires_at_ms = 5\n"] {
        let text = format!(
            "version = 1\n[[join]]\nhash = \"{hash}\"\nprovider = \"p\"\ninstance_id = \"i\"\nexpires_at_ms = 1\n{extra}"
        );
        let error = parse(&text).unwrap_err();
        assert!(error.contains("only one of"), "{error}");
    }
}
