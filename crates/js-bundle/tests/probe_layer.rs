//! Probe cases for the layer extractor.

mod layer;

use std::fmt::Write as _;
use std::time::Instant;

use js_bundle::{Limits, TreeRefusal, UnpackError};
use layer::{Entry, Seen, Tar, extract, extract_with, snapshot};

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

fn patched(e: &Entry<'_>, f: impl FnOnce(&mut [u8; 512])) -> Vec<u8> {
    let mut h = layer::header(e);
    f(&mut h);
    h[148..156].fill(b' ');
    let sum: u32 = h.iter().map(|b| u32::from(*b)).sum();
    h[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
    let mut out = h.to_vec();
    out.resize(out.len() + 1024, 0);
    out
}

#[test]
fn reserved_names_in_other_case() {
    let mut accepted = Vec::new();
    for name in [".FSEVENTSD", ".spotlight-v100", ".TRASHES", ".Metadata_Never_Index"] {
        if extract(&Tar::new().dir(name).end()).result.is_ok() {
            accepted.push(name);
        }
    }
    assert!(accepted.is_empty(), "accepted at the root: {accepted:?}");
}

#[test]
fn kernel_differential() {
    let comps = ["a", "b", "c", "l", "m", "n", "..", "..", "..", "."];
    let names = ["a", "b", "c", "l", "m", "n"];
    let mut rng = Rng(0x1234_5678_9ABC_DEF1);
    let (mut total, mut accepted, mut dotdot_ok, mut refused_link) = (0u32, 0u32, 0u32, 0u32);
    let mut other = String::new();
    for _ in 0..30000 {
        total += 1;
        let mut dirs = vec![String::new()];
        let mut used: Vec<String> = Vec::new();
        let mut tar = Tar::new();
        for _ in 0..3 + rng.below(8) {
            let parent = dirs[rng.below(dirs.len())].clone();
            let name = names[rng.below(names.len())];
            let path = if parent.is_empty() { name.to_string() } else { format!("{parent}/{name}") };
            if used.contains(&path) {
                continue;
            }
            used.push(path.clone());
            match rng.below(10) {
                0..=3 => {
                    tar = tar.dir(&path);
                    dirs.push(path);
                }
                4 => tar = tar.file(&path, b"x"),
                _ => {
                    let n = 1 + rng.below(5);
                    let target: Vec<&str> = (0..n).map(|_| comps[rng.below(comps.len())]).collect();
                    tar = tar.symlink(&path, &target.join("/"));
                }
            }
        }
        let run = extract(&tar.end());
        match &run.result {
            Ok(_) => {
                accepted += 1;
                let root = run.root();
                let canon = root.canonicalize().unwrap();
                for (path, seen) in snapshot(&root) {
                    if let Seen::Link(target) = seen {
                        if let Ok(resolved) = root.join(&path).canonicalize() {
                            assert!(resolved.starts_with(&canon), "ESCAPE {path} -> {target} = {resolved:?}");
                            if target.contains("..") {
                                dotdot_ok += 1;
                            }
                        }
                    }
                }
            }
            Err(UnpackError::Tree(TreeRefusal::Link { .. })) => refused_link += 1,
            Err(e) => {
                if other.len() < 600 {
                    let _ = write!(other, "{e}; ");
                }
            }
        }
        run.scratch.assert_contained();
    }
    panic!("REPORT total={total} accepted={accepted} dotdot_resolved_inside={dotdot_ok} refused_link={refused_link} other={other}");
}

#[test]
fn size_field_encodings() {
    let mut report = String::new();
    let cases: Vec<(&str, [u8; 12])> = vec![
        ("ff*12", [0xFF; 12]),
        ("ff,0*11", { let mut f = [0u8; 12]; f[0] = 0xFF; f }),
        ("80,max", { let mut f = [0xFFu8; 12]; f[0] = 0x80; f }),
        ("80,2^63", { let mut f = [0u8; 12]; f[0] = 0x80; f[4] = 0x80; f }),
        ("80,zero", { let mut f = [0u8; 12]; f[0] = 0x80; f }),
        ("80,high-bytes-set-low-zero", { let mut f = [0u8; 12]; f[0] = 0x80; f[1] = 1; f }),
    ];
    for (label, field) in cases {
        for flag in [b'0', b'5', b'L'] {
            let e = Entry::new(b"f", flag);
            let tar = patched(&e, |h| h[124..136].copy_from_slice(&field));
            let run = extract(&tar);
            let _ = write!(report, "[{label} {}] {:?} | ", flag as char, run.result.as_ref().map(|u| u.file_bytes).map_err(|e| e.to_string()));
            run.scratch.assert_contained();
        }
    }
    panic!("REPORT {report}");
}

#[test]
fn ustar_prefix_and_slash() {
    let mut report = String::new();
    let e = Entry::new(b"x", b'0');
    let tar = patched(&e, |h| h[345..355].copy_from_slice(b"../outside"));
    let run = extract(&tar);
    let _ = write!(report, "prefix-dotdot: {:?} | ", run.result.as_ref().map(|_| ()).map_err(|e| e.to_string()));
    run.scratch.assert_contained();
    let run = extract(&Tar::new().entry(Entry::new(b"d/", b'5')).end());
    let _ = write!(report, "dir-trailing-slash: {:?} | ", run.result.as_ref().map(|_| ()).map_err(|e| e.to_string()));
    let tar = patched(&e, |h| { h[257..265].copy_from_slice(b"ustar  \0"); h[345..356].copy_from_slice(b"../outside/"); });
    let run = extract(&tar);
    let _ = write!(report, "gnu-magic-garbage-prefix: {:?} | ", run.result.as_ref().map(|u| u.entries).map_err(|e| e.to_string()));
    run.scratch.assert_contained();
    panic!("REPORT {report}");
}

#[test]
fn walker_cost() {
    let pattern = "a/../".repeat(190);
    let n = 30000usize;
    let mut long = Tar::new().dir("a").file("f", b"x");
    let mut short = Tar::new().dir("a").file("f", b"x");
    for i in 0..n {
        let next = if i % 8 == 7 || i + 1 == n { "f".to_string() } else { format!("l{}", i + 1) };
        let target = format!("{pattern}{next}");
        long = long.long_link(target.as_bytes()).symlink(&format!("l{i}"), &target);
        short = short.symlink(&format!("l{i}"), "f");
    }
    let limits = Limits { entries: 1 << 20, bytes: 1 << 30 };
    let t = Instant::now();
    let r = extract_with(&short.end(), &limits);
    let short_t = t.elapsed();
    let short_ok = r.result.is_ok();
    drop(r);
    let t = Instant::now();
    let r = extract_with(&long.end(), &limits);
    let long_t = t.elapsed();
    panic!("REPORT n={n} short={short_t:?} ok={short_ok} long={long_t:?} result={:?}", r.result.as_ref().map(|u| u.entries).map_err(|e| e.to_string()));
}
