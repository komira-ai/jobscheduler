#[test]
#[allow(clippy::assertions_on_constants)]
#[ignore = "real crun: run through cargo xtask crun-tests"]
fn planted_failure_for_the_ci_gate_proof() {
    assert!(false, "planted failure for the CI gate proof");
}
