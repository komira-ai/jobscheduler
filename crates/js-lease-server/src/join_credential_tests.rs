//! Tests of the host credential that takes the place of a one-time token at the first
//! hello of a host.

use std::fs;
use std::os::unix::fs::PermissionsExt;

use js_lease::{ManualClock, Timestamp};
use js_registry::SequentialSessions;

use super::*;

const T0: u64 = 5_000_000;
const HOUR_MS: u64 = 60 * 60 * 1000;

fn instance(id: &str) -> InstanceIdentity {
    InstanceIdentity {
        provider: "lambdalabs".to_string(),
        instance_id: id.to_string(),
    }
}

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/join_credential_unit")
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

struct NoRandom;

impl SessionSource for NoRandom {
    fn fresh(&self) -> Option<[u8; 32]> {
        None
    }
}

/// A token that registered a host and was swapped for a credential.
struct Swapped {
    token_text: String,
    token: Credential,
    secret: HostCredential,
    digest: Credential,
}

/// Mints a token for `id` and registers `host` with it, asking for a credential.
fn swapped(joins: &JoinTokens, id: &str, host: &str) -> Swapped {
    let minted = joins.mint(instance(id), DEFAULT_TTL).unwrap();
    let token = Credential::of_token(&minted.token);
    let admitted = joins
        .admit_with(&token, host, Some(&instance(id)), true)
        .unwrap();
    assert_eq!(admitted.admission, Admission::Fresh);
    let secret = admitted.issued.unwrap();
    let digest = Credential::of_token(secret.expose());
    Swapped {
        token_text: minted.token,
        token,
        secret,
        digest,
    }
}

fn expiry_in_file(path: &Path) -> u64 {
    let text = fs::read_to_string(path).unwrap();
    let line = text
        .lines()
        .find(|line| line.starts_with("credential_expires_at_ms"))
        .unwrap();
    line.split("= ").nth(1).unwrap().parse().unwrap()
}

/// The first hello that asks for a credential and registers its host with an unused token
/// is given a credential, and the token is retired in the same step: it is unknown (no
/// authentication, no heartbeat, no second hello), while the credential serves exactly the
/// host the token registered and says hello again as the token did. The registry still
/// knows the host by the token's digest. It catches a token that stays good beside the
/// credential, a credential that serves another host, and a credential the registry would
/// refuse as another one.
#[test]
fn the_first_hello_swaps_the_token_for_a_credential() {
    let path = scratch("swap");
    let (joins, _) = tokens(&path);
    let one = swapped(&joins, "i-1", "host-a");
    assert_eq!(one.secret.expose().len(), 64);
    assert_ne!(one.secret.expose(), one.token_text);
    assert!(joins.identify(&one.token_text).is_none());
    assert_eq!(
        joins.identify(one.secret.expose()),
        Some(one.digest.clone())
    );
    assert_eq!(
        joins.serves(&one.token, "host-a"),
        Err(JoinRefusal::NotAdmitted)
    );
    assert_eq!(
        joins.admit_with(&one.token, "host-a", Some(&instance("i-1")), true),
        Err(JoinRefusal::Expired)
    );
    assert_eq!(joins.serves(&one.digest, "host-a"), Ok(()));
    assert_eq!(
        joins.serves(&one.digest, "host-b"),
        Err(JoinRefusal::NotAdmitted)
    );
    let again = joins.admit_with(&one.digest, "host-a", None, true);
    assert_eq!(
        again,
        Ok(Admitted {
            admission: Admission::Again,
            issued: None
        })
    );
    assert_eq!(
        joins.admit_with(&one.digest, "host-b", None, true),
        Err(JoinRefusal::Used)
    );
    assert_eq!(joins.registered_as(&one.digest), one.token);
    assert_eq!(joins.registered_as(&one.token), one.token);
    let pooled = Credential::of_token("a-token-of-the-pool");
    assert_eq!(joins.registered_as(&pooled), pooled);
}

