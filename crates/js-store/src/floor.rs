//! The `floor/<class>/<dedup>` and `keyfloor/<key>` rows: the memory floors
//! ([`js_sched::Floor`]) and key floors ([`js_sched::KeyFloor`]) the scheduler keeps, written
//! with the give-up that raises them ([`Rows::requeued`]) and read at a start.
//!
//! A floor row's key is `floor/`, the work key's class in hexadecimal (the class may hold any
//! character, a `/` too), `/`, and its dedup text ([`crate::keys::floor`]). The value is a
//! version byte (1), the ask (8 bytes, big-endian), the stamp (8), the class (2-byte length,
//! then the bytes) and the dedup text to the end. A key floor's key is `keyfloor/` and the
//! size key as sixteen hexadecimal digits; its value is a version byte (1), the key, the ask
//! and the stamp (8 bytes each, big-endian), then the name of the size summary the key's floor
//! decays with (behind a two-byte length; empty for none), which this crate keeps as the text
//! it is given and never reads. A row of another version is refused, not guessed at.
//!
//! A floor outlives the operation that raised it and has no expiry. A floor is never lowered
//! by a later writer ([`Rows::requeued`] writes the larger ask of the row and its own).
//!
//! The dedup text of a floor row is not checked to be anything but text: the farm server
//! names the work by an action's digest, and a row of another class is as good a row here.

use js_sched::{Floor, KeyFloor};
use js_state::{Cond, Txn, Versioned};
use js_types::sched::WorkKey;

use crate::blocks::contended;
use crate::{Rows, StateError, keys};

const VERSION: u8 = 1;
const EARLY: &str = "the row ends early";

/// `floor` as a row value, or `None` if its class is too long for it.
pub(crate) fn encode(floor: &Floor) -> Option<Vec<u8>> {
    let class = floor.key.class.as_bytes();
    let mut row = vec![VERSION];
    row.extend(floor.ask.to_be_bytes());
    row.extend(floor.stamp.to_be_bytes());
    row.extend(u16::try_from(class.len()).ok()?.to_be_bytes());
    row.extend(class);
    row.extend(floor.key.dedup.as_bytes());
    Some(row)
}

/// A row value as a [`Floor`], or what is wrong with it.
pub(crate) fn decode(row: &[u8]) -> Result<Floor, &'static str> {
    let (version, rest) = row.split_first().ok_or(EARLY)?;
    if *version != VERSION {
        return Err("not a version 1 floor");
    }
    let (ask, rest) = rest.split_first_chunk::<8>().ok_or(EARLY)?;
    let (stamp, rest) = rest.split_first_chunk::<8>().ok_or(EARLY)?;
    let (len, rest) = rest.split_first_chunk::<2>().ok_or(EARLY)?;
    let len = usize::from(u16::from_be_bytes(*len));
    let (class, dedup) = rest.split_at_checked(len).ok_or(EARLY)?;
    let class = std::str::from_utf8(class).map_err(|_| "a class name is not text")?;
    let dedup = std::str::from_utf8(dedup).map_err(|_| "a dedup text is not text")?;
    Ok(Floor {
        key: WorkKey::new(class, dedup),
        ask: u64::from_be_bytes(*ask),
        stamp: u64::from_be_bytes(*stamp),
    })
}

/// `floor` as a row value, naming the size summary `summary` (empty for none).
pub(crate) fn encode_key(floor: &KeyFloor, summary: Option<&str>) -> Vec<u8> {
    let name = summary.unwrap_or("").as_bytes();
    let mut row = vec![VERSION];
    row.extend(floor.key.to_be_bytes());
    row.extend(floor.ask.to_be_bytes());
    row.extend(floor.stamp.to_be_bytes());
    row.extend(u16::try_from(name.len()).unwrap_or(0).to_be_bytes());
    row.extend(name);
    row
}

