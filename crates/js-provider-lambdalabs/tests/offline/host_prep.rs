//! The steps that prepare Lambda's stock image: against files, at the size bounds and at
//! the edge of the disk.

use js_elastic::{BootStep, ElasticProvider};
use js_provider_lambdalabs::{
    ConfigError, DEFAULT_SCRATCH_GIB, DISK_RESERVE_GIB, MAX_SCRATCH_GIB, MIN_SCRATCH_GIB,
};

use crate::rig::{candidate, live, rig};

const DEFAULT: &str = include_str!("../fixtures/host_prep_default.txt");
const SMALL_DISK: &str = include_str!("../fixtures/host_prep_small_disk.txt");

/// The steps, one argument list per line. Every step must be a program run: a step of any
/// other kind would be a secret or a download, and these have neither.
fn shown(steps: &[BootStep]) -> String {
    steps
        .iter()
        .map(|step| match step {
            BootStep::Run(argv) => format!("{argv:?}\n"),
            other => panic!("not a run step: {other:?}"),
        })
        .collect()
}

fn steps(config: js_provider_lambdalabs::ElasticConfig, disk_gib: u64) -> Vec<BootStep> {
    let mut offer = candidate("region-a");
    offer.disk_bytes = disk_gib << 30;
    rig(config).provider.host_prep(&offer)
}

/// Asserts the whole list for the default size on a roomy disk: the driver and container
/// checks first, then the image made, formatted and mounted on the loop device with
/// `nodev,nosuid,noatime`, then the mount root closed to others. A change is a change to
/// this file.
#[test]
fn the_default_steps_match_their_golden_file() {
    assert_eq!(DEFAULT_SCRATCH_GIB, 200);
    assert_eq!(shown(&steps(live(5), 512)), DEFAULT);
}

/// Asserts that the configured size reaches the allocation and nothing else changes.
#[test]
fn the_configured_size_is_the_only_difference() {
    let config = live(5).scratch_gib(300).unwrap();
    assert_eq!(shown(&steps(config, 512)), DEFAULT.replace("200G", "300G"));
}

/// Asserts the disk edge: a disk of exactly the scratch size plus the reserve gets the
/// full steps, one GiB less gets the logged failure, and the failure is the golden file.
#[test]
fn a_disk_that_cannot_hold_the_image_and_the_reserve_gets_a_logged_failure() {
    let needed = u64::from(DEFAULT_SCRATCH_GIB) + u64::from(DISK_RESERVE_GIB);
    assert_eq!(steps(live(5), needed).len(), 8);
    assert_eq!(steps(live(5), needed - 1).len(), 2);
    assert_eq!(shown(&steps(live(5), 240)), SMALL_DISK);
    // A byte short of the edge is a GiB short: sizes round down.
    let mut offer = candidate("region-a");
    offer.disk_bytes = (needed << 30) - 1;
    assert_eq!(rig(live(5)).provider.host_prep(&offer).len(), 2);
}

/// Asserts the size bounds of the configuration, at and beside each edge.
#[test]
fn the_scratch_size_is_bounded() {
    for gib in [MIN_SCRATCH_GIB, 200, MAX_SCRATCH_GIB] {
        assert!(live(5).scratch_gib(gib).is_ok(), "{gib}");
    }
    for gib in [0, MIN_SCRATCH_GIB - 1, MAX_SCRATCH_GIB + 1, u32::MAX] {
        assert_eq!(
            live(5).scratch_gib(gib).err(),
            Some(ConfigError::ScratchSize { gib }),
            "{gib}"
        );
    }
}

/// Asserts that a region list is codes: not empty, lower case letters, digits and `-`.
#[test]
fn a_region_list_is_region_codes() {
    assert!(live(5).regions(&["us-west-1", "region-a"]).is_ok());
    let long = "r".repeat(65);
    let cases: [&[&str]; 6] = [
        &[],
        &[""],
        &["US-West"],
        &["a b"],
        &["ok", "bad_one"],
        &[long.as_str()],
    ];
    for bad in cases {
        assert_eq!(
            live(5).regions(bad).err(),
            Some(ConfigError::Region),
            "{bad:?}"
        );
    }
}

/// Asserts that each configuration error names the rule it broke.
#[test]
fn a_configuration_error_names_its_rule() {
    assert_eq!(
        ConfigError::ScratchSize { gib: 3 }.to_string(),
        "the scratch size must be 10 to 4096 GiB, not 3"
    );
    assert_eq!(
        ConfigError::Region.to_string(),
        "a region is 1 to 64 lower case letters, digits or '-'"
    );
}