/// A hello that does not ask for a credential leaves the token as it was: it is still
/// known and serves its host, no credential is made, and a later hello of the same host
/// that does ask finds the token already used and swaps nothing. It catches a server that
/// retires the token of an older agent, which would then be locked out with nothing in
/// its hand, and one that swaps a token in the middle of its life.
#[test]
fn a_hello_that_asks_for_nothing_leaves_the_token() {
    let path = scratch("quiet");
    let (joins, _) = tokens(&path);
    let minted = joins.mint(instance("i-1"), DEFAULT_TTL).unwrap();
    let token = Credential::of_token(&minted.token);
    let plain = joins.admit_with(&token, "host-a", Some(&instance("i-1")), false);
    assert_eq!(
        plain,
        Ok(Admitted {
            admission: Admission::Fresh,
            issued: None
        })
    );
    let later = joins.admit_with(&token, "host-a", Some(&instance("i-1")), true);
    assert_eq!(
        later,
        Ok(Admitted {
            admission: Admission::Again,
            issued: None
        })
    );
    assert!(joins.identify(&minted.token).is_some());
    assert_eq!(joins.serves(&token, "host-a"), Ok(()));
    assert!(!fs::read_to_string(&path).unwrap().contains("replaces"));
}

/// The file holds the digest of the credential and the digest of the token it replaced, and
/// neither secret; it is owner-only; and a restart reads the same state: the credential is
/// known and serves its host, the token is not, and the registry's identity of the host is
/// still the token's. It catches a credential stored in the clear, one that is kept in
/// memory only (a restart would lock the host out), and a file that gives the token back.
#[test]
fn the_file_holds_digests_only_and_a_restart_reads_it_back() {
    let path = scratch("file");
    let (joins, clock) = tokens(&path);
    let one = swapped(&joins, "i-1", "host-a");
    let text = fs::read_to_string(&path).unwrap();
    assert!(!text.contains(one.secret.expose()), "{text}");
    assert!(!text.contains(&one.token_text), "{text}");
    assert!(text.contains(&one.digest.to_hex()), "{text}");
    assert!(
        text.contains(&format!("replaces = \"{}\"", one.token.to_hex())),
        "{text}"
    );
    assert!(
        !text.contains(&format!("hash = \"{}\"", one.token.to_hex())),
        "{text}"
    );
    assert_eq!(text.matches("[[join]]").count(), 1);
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    drop(joins);

    let shared = SharedClock::new(clock);
    let reopened = JoinTokens::open(path, shared, Box::new(SequentialSessions::new())).unwrap();
    assert!(reopened.identify(one.secret.expose()).is_some());
    assert!(reopened.identify(&one.token_text).is_none());
    assert_eq!(reopened.serves(&one.digest, "host-a"), Ok(()));
    assert_eq!(reopened.registered_as(&one.digest), one.token);
}

/// The credential ends after the same silence as a token: a heartbeat moves the bound as it
/// does for a token (the file takes it once it gains a step), and at the bound the
/// credential is refused as expired by heartbeat and by hello, and the next mint drops its
/// row. It catches a credential that lasts for ever, one that never moves with the host's
/// life, and one that stays in the file after its time.
#[test]
fn the_credential_has_the_silence_bound_of_a_token() {
    let path = scratch("bound");
    let (joins, clock) = tokens(&path);
    let one = swapped(&joins, "i-1", "host-a");
    let bound = duration_ms(CREDENTIAL_TTL);
    assert_eq!(expiry_in_file(&path), T0 + bound);
    clock.advance(Duration::from_millis(2 * HOUR_MS));
    assert_eq!(joins.serves(&one.digest, "host-a"), Ok(()));
    assert_eq!(expiry_in_file(&path), T0 + 2 * HOUR_MS + bound);
    clock.advance(Duration::from_millis(bound));
    assert_eq!(
        joins.serves(&one.digest, "host-a"),
        Err(JoinRefusal::Expired)
    );
    assert_eq!(
        joins.admit_with(&one.digest, "host-a", None, false),
        Err(JoinRefusal::Expired)
    );
    joins.mint(instance("i-2"), DEFAULT_TTL).unwrap();
    assert!(!fs::read_to_string(&path).unwrap().contains("replaces"));
}