/// A `keyfloor/` row value as a [`KeyFloor`] and the name of its size summary, or what is
/// wrong with it.
pub(crate) fn decode_key(row: &[u8]) -> Result<(KeyFloor, Option<String>), &'static str> {
    let (version, rest) = row.split_first().ok_or(EARLY)?;
    if *version != VERSION {
        return Err("not a version 1 key floor");
    }
    let (key, rest) = rest.split_first_chunk::<8>().ok_or(EARLY)?;
    let (ask, rest) = rest.split_first_chunk::<8>().ok_or(EARLY)?;
    let (stamp, rest) = rest.split_first_chunk::<8>().ok_or(EARLY)?;
    let (len, rest) = rest.split_first_chunk::<2>().ok_or(EARLY)?;
    if rest.len() != usize::from(u16::from_be_bytes(*len)) {
        return Err("the row is not the length it says");
    }
    let name = std::str::from_utf8(rest).map_err(|_| "a summary name is not text")?;
    let floor = KeyFloor {
        key: u64::from_be_bytes(*key),
        ask: u64::from_be_bytes(*ask),
        stamp: u64::from_be_bytes(*stamp),
    };
    Ok((floor, (!name.is_empty()).then(|| name.to_owned())))
}

/// `floors`, oldest stamp first (two floors with one stamp in key order, as the scan gave
/// them), the later of two with the stamp after the earlier's: frontends sharing a store stamp
/// their raises from counters of their own, so two floors may carry one stamp.
fn restamped<T>(
    mut floors: Vec<T>,
    stamp: impl Fn(&T) -> u64,
    set: impl Fn(&mut T, u64),
) -> Vec<T> {
    floors.sort_by_key(&stamp);
    let mut next = 0;
    for floor in &mut floors {
        let stamped = stamp(floor).max(next);
        set(floor, stamped);
        next = stamped.saturating_add(1);
    }
    floors
}

/// What a start reads of the key floors: the floors, and the size summary name each row
/// carries, for those that name one.
pub(crate) type KeyFloors = (Vec<KeyFloor>, Vec<(u64, String)>);

impl Rows {
    /// Every memory floor, oldest stamp first (two floors with one stamp in key order, the
    /// later with the next stamp).
    ///
    /// # Errors
    /// The store cannot be read, a row is not a floor, or a row is not under the key of the
    /// floor it holds.
    pub(crate) fn stored_floors(&self) -> Result<Vec<Floor>, StateError> {
        let mut floors = Vec::new();
        for (key, row) in self.store.scan_prefix(keys::FLOOR)? {
            let floor = decode(&row.value).map_err(|why| StateError::corrupt(&key, why))?;
            if key != keys::floor(&floor.key) {
                return Err(StateError::corrupt(
                    &key,
                    "not the key of the floor it holds",
                ));
            }
            floors.push(floor);
        }
        Ok(restamped(floors, |f| f.stamp, |f, stamp| f.stamp = stamp))
    }

    /// Every key floor, in the same order as [`Rows::stored_floors`], and the size summary
    /// name of each row that names one.
    ///
    /// # Errors
    /// The store cannot be read, a row is not a key floor, or a row is not under the key of
    /// the key floor it holds.
    pub(crate) fn stored_key_floors(&self) -> Result<KeyFloors, StateError> {
        let mut floors = Vec::new();
        let mut names = Vec::new();
        for (key, row) in self.store.scan_prefix(keys::KEY_FLOOR)? {
            let (floor, name) =
                decode_key(&row.value).map_err(|why| StateError::corrupt(&key, why))?;
            if key != keys::key_floor(floor.key) {
                return Err(StateError::corrupt(
                    &key,
                    "not the key of the key floor it holds",
                ));
            }
            names.extend(name.map(|name| (floor.key, name)));
            floors.push(floor);
        }
        let floors = restamped(floors, |f| f.stamp, |f, stamp| f.stamp = stamp);
        Ok((floors, names))
    }

    /// Every memory floor the store holds now, each as its row says: for a frontend that
    /// raises its own floors to the ones the others raised since it started. Unlike the read
    /// of a start ([`Rows::load`]) the rows are not restamped and not compared with their
    /// keys.
    ///
    /// # Errors
    /// The store cannot be read, or a row is not a floor.
    pub fn floors(&self) -> Result<Vec<Floor>, StateError> {
        let mut floors = Vec::new();
        for (key, row) in self.store.scan_prefix(keys::FLOOR)? {
            floors.push(decode(&row.value).map_err(|why| StateError::corrupt(&key, why))?);
        }
        Ok(floors)
    }

