//! The rule that judges a launched machine's hello against the offer it was acquired for
//! (design 5.6.1, step 6), on its own: accelerator model by canonical name, accelerator
//! count at least the offer's, the same architecture, and CPU, memory and disk within the
//! size tolerance, computed in integers.

mod common;

use common::*;
use js_elastic::Offer;
use js_scaler::{HelloDimension, StoredOffer, Verdict, verify_hello};

use HelloDimension::{AcceleratorCount, AcceleratorModel, Architecture, Cpu, Disk, Memory};

/// An offer of `count` accelerators spelt `class`, of 10 cores, 100 GiB of memory and
/// 1000 GiB of disk: sizes whose 90 percent is a whole number.
fn sized(class: &str, count: u32) -> Offer {
    Offer {
        cpu_millis: 10_000,
        memory_bytes: 100 << 30,
        disk_bytes: 1000 << 30,
        accelerator: accel(class, count),
        ..offer("gpu_8x", 60)
    }
}

/// The hello of a host of exactly the shape of `offer`.
fn hello_of(offer: &Offer) -> Offer {
    Offer {
        price_cents_per_hour: None,
        ..offer.clone()
    }
}

fn judge(offer: &Offer, hello: &Offer, tolerance: u8) -> Verdict {
    verify_hello(&StoredOffer::from_offer(offer), hello, tolerance)
}

fn mismatch(failed: &[HelloDimension]) -> Verdict {
    Verdict::Mismatch(failed.to_vec())
}

/// Asserts that a hello with fewer accelerators than its offer mismatches in the count and
/// nothing else, that one with more matches, and that the count takes no tolerance (seven of
/// eight is short even at the largest tolerance the file allows). Catches a comparison that
/// is equality, one that is reversed, and a tolerance applied to the count.
#[test]
fn fewer_accelerators_is_a_mismatch_and_more_is_a_match() {
    let offer = sized("nvidia-h100", 8);
    let with = |count| Offer {
        accelerator: accel("nvidia-h100", count),
        ..hello_of(&offer)
    };
    assert_eq!(judge(&offer, &with(7), 10), mismatch(&[AcceleratorCount]));
    assert_eq!(judge(&offer, &with(7), 50), mismatch(&[AcceleratorCount]));
    assert_eq!(judge(&offer, &with(8), 10), Verdict::Match);
    assert_eq!(judge(&offer, &with(9), 0), Verdict::Match);
}

/// Asserts the tolerance at its edge: at 10 percent, CPU, memory and disk at exactly 90
/// percent of the offer match, and one unit below that mismatches in that dimension alone;
/// at 0 percent the same hello mismatches in all three; an offer of `u64::MAX` in every size
/// is met by a hello of `u64::MAX` and, at 10 percent, by one a unit below it, without
/// overflowing. Catches an off-by-one at the boundary, a rounding of a fraction, and an
/// overflow near the maximum.
#[test]
fn a_size_within_tolerance_matches() {
    let offer = sized("nvidia-h100", 8);
    let edge = Offer {
        cpu_millis: 9_000,
        memory_bytes: 90 << 30,
        disk_bytes: 900 << 30,
        ..hello_of(&offer)
    };
    assert_eq!(judge(&offer, &edge, 10), Verdict::Match);
    let below: [(fn(&mut Offer), HelloDimension); 3] = [
        (|hello| hello.cpu_millis -= 1, Cpu),
        (|hello| hello.memory_bytes -= 1, Memory),
        (|hello| hello.disk_bytes -= 1, Disk),
    ];
    for (lower, dimension) in below {
        let mut short = edge.clone();
        lower(&mut short);
        assert_eq!(judge(&offer, &short, 10), mismatch(&[dimension]));
    }
    assert_eq!(judge(&offer, &edge, 0), mismatch(&[Cpu, Memory, Disk]));
    assert_eq!(judge(&offer, &hello_of(&offer), 0), Verdict::Match);

    let most = Offer {
        cpu_millis: u64::MAX,
        memory_bytes: u64::MAX,
        disk_bytes: u64::MAX,
        ..offer.clone()
    };
    assert_eq!(judge(&most, &hello_of(&most), 0), Verdict::Match);
    assert_eq!(judge(&most, &hello_of(&most), 10), Verdict::Match);
    let under = Offer {
        cpu_millis: u64::MAX - 1,
        ..hello_of(&most)
    };
    assert_eq!(judge(&most, &under, 10), Verdict::Match);
    assert_eq!(judge(&most, &under, 0), mismatch(&[Cpu]));
}

