//! Random trees of directories, files and symlinks (targets from a small alphabet that
//! includes `..`) go through the extractor, and the kernel judges the answer: every symlink
//! of an admitted tree that resolves at all resolves inside the root.

mod layer;

use layer::{Seen, Tar, extract_with, limits, snapshot};

struct Rng(u64);

impl Rng {
    fn step(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.step() % bound as u64) as usize
    }
}

const NAMES: [&str; 4] = ["a", "b", "c", "l"];
const PARTS: [&str; 7] = ["a", "b", "c", "l", "m", "..", "."];

/// A random archive: entries in parent-first order, no path twice.
fn random_tar(rng: &mut Rng) -> Vec<u8> {
    let mut tar = Tar::new();
    let mut dirs = vec![String::new()];
    let mut taken: Vec<String> = Vec::new();
    for _ in 0..4 + rng.below(10) {
        let parent = dirs[rng.below(dirs.len())].clone();
        let name = NAMES[rng.below(NAMES.len())];
        let path = if parent.is_empty() {
            name.to_string()
        } else {
            format!("{parent}/{name}")
        };
        if taken.contains(&path) || path.matches('/').count() > 3 {
            continue;
        }
        taken.push(path.clone());
        match rng.below(3) {
            0 => {
                tar = tar.dir(&path);
                dirs.push(path);
            }
            1 => tar = tar.file(&path, b"x"),
            _ => {
                let target: Vec<&str> = (0..1 + rng.below(4))
                    .map(|_| PARTS[rng.below(PARTS.len())])
                    .collect();
                tar = tar.symlink(&path, &target.join("/"));
            }
        }
    }
    tar.end()
}

/// Whatever the kernel resolves of an admitted tree is inside it; the walker admits some
/// trees with `..` links and refuses others.
#[test]
fn every_admitted_symlink_resolves_inside_the_root() {
    let mut rng = Rng(0x2545_F491_4F6C_DD1D);
    let (mut admitted, mut refused, mut climbing) = (0, 0, 0);
    for _ in 0..400 {
        let run = extract_with(&random_tar(&mut rng), &limits());
        if run.result.is_err() {
            refused += 1;
            continue;
        }
        admitted += 1;
        let root = run.root().canonicalize().unwrap();
        for (path, seen) in snapshot(&run.root()) {
            let Seen::Link(target) = seen else { continue };
            climbing += usize::from(target.contains(".."));
            if let Ok(resolved) = run.root().join(&path).canonicalize() {
                assert!(
                    resolved.starts_with(&root),
                    "{path} -> {target}: {resolved:?}"
                );
            }
        }
        run.scratch.assert_contained();
    }
    assert!(
        admitted > 20 && refused > 20 && climbing > 2,
        "{admitted} {refused} {climbing}"
    );
}
