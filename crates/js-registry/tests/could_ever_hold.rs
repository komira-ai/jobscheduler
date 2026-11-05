//! Whether some host in the table could ever hold a request: the check a whole-node start
//! makes before it answers that no host is capable, asked on its own.

mod support;

use js_agent_wire::{Heartbeat, Taint};
use js_types::{Capabilities, Resources};
use support::*;

fn cores(count: u64) -> Resources {
    Resources::new(count * 1000, 1 << 30, 0, None).unwrap()
}

fn requiring(pairs: &[(&str, &str)]) -> Capabilities {
    map(pairs)
}

/// Asserts that an empty table holds nothing; that a registered host of the pool's shape
/// (4 cores, macOS) could hold a request that fits it, busy or not, and not one larger
/// than it or one that requires another operating system; that a tainted host could hold
/// nothing; and that a clean host beside it still could. It catches a check that counts
/// a tainted host, ignores the capabilities a request requires, or compares no size.
#[test]
fn a_host_could_hold_what_fits_its_shape_unless_tainted() {
    let fx = fixture(4);
    let none = requiring(&[]);
    assert!(!fx.registry.could_ever_hold(&cores(1), &none));
    let epoch = fx.join("a", "t");
    assert!(fx.registry.could_ever_hold(&cores(4), &none));
    assert!(
        fx.registry
            .could_ever_hold(&cores(1), &requiring(&[("os", "macos")]))
    );
    assert!(!fx.registry.could_ever_hold(&cores(5), &none));
    assert!(
        !fx.registry
            .could_ever_hold(&cores(1), &requiring(&[("os", "linux")]))
    );
    let report = Heartbeat {
        taint: Some(Taint {
            reason: "residue".to_owned(),
            run_id: None,
            check: None,
        }),
        ..fx.heartbeat("a", epoch)
    };
    fx.registry.heartbeat(&report, &cred("t")).unwrap();
    assert!(!fx.registry.could_ever_hold(&cores(1), &none));
    fx.join("b", "t");
    assert!(fx.registry.could_ever_hold(&cores(1), &none));
}
