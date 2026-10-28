//! One-time join tokens: minted for one launched instance, used once to register one host.
//!
//! The pool's join tokens (`--agent-token-file`) are shared by every host of the pool. A
//! machine that a provider launches for the pool should not hold such a token: it is
//! delivered after boot, and whoever reads it can register hosts until the operator
//! rotates it. A one-time token is the narrower credential. The operator's tooling mints
//! one per launched instance (`POST /v1/agent/join-tokens`, behind the client token),
//! names the provider's instance id, and delivers the token to that instance only.
//!
//! A token is bound to the instance (provider and instance id) and to a time. The first
//! hello that presents it, names that instance and arrives before the expiry registers the
//! host and consumes the token: it is then tied to the host id of that hello. After that
//! the token is the credential of that one host. It carries the host's later hellos (the
//! agent says hello again after a lost connection) and heartbeats, and nothing else: a
//! hello for another host id, or a heartbeat for one, is refused, and so is a hello of a
//! token that was never used once its time has passed.
//!
//! Only the SHA-256 digest of a token is kept, in memory and in `<state file>.joins`, which
//! is written whole through a temporary file and renamed, like the file of bindings, and
//! is owner-only:
//!
//! ```toml
//! version = 1
//!
//! [[join]]
//! hash = "<64 hex digits: SHA-256 of the token>"
//! provider = "lambdalabs"
//! instance_id = "i-0abc"
//! expires_at_ms = 1700000000000
//! used_by = "host-7"          # once the token has registered a host
//! ```
//!
//! The lookup of a presented token is by its digest. The token is 256 random bits, so
//! the time a lookup takes tells nothing that could be turned into a guess.
//!
//! Every change is made on a copy, written, and kept only when the write succeeded: a
//! token is never consumed in memory alone, because a restart would give it back.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use js_agent_wire::InstanceIdentity;
use js_lease::Clock;
use js_registry::{Credential, SessionSource};
use serde::{Deserialize, Serialize};
use tracing::error;

use crate::adapt::SharedClock;
use crate::agent_state::{read_if_present, replace};
use crate::error::StartupError;

/// The flag the refusals name: the file lives beside the lease file.
const FLAG: &str = "--state-file";

/// The version this build writes.
const VERSION: u32 = 1;

/// How long a token lasts when the request does not say.
pub(crate) const DEFAULT_TTL: Duration = Duration::from_secs(30 * 60);

/// The longest a token may be asked to last.
pub(crate) const MAX_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// The most tokens that may wait to be used at once.
pub(crate) const MAX_WAITING: usize = 256;

/// What the server keeps of one token. The key is the digest of the token.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    instance: InstanceIdentity,
    expires_at_ms: u64,
    used_by: Option<String>,
}

/// The tokens by digest, as 64 lower-case hex digits.
type Entries = BTreeMap<String, Entry>;

#[derive(Debug, Serialize, Deserialize)]
struct Document {
    version: u32,
    #[serde(default)]
    join: Vec<Row>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Row {
    hash: String,
    provider: String,
    instance_id: String,
    expires_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    used_by: Option<String>,
}

/// A token just made. The text is shown once, in the answer to the request that made it.
pub(crate) struct Minted {
    pub(crate) token: String,
    pub(crate) expires_at_ms: u64,
}

/// Why no token was made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MintError {
    /// The operating system gave no random bytes.
    NoRandom,
    /// [`MAX_WAITING`] tokens already wait to be used.
    Full,
    /// The file could not be written; nothing was made.
    Unsaved,
}

/// Why a presented token was not accepted. Nothing of the token is in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JoinRefusal {
    /// The token was never used and its time has passed (or it was dropped after that).
    Expired,
    /// The token registered another host.
    Used,
    /// The hello names no instance, or one other than the token is bound to.
    WrongInstance,
    /// A heartbeat of a host the token did not register.
    NotAdmitted,
    /// The use could not be written down, so it was not allowed.
    Unsaved,
}