/// A revoke of the instance drops its credential with the rest of its rows, and only that
/// instance's: the credential is unknown afterwards, in memory and in the file, a second
/// revoke drops nothing, and another instance's credential serves on. It catches a revoke
/// that leaves a credential good after the host is released.
#[test]
fn a_revoke_drops_the_credential_with_its_instance() {
    let path = scratch("revoke");
    let (joins, _) = tokens(&path);
    let one = swapped(&joins, "i-1", "host-a");
    let two = swapped(&joins, "i-2", "host-b");
    assert_eq!(joins.revoke(&instance("i-1")), Ok(1));
    assert!(joins.identify(one.secret.expose()).is_none());
    assert_eq!(
        joins.serves(&one.digest, "host-a"),
        Err(JoinRefusal::NotAdmitted)
    );
    assert_eq!(joins.registered_as(&one.digest), one.digest);
    assert_eq!(joins.serves(&two.digest, "host-b"), Ok(()));
    let text = fs::read_to_string(&path).unwrap();
    assert!(!text.contains(&one.digest.to_hex()), "{text}");
    assert_eq!(text.matches("[[join]]").count(), 1);
    assert_eq!(joins.revoke(&instance("i-1")), Ok(0));
}

/// A later mint for the instance of a live host leaves the host's credential alone and
/// adds the new token, as it leaves a used token alone. It catches a relaunch that drops
/// the credential of a host that is still running.
#[test]
fn a_mint_for_the_instance_keeps_the_live_credential() {
    let path = scratch("mint");
    let (joins, _) = tokens(&path);
    let one = swapped(&joins, "i-1", "host-a");
    let minted = joins.mint(instance("i-1"), DEFAULT_TTL).unwrap();
    assert!(joins.identify(&minted.token).is_some());
    assert_eq!(joins.serves(&one.digest, "host-a"), Ok(()));
    assert_eq!(
        fs::read_to_string(&path)
            .unwrap()
            .matches("[[join]]")
            .count(),
        2
    );
}

/// With no random bytes the hello that asks for a credential is refused with its own code
/// and nothing is used: the token is as it was and a hello that asks for nothing registers
/// the host. It catches a token retired without a credential to take its place.
#[test]
fn a_credential_that_cannot_be_made_retires_nothing() {
    let path = scratch("no_random");
    let (joins, clock) = tokens(&path);
    let minted = joins.mint(instance("i-1"), DEFAULT_TTL).unwrap();
    let token = Credential::of_token(&minted.token);
    drop(joins);
    let shared = SharedClock::new(clock);
    let dry = JoinTokens::open(path, shared, Box::new(NoRandom)).unwrap();
    let refused = dry.admit_with(&token, "host-a", Some(&instance("i-1")), true);
    assert_eq!(refused, Err(JoinRefusal::Unavailable));
    assert_eq!(JoinRefusal::Unavailable.code(), "join-token-unavailable");
    assert!(dry.identify(&minted.token).is_some());
    assert_eq!(dry.serves(&token, "host-a"), Err(JoinRefusal::NotAdmitted));
    let plain = dry.admit_with(&token, "host-a", Some(&instance("i-1")), false);
    assert_eq!(
        plain.map(|admitted| admitted.admission),
        Ok(Admission::Fresh)
    );
}

/// A file that cannot be written ends the hello that asks for a credential with `Unsaved`:
/// the token is still unused, and no credential row exists, in memory or in the file. It
/// catches a token retired in memory alone, which a restart would give back with a
/// credential nobody holds.
#[test]
fn a_credential_the_file_refuses_retires_nothing() {
    let path = scratch("unwritable");
    let (joins, _) = tokens(&path);
    let minted = joins.mint(instance("i-1"), DEFAULT_TTL).unwrap();
    let token = Credential::of_token(&minted.token);
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    let blocked = joins.admit_with(&token, "host-a", Some(&instance("i-1")), true);
    assert_eq!(blocked, Err(JoinRefusal::Unsaved));
    assert!(joins.identify(&minted.token).is_some());
    let entries = joins.lock();
    assert_eq!(entries.len(), 1);
    assert!(entries.values().all(|entry| entry.replaces.is_none()));
}

