//! One lease and its siblings.

use std::os::unix::fs::PermissionsExt as _;

use crate::cell::{Cell, Shape};

/// Catches a lease that can reach another's directory: a job reads, lists and opens, by
/// their paths on the host, a secret in a sibling lease's directory (world-readable on the
/// host, so only the container's own mount namespace stands in the way), the sibling's
/// upper layer, and the scratch directory that holds both. Each fails with "No such file
/// or directory", the secret is nowhere in the job's output, and the job's own mount table
/// (which does name its own lease's directories, so the check can see a name) names no
/// directory of the sibling.
#[tokio::test]
#[ignore = "real crun, root and a job user: run through cargo xtask crun-tests --job-user"]
async fn a_lease_cannot_read_a_sibling_leases_directory() {
    let cell = Cell::new("isolation");
    let sibling = cell.lease("sibling-two", &["/bin/true"], Shape::default());
    let secret = "TOPSECRET-of-the-sibling";
    for place in [&sibling.spec.dir, &sibling.spec.upper] {
        let file = place.join("secret");
        std::fs::write(&file, secret).expect("write a secret");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).expect("mode");
    }
    let (dir, upper, scratch) = (
        sibling.spec.dir.display(),
        sibling.spec.upper.display(),
        cell.scratch.display(),
    );
    let script = format!(
        "cat {dir}/secret 2>&1; echo cat=$?; cat {upper}/secret 2>&1; echo upper=$?; \
         ls {dir} 2>&1; echo ls=$?; ls {scratch} 2>&1; echo scratch=$?; \
         echo --; cat /proc/self/mountinfo"
    );
    let run = cell.sh("sibling-one", &script).await;
    let out = run.out();
    assert!(!out.contains("TOPSECRET"), "{}", run.said);
    for status in ["cat", "upper", "ls", "scratch"] {
        let line = out.lines().find(|l| l.starts_with(&format!("{status}=")));
        assert!(
            line.is_some_and(|l| l != format!("{status}=0")),
            "{status}: {line:?}: {}",
            run.said
        );
    }
    assert_eq!(
        out.matches("No such file or directory").count(),
        4,
        "{}",
        run.said
    );
    let table: String = out.lines().skip_while(|l| *l != "--").collect();
    assert!(table.contains("sibling-one"), "{}", run.said);
    assert!(!table.contains("sibling-two"), "{}", run.said);
}