    /// `floor` with the larger ask of its own and the stored row's, and what a write of the
    /// row must find: the version read, or no row. A row that cannot be read counts for
    /// nothing, and is written over.
    pub(crate) fn merged_floor(&self, floor: &Floor) -> Result<(Floor, Cond), StateError> {
        let row = self.store.get(&keys::floor(&floor.key))?;
        let held = row
            .as_ref()
            .and_then(|row| decode(&row.value).ok())
            .map_or(0, |held| held.ask);
        let merged = Floor {
            ask: held.max(floor.ask),
            ..floor.clone()
        };
        Ok((merged, found(row)))
    }

    /// [`Rows::merged_floor`] for a key floor.
    pub(crate) fn merged_key_floor(
        &self,
        floor: &KeyFloor,
    ) -> Result<(KeyFloor, Cond), StateError> {
        let row = self.store.get(&keys::key_floor(floor.key))?;
        let held = row
            .as_ref()
            .and_then(|row| decode_key(&row.value).ok())
            .map_or(0, |(held, _)| held.ask);
        let merged = KeyFloor {
            ask: held.max(floor.ask),
            ..floor.clone()
        };
        Ok((merged, found(row)))
    }

    /// The value of the `keyfloor/` row of `floor`: it names the size summary `summary` when
    /// it is given and fits, else the name the row already holds (a write that does not know
    /// the summary leaves the name where it was).
    pub(crate) fn key_floor_value(
        &self,
        floor: &KeyFloor,
        summary: Option<&str>,
    ) -> Result<Vec<u8>, StateError> {
        let row = self.store.get(&keys::key_floor(floor.key))?;
        Ok(key_floor_over(floor, summary, row.as_ref()))
    }

    /// Writes the key floor `floor` the estimator's decay lowered from `before`, or deletes
    /// its row when it was forgotten (an `ask` of 0), only over the row it read: the write
    /// is made at the version read, and a row that changed since is read again (at most
    /// [`crate::CAS_TRIES`] tries in all). The row is left alone when it holds more than
    /// `before` (another frontend raised it after the decay's last look at the key, and the
    /// higher floor is the safe side) and when it already holds `floor.ask` or less. With no
    /// row the floor is written (an earlier write of it was refused), and a forgotten floor
    /// has nothing to delete. The row keeps its stamp, and names the size summary `summary`
    /// that its floor decays with, or the one it already names.
    ///
    /// # Errors
    /// The store cannot be read or written, or other writers won every try
    /// ([`StateError::Conflict`]).
    pub fn lower_key_floor(
        &self,
        floor: &KeyFloor,
        before: u64,
        summary: Option<&str>,
    ) -> Result<(), StateError> {
        let key = keys::key_floor(floor.key);
        contended(|| {
            let row = self.store.get(&key)?;
            // A row that cannot be read counts for nothing, and is written over.
            let held = row
                .as_ref()
                .map(|row| decode_key(&row.value).map_or(0, |(held, _)| held.ask));
            if held.is_some_and(|held| held > before || (held != 0 && held <= floor.ask)) {
                return Ok(());
            }
            let txn = if floor.ask == 0 {
                if row.is_none() {
                    return Ok(());
                }
                Txn::new().delete(key.clone(), found(row))
            } else {
                let value = key_floor_over(floor, summary, row.as_ref());
                Txn::new().put(key.clone(), value, found(row))
            };
            self.commit(txn)
        })
    }
}

/// The value of the `keyfloor/` row of `floor` over `row`, the row as it stands: it names
/// the size summary `summary` when it is given and fits, else the name `row` holds.
fn key_floor_over(floor: &KeyFloor, summary: Option<&str>, row: Option<&Versioned>) -> Vec<u8> {
    let given = summary.filter(|name| name.len() <= usize::from(u16::MAX));
    let name = match given {
        Some(name) => Some(name.to_owned()),
        None => row
            .and_then(|row| decode_key(&row.value).ok())
            .and_then(|(_, name)| name),
    };
    encode_key(floor, name.as_deref())
}

/// What a write that replaces `row` must find: its version, or no row.
pub(crate) fn found(row: Option<Versioned>) -> Cond {
    row.map_or(Cond::Absent, |row| Cond::Version(row.version))
}