/// Asserts that the accelerator model is compared by canonical name after alias
/// resolution: an offer that names a variant alias is met by a hello whose model is the
/// canonical family (a text that differs from the offer's), and an offer that names the
/// family by any of its variants; a hello of another model with the same count mismatches
/// in the model alone. Asserts the architecture in its one spelling: `aarch64` against
/// `arm64` either way round matches, `x86_64` against `arm64` mismatches in the
/// architecture alone. Catches raw strings compared, and an architecture compared before
/// it is normalised.
#[test]
fn the_accelerator_model_is_compared_by_canonical_name() {
    let alias = "nvidia-h100-80gb-hbm3";
    let offer = sized(alias, 8);
    let hello = hello_of(&offer);
    let reported = hello.accelerator.as_ref().unwrap();
    assert_eq!(reported.model.as_str(), "nvidia-h100");
    assert_ne!(
        StoredOffer::from_offer(&offer).gpu_class.as_deref(),
        Some("nvidia-h100")
    );
    assert_eq!(judge(&offer, &hello, 10), Verdict::Match);

    let family = sized("nvidia-h100", 8);
    assert_eq!(judge(&family, &hello, 10), Verdict::Match);

    let other = Offer {
        accelerator: accel("nvidia-a100-sxm4-80gb", 8),
        ..hello.clone()
    };
    assert_eq!(judge(&offer, &other, 10), mismatch(&[AcceleratorModel]));
    assert_eq!(judge(&family, &other, 10), mismatch(&[AcceleratorModel]));

    let arm = |arch: &str| Offer {
        arch: arch.to_owned(),
        ..sized(alias, 8)
    };
    assert_eq!(judge(&arm("arm64"), &arm("aarch64"), 10), Verdict::Match);
    assert_eq!(judge(&arm("aarch64"), &arm("arm64"), 10), Verdict::Match);
    assert_eq!(
        judge(&arm("arm64"), &arm("x86_64"), 10),
        mismatch(&[Architecture])
    );
}

/// Asserts that an offer with no accelerator accepts a hello with none, with any count and
/// with any model, and that an offer with accelerators is not met by a hello with none,
/// which fails in the model and in the count. Catches an offer without accelerators that
/// refuses a host that has some, and a host without any taken for one of the offer's model.
#[test]
fn an_offer_with_no_accelerator_accepts_any_accelerator() {
    let bare = Offer {
        accelerator: None,
        ..sized("nvidia-h100", 8)
    };
    for accelerator in [None, accel("nvidia-h100", 8), accel("nvidia-a10", 1)] {
        let hello = Offer {
            accelerator,
            ..hello_of(&bare)
        };
        assert_eq!(judge(&bare, &hello, 10), Verdict::Match);
    }
    let offer = sized("nvidia-h100", 8);
    let none = Offer {
        accelerator: None,
        ..hello_of(&offer)
    };
    assert_eq!(
        judge(&offer, &none, 10),
        mismatch(&[AcceleratorModel, AcceleratorCount])
    );
}

/// Asserts that a hello that states no architecture (an empty one) and no disk (zero, what
/// a host that reports none offers) is not judged on either, while one that states them is;
/// and that a hello short in every dimension names all six, in their order. Catches a
/// healthy machine released because its agent reports no disk, and a verdict that names
/// only the first dimension that failed.
#[test]
fn a_dimension_the_hello_does_not_state_is_not_judged() {
    let offer = sized("nvidia-h100", 8);
    let silent = Offer {
        arch: String::new(),
        disk_bytes: 0,
        ..hello_of(&offer)
    };
    assert_eq!(judge(&offer, &silent, 10), Verdict::Match);
    let worst = Offer {
        accelerator: accel("nvidia-a10", 1),
        arch: "arm64".to_owned(),
        cpu_millis: 1,
        memory_bytes: 1,
        disk_bytes: 1,
        ..hello_of(&offer)
    };
    assert_eq!(
        judge(&offer, &worst, 10),
        mismatch(&[
            AcceleratorModel,
            AcceleratorCount,
            Architecture,
            Cpu,
            Memory,
            Disk
        ])
    );
}
