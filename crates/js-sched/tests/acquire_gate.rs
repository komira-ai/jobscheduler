//! The gate before pass 2 (`js_sched::acquire::needs_acquire`): a job is acquired a machine
//! only when no free capable host and no fitting machine on its way can serve it.

use js_sched::acquire::needs_acquire;

/// Asserts the whole table of the gate: nothing free and nothing on its way runs pass 2; one
/// free capable host, of whichever provider, or one fitting machine on its way, holds it
/// back, and so do several of either and both at once. It also holds at the largest counts,
/// where a sum that wrapped would read as none. Catches a gate that counts only one of the
/// two, one that needs both before it holds back, and one that runs pass 2 beside a host.
#[test]
fn pass_two_runs_only_when_nothing_free_or_on_its_way_can_serve() {
    let cases = [
        ((0, 0), true),
        ((1, 0), false),
        ((0, 1), false),
        ((1, 1), false),
        ((3, 0), false),
        ((0, 2), false),
        ((usize::MAX, usize::MAX), false),
        ((usize::MAX, 0), false),
    ];
    for ((free, booting), runs) in cases {
        assert_eq!(
            needs_acquire(free, booting),
            runs,
            "free {free}, booting {booting}"
        );
    }
}
