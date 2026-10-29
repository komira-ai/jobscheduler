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


fn probe(n: usize) -> String {
    let mut rng = Rng(0x2545_F491_4F6C_DD1D);
    let (mut admitted, mut refused, mut climbing, mut resolving, mut first_escape) = (0, 0, 0, 0, None);
    for i in 0..n {
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
                resolving += usize::from(target.contains(".."));
                if !resolved.starts_with(&root) && first_escape.is_none() {
                    first_escape = Some(i);
                }
            }
        }
    }
    format!("n={n} admitted={admitted} refused={refused} climbing={climbing} resolving_dotdot={resolving} first_escape={first_escape:?}")
}

#[test]
fn probe_400() {
    panic!("PROBE {}", probe(400));
}

#[test]
fn probe_5000() {
    panic!("PROBE {}", probe(5000));
}

#[test]
fn probe_links_100_admitted() {
    let target = format!("{}a", "a/../".repeat(190));
    let mut tar = Tar::new().dir("a");
    for i in 0..100 {
        tar = tar.long_link(target.as_bytes()).symlink(&format!("l{i:03}"), &target);
    }
    let l = js_bundle::Limits { entries: 1000, bytes: 4 << 20, model_bytes: 1 << 28 };
    let run = extract_with(&tar.end(), &l);
    panic!("PROBE100 {:?}", run.result.as_ref().map(|_| ()).map_err(|e| e.to_string()));
}
// retrigger
