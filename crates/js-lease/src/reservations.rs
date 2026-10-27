//! Reservation records: the ledger rows kept beside the leases, and their conversion to
//! and from the file. Pure data; what a record means (when it is claimed, dropped or
//! extended) is the caller's policy and is not decided here.

use std::collections::BTreeMap;
use std::path::Path;

use js_types::HostId;

use crate::clock::Timestamp;
use crate::error::OpenError;
use crate::store::{self, Document, StoredExclusion, StoredReservation};
use crate::types::{Exclusion, ExclusionEnd, Held, ReservationRecord, Settings};

/// The records of `document`, checked, with the restart grace applied at `boot`: the
/// count matches, no request twice, every row valid. Each held deadline becomes the
/// later of the stored one and `boot` plus the boot grace (never the stored one plus
/// the grace, which a restart loop would push out for ever), and every record may be
/// claimed until `boot` plus the regain grace.
/// Fields this build does not know are returned apart, by request id, to be written
/// back. Any failure is an [`OpenError::Corrupt`] naming `path`.
pub(crate) fn restore_all(
    path: &Path,
    document: &Document,
    settings: &Settings,
    boot: Timestamp,
) -> Result<(Vec<ReservationRecord>, BTreeMap<String, toml::Table>), OpenError> {
    let counted = u64::try_from(document.reservation.len()).ok();
    if counted != Some(document.reservation_count) {
        let reason = format!(
            "the file counts {} reservations and holds {}",
            document.reservation_count,
            document.reservation.len()
        );
        return Err(store::corrupt(path, reason));
    }
    let claim_by = boot.plus(settings.regain_grace());
    let mut records = Vec::new();
    let mut kept = BTreeMap::new();
    for stored in &document.reservation {
        let record =
            restore(stored, settings, boot, claim_by).map_err(|r| store::corrupt(path, r))?;
        if kept
            .insert(stored.request_id.clone(), stored.extra.clone())
            .is_some()
        {
            let reason = format!("reservation {} is listed twice", stored.request_id);
            return Err(store::corrupt(path, reason));
        }
        records.push(record);
    }
    Ok((records, kept))
}

fn restore(
    stored: &StoredReservation,
    settings: &Settings,
    boot: Timestamp,
    claim_by: Timestamp,
) -> Result<ReservationRecord, String> {
    let held = match (&stored.host, stored.reserved_at_ms, stored.deadline_ms) {
        (None, None, None) => None,
        (Some(host), Some(reserved_at), Some(deadline)) => {
            let host = HostId::new(host).map_err(|err| err.to_string())?;
            Some(Held {
                host,
                reserved_at: Timestamp::from_millis(reserved_at),
                deadline: Timestamp::from_millis(deadline).max(boot.plus(settings.boot_grace())),
            })
        }
        _ => {
            let id = &stored.request_id;
            return Err(format!(
                "reservation {id} must name host, reserved_at_ms and deadline_ms together"
            ));
        }
    };
    let exclusions = stored.exclusion.iter().map(restore_exclusion);
    Ok(ReservationRecord {
        request_id: stored.request_id.clone(),
        held,
        give_ups: stored.give_ups,
        prepare_failures: stored.prepare_failures,
        exclusions: exclusions.collect::<Result<_, _>>()?,
        claim_by: Some(claim_by),
    })
}

fn restore_exclusion(stored: &StoredExclusion) -> Result<Exclusion, String> {
    let host = HostId::new(&stored.host).map_err(|err| err.to_string())?;
    let until = match (stored.until_ms, stored.forever) {
        (Some(ms), false) => ExclusionEnd::At(Timestamp::from_millis(ms)),
        (None, true) => ExclusionEnd::Forever,
        _ => {
            let reason = format!("exclusion of {host} needs exactly one of until_ms and forever");
            return Err(reason);
        }
    };
    Ok(Exclusion { host, until })
}

/// The row written for `record`, with the unknown fields kept for its request.
pub(crate) fn stored(
    record: &ReservationRecord,
    kept: &BTreeMap<String, toml::Table>,
) -> StoredReservation {
    let held = record.held.as_ref();
    StoredReservation {
        request_id: record.request_id.clone(),
        host: held.map(|held| held.host.as_str().to_owned()),
        reserved_at_ms: held.map(|held| held.reserved_at.millis()),
        deadline_ms: held.map(|held| held.deadline.millis()),
        give_ups: record.give_ups,
        prepare_failures: record.prepare_failures,
        exclusion: record.exclusions.iter().map(stored_exclusion).collect(),
        extra: kept.get(&record.request_id).cloned().unwrap_or_default(),
    }
}

fn stored_exclusion(exclusion: &Exclusion) -> StoredExclusion {
    let until = match exclusion.until {
        ExclusionEnd::At(at) => Some(at.millis()),
        ExclusionEnd::Forever => None,
    };
    StoredExclusion {
        host: exclusion.host.as_str().to_owned(),
        until_ms: until,
        forever: until.is_none(),
    }
}
