//! `life_minutes`: how many leading minutes at least half of a key's recent runs reached.

use super::*;

const MINUTE_US: u64 = 60_000_000;

/// A success that ran for `wall_us`.
fn lasting(wall_us: u64) -> Run {
    Run {
        wall_us,
        ..success(GIB)
    }
}

/// The `life_minutes` of a key after one success per entry of `walls` (microseconds).
fn life_after(walls: &[u64]) -> u8 {
    let k = linux(1);
    let mut s = Summaries::default();
    for wall in walls {
        s.observe(&k, &lasting(*wall), DAY, &Bounds::default());
    }
    s.get(&k).map_or(1, Summary::life_minutes)
}

/// A run that reached minute `m` (its wall time is a little over `m` minutes).
fn reaching(m: u64) -> u64 {
    m * MINUTE_US + 5_000_000
}

/// Catches: the count of leading minutes taken from the longest run (the mean, or a
/// minute only one run reached): runs reaching minutes 0, 3, 3 and 5 give 4, because
/// minutes 0 to 3 were reached by at least two of the four and minute 4 by one.
#[test]
fn life_is_the_leading_minutes_at_least_half_of_the_runs_reached() {
    assert_eq!(
        life_after(&[reaching(0), reaching(3), reaching(3), reaching(5)]),
        4
    );
    // Minute 1 is reached by 2 of 4: exactly half counts; minute 2 by 1 of 4 does not.
    assert_eq!(
        life_after(&[reaching(0), reaching(0), reaching(1), reaching(1)]),
        2
    );
    assert_eq!(
        life_after(&[reaching(0), reaching(0), reaching(0), reaching(1)]),
        1
    );
}

/// Catches: a key whose runs are all under a minute reporting more than 1 (their wall
/// time rounded up), and an empty key reporting 0.
#[test]
fn runs_under_a_minute_and_an_empty_ring_have_a_life_of_one() {
    assert_eq!(life_after(&[10_000_000, 59_999_999, 2_000_000]), 1);
    assert_eq!(life_after(&[]), 1);
    assert_eq!(Summary::new(Basis::Cgroup, DAY).life_minutes(), 1);
}

/// Catches: no upper clamp (a day-long key reporting hundreds of minutes) and the clamp
/// at the wrong end: the life never exceeds 30 and runs of 29 and 30 minutes are told
/// apart from runs of an hour.
#[test]
fn life_is_clamped_to_the_horizon() {
    assert_eq!(life_after(&[reaching(120); 6]), 30);
    assert_eq!(life_after(&[reaching(1_000); 6]), 30);
    assert_eq!(life_after(&[reaching(29); 6]), 30);
    assert_eq!(life_after(&[reaching(28); 6]), 29);
    assert_eq!(LIFE_HORIZON, 30);
}

/// Catches: a ring that keeps old runs for ever: after 32 short runs replace 32 long
/// ones, the life is back to 1, and a partly replaced ring counts the runs it holds.
#[test]
fn life_follows_the_last_32_runs() {
    let mut walls = vec![reaching(10); 32];
    assert_eq!(life_after(&walls), 11);
    walls.extend(vec![reaching(0); 15]);
    assert_eq!(
        life_after(&walls),
        11,
        "17 of 32 runs still reach minute 10"
    );
    walls.push(reaching(0));
    assert_eq!(life_after(&walls), 11, "16 of 32 is exactly half");
    walls.push(reaching(0));
    assert_eq!(life_after(&walls), 1, "15 of 32 is under half");
}

/// Catches: the life lost in storage: it round-trips, and a record stored before it was
/// kept (version 1, shorter) still decodes, as a key of runs under a minute.
#[test]
fn life_survives_storage_and_a_version_1_record_decodes() {
    let k = linux(1);
    let mut s = Summaries::default();
    for wall in [reaching(0), reaching(3), reaching(3), reaching(5)] {
        s.observe(&k, &lasting(wall), DAY, &Bounds::default());
    }
    let summary = s.get(&k).unwrap().clone();
    let decoded = Summary::decode(&summary.encode()).unwrap();
    assert_eq!(decoded.life_minutes(), 4);
    // A version 1 record: version byte 1, no minute ring, resealed checksum.
    let bytes = summary.encode();
    let mut v1 = bytes[..bytes.len() - 8 - 32].to_vec();
    v1[0] = 1;
    let sum = crate::codec::checksum(&v1);
    v1.extend_from_slice(&sum.to_le_bytes());
    let old = Summary::decode(&v1).expect("version 1 decodes");
    assert_eq!(old.samples(), summary.samples());
    assert_eq!(old.life_minutes(), 1);
    // A version byte that does not match the length is refused.
    let mut wrong = v1.clone();
    wrong[0] = 2;
    let sum = crate::codec::checksum(&wrong[..wrong.len() - 8]);
    let at = wrong.len() - 8;
    wrong[at..].copy_from_slice(&sum.to_le_bytes());
    assert_eq!(Summary::decode(&wrong), None);
}