impl JoinRefusal {
    /// The code of the answer.
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::Expired => "join-token-expired",
            Self::Used => "join-token-used",
            Self::WrongInstance => "join-token-instance",
            Self::NotAdmitted => "join-token-not-admitted",
            Self::Unsaved => "join-token-unsaved",
        }
    }
}

/// The one-time tokens of the server.
pub(crate) struct JoinTokens {
    path: PathBuf,
    clock: SharedClock,
    random: Box<dyn SessionSource>,
    entries: Mutex<Entries>,
}

impl JoinTokens {
    /// The tokens the file at `path` holds (none when there is no file). `random` is the
    /// source of the bytes of a token.
    pub(crate) fn open(
        path: PathBuf,
        clock: SharedClock,
        random: Box<dyn SessionSource>,
    ) -> Result<Self, StartupError> {
        let entries = load(&path)?;
        Ok(Self {
            path,
            clock,
            random,
            entries: Mutex::new(entries),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Entries> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Makes the file hold `entries`.
    fn write(&self, entries: &Entries) -> io::Result<()> {
        replace(&self.path, render(entries)?.as_bytes())
    }

    /// The credential of `given` when it is a token of this set, used or not.
    pub(crate) fn identify(&self, given: &str) -> Option<Credential> {
        let credential = Credential::of_token(given);
        let known = self.lock().contains_key(&credential.to_hex());
        known.then_some(credential)
    }

    /// Makes a token for `instance` that lasts `ttl` from now.
    ///
    /// Tokens that passed their time unused are dropped first, and so is every unused
    /// token of the same instance: a second mint for an instance (the delivery of the
    /// first failed) replaces the first instead of leaving two ways in. A token that was
    /// used stays.
    pub(crate) fn mint(
        &self,
        instance: InstanceIdentity,
        ttl: Duration,
    ) -> Result<Minted, MintError> {
        let bytes = self.random.fresh().ok_or(MintError::NoRandom)?;
        let token = hex(&bytes);
        let now = self.clock.now();
        let expires_at_ms = now.plus(ttl).millis();
        let mut entries = self.lock();
        let mut next = entries.clone();
        next.retain(|_, entry| {
            entry.used_by.is_some()
                || (entry.expires_at_ms > now.millis() && entry.instance != instance)
        });
        let waiting = next
            .values()
            .filter(|entry| entry.used_by.is_none())
            .count();
        if waiting >= MAX_WAITING {
            return Err(MintError::Full);
        }
        let entry = Entry {
            instance,
            expires_at_ms,
            used_by: None,
        };
        next.insert(Credential::of_token(&token).to_hex(), entry);
        self.write(&next).map_err(|err| {
            error!(error = %err, "cannot write the join token file");
            MintError::Unsaved
        })?;
        *entries = next;
        Ok(Minted {
            token,
            expires_at_ms,
        })
    }

    /// Lets the hello of `host`, naming `instance`, register with the token whose
    /// credential is `credential`, and consumes the token when it was unused.
    ///
    /// The token is accepted when it already registered this very host (the agent says
    /// hello again), or when it is unused, not expired, and `instance` is the one it is
    /// bound to. The check, the use and the write happen under one lock, so of two hellos
    /// that race for an unused token one wins.
    pub(crate) fn admit(
        &self,
        credential: &Credential,
        host: &str,
        instance: Option<&InstanceIdentity>,
    ) -> Result<(), JoinRefusal> {
        let now = self.clock.now().millis();
        let key = credential.to_hex();
        let mut entries = self.lock();
        let entry = entries.get(&key).ok_or(JoinRefusal::Expired)?;
        if let Some(used_by) = &entry.used_by {
            return match used_by == host {
                true => Ok(()),
                false => Err(JoinRefusal::Used),
            };
        }
        if entry.expires_at_ms <= now {
            return Err(JoinRefusal::Expired);
        }
        if instance != Some(&entry.instance) {
            return Err(JoinRefusal::WrongInstance);
        }
        let used = Entry {
            used_by: Some(host.to_owned()),
            ..entry.clone()
        };
        let mut next = entries.clone();
        next.insert(key, used);
        self.write(&next).map_err(|err| {
            error!(error = %err, "cannot write the join token file");
            JoinRefusal::Unsaved
        })?;
        *entries = next;
        Ok(())
    }

    /// Whether the token whose credential is `credential` registered `host`: the only
    /// host it may heartbeat for.
    pub(crate) fn serves(&self, credential: &Credential, host: &str) -> Result<(), JoinRefusal> {
        let entries = self.lock();
        let used_by = entries
            .get(&credential.to_hex())
            .and_then(|entry| entry.used_by.as_deref());
        (used_by == Some(host))
            .then_some(())
            .ok_or(JoinRefusal::NotAdmitted)
    }
}

/// `bytes` in lower-case hex.
fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push(char::from(DIGITS[usize::from(byte >> 4)]));
        text.push(char::from(DIGITS[usize::from(byte & 15)]));
    }
    text
}

/// Reads the file at `path`: no tokens when there is no file, a refusal to start when it
/// is not owner-only or cannot be read as written (a start with no tokens would give back
/// the ones that were used).
fn load(path: &Path) -> Result<Entries, StartupError> {
    let Some(bytes) = read_if_present(path)? else {
        return Ok(Entries::new());
    };
    let text = String::from_utf8(bytes).map_err(|_| "it is not text".to_string());
    text.and_then(|text| parse(&text)).map_err(|reason| {
        let named = format!(
            "{} is not a valid join token file: {reason}",
            path.display()
        );
        StartupError::new(FLAG, named)
    })
}

/// The tokens a file's text holds, or what is wrong with it.
fn parse(text: &str) -> Result<Entries, String> {
    let document: Document = toml::from_str(text).map_err(|err| err.message().to_string())?;
    let mut entries = Entries::new();
    for row in document.join {
        if Credential::from_hex(&row.hash).is_none() {
            return Err("a token hash is not 64 lower-case hex digits".to_string());
        }
        let entry = Entry {
            instance: InstanceIdentity {
                provider: row.provider,
                instance_id: row.instance_id,
            },
            expires_at_ms: row.expires_at_ms,
            used_by: row.used_by,
        };
        if entries.insert(row.hash, entry).is_some() {
            return Err("a token hash is listed twice".to_string());
        }
    }
    Ok(entries)
}

/// The text of the file that holds `entries`.
fn render(entries: &Entries) -> io::Result<String> {
    let join = entries
        .iter()
        .map(|(hash, entry)| Row {
            hash: hash.clone(),
            provider: entry.instance.provider.clone(),
            instance_id: entry.instance.instance_id.clone(),
            expires_at_ms: entry.expires_at_ms,
            used_by: entry.used_by.clone(),
        })
        .collect();
    let document = Document {
        version: VERSION,
        join,
    };
    toml::to_string(&document).map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
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
        assert_eq!(joins.admit(&who, "host-a", Some(&instance("i-1"))), Ok(()));
        assert_eq!(joins.admit(&who, "host-a", Some(&instance("i-1"))), Ok(()));
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
        assert_eq!(joins.admit(&a, "host-a", Some(&instance("i-1"))), Ok(()));
        clock.set(Timestamp::from_millis(T0 + 60_000));
        assert_eq!(
            joins.admit(&b, "host-b", Some(&instance("i-2"))),
            Err(JoinRefusal::Expired)
        );
        assert_eq!(joins.admit(&a, "host-a", Some(&instance("i-1"))), Ok(()));
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
        assert_eq!(joins.admit(&who, "host-a", Some(&instance("i-1"))), Ok(()));
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
            Ok(())
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
}