/// The use that issued a credential is given back when the registry refuses the hello:
/// the credential is unknown, the token is unused again for its instance and keeps its
/// first expiry (it is not extended), and a later hello can be given a credential. A
/// release by another host, or of a token nobody swapped, changes nothing, and neither
/// does a release for a host whose instance holds a credential of another token. It
/// catches a refused hello that leaves the host with a retired token and a credential it
/// never received, and a release that frees another host's credential.
#[test]
fn a_release_takes_the_credential_back_and_returns_the_token() {
    let path = scratch("release");
    let (joins, clock) = tokens(&path);
    let one = swapped(&joins, "i-1", "host-a");
    let two = swapped(&joins, "i-2", "host-b");
    joins.release(&one.token, "host-b");
    joins.release(&Credential::of_token("never-minted"), "host-a");
    assert_eq!(joins.serves(&one.digest, "host-a"), Ok(()));
    joins.release(&one.token, "host-a");
    assert!(joins.identify(one.secret.expose()).is_none());
    assert!(joins.identify(&one.token_text).is_some());
    assert_eq!(joins.serves(&two.digest, "host-b"), Ok(()));
    let text = fs::read_to_string(&path).unwrap();
    assert_eq!(text.matches("replaces").count(), 1, "{text}");
    assert!(!text.contains("host-a"), "{text}");
    drop(text);

    let next = joins.admit_with(&one.token, "host-c", Some(&instance("i-1")), true);
    let issued = next.unwrap().issued.unwrap();
    assert_ne!(issued, one.secret);
    joins.release(&one.token, "host-c");
    clock.advance(Duration::from_secs(31 * 60));
    assert_eq!(
        joins.admit_with(&one.token, "host-c", Some(&instance("i-1")), true),
        Err(JoinRefusal::Expired)
    );
}

/// A release the file refuses keeps the use and the credential, as for a token: the
/// credential serves its host and the token stays retired, in step with what a restart
/// would read.
#[test]
fn a_release_the_file_refuses_keeps_the_credential() {
    let path = scratch("release_unwritable");
    let (joins, _) = tokens(&path);
    let one = swapped(&joins, "i-1", "host-a");
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    joins.release(&one.token, "host-a");
    assert!(joins.identify(one.secret.expose()).is_some());
    assert!(joins.identify(&one.token_text).is_none());
}

/// A row of a host credential reads back as written, and two faults are refused by name: a
/// replaced-token digest that is not 64 lower-case hex digits, and a credential row with no
/// host. It catches a start that reads a credential as an unused token, which would let the
/// credential register a second host.
#[test]
fn a_damaged_credential_row_is_refused() {
    let hash = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let other = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";
    let row = |extra: &str| {
        format!(
            "version = 1\n[[join]]\nhash = \"{hash}\"\nprovider = \"p\"\ninstance_id = \"i\"\nexpires_at_ms = 1\n{extra}"
        )
    };
    let good = format!("used_by = \"h\"\ncredential_expires_at_ms = 5\nreplaces = \"{other}\"\n");
    let read = parse(&row(&good)).unwrap();
    assert_eq!(read[hash].replaces.as_deref(), Some(other));
    assert_eq!(parse(&render(&read).unwrap()).unwrap(), read);
    let short = "used_by = \"h\"\ncredential_expires_at_ms = 5\nreplaces = \"abc\"\n";
    assert!(
        parse(&row(short))
            .unwrap_err()
            .contains("replaced token hash")
    );
    let hostless = format!("replaces = \"{other}\"\n");
    assert_eq!(
        parse(&row(&hostless)).unwrap_err(),
        "a host credential has no host"
    );
}

/// What a granted admission prints is free of the credential it carries. It catches a
/// derived `Debug` on the credential, which a log line of the result would print.
#[test]
fn an_admission_prints_no_secret() {
    let path = scratch("debug");
    let (joins, _) = tokens(&path);
    let one = swapped(&joins, "i-1", "host-a");
    let admitted = Admitted {
        admission: Admission::Fresh,
        issued: Some(one.secret.clone()),
    };
    let text = format!("{admitted:?}");
    assert!(!text.contains(one.secret.expose()), "{text}");
    assert!(text.contains("HostCredential(***)"), "{text}");
}
