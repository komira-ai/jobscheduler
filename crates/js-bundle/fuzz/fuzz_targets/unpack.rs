//! Any bytes as a layer: the extractor must not panic, must create nothing outside its
//! directory, and must leave no symlink that resolves outside it when it succeeds.

#![no_main]

use std::os::fd::AsFd as _;
use std::sync::atomic::{AtomicU64, Ordering};

use js_bundle::{Limits, unpack_tar};
use libfuzzer_sys::fuzz_target;

static NEXT: AtomicU64 = AtomicU64::new(0);

fuzz_target!(|data: &[u8]| {
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("js-bundle-fuzz-{}-{n}", std::process::id()));
    let root = dir.join("root");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(dir.join("outside")).unwrap();
    let handle = std::fs::File::open(&root).unwrap();
    let limits = Limits {
        entries: 256,
        bytes: 1 << 20,
        model_bytes: 1 << 28,
    };
    let result = unpack_tar(data, handle.as_fd(), &limits);
    assert_eq!(std::fs::read_dir(dir.join("outside")).unwrap().count(), 0);
    if result.is_ok() {
        let canonical = root.canonicalize().unwrap();
        let mut stack = vec![root.clone()];
        while let Some(next) = stack.pop() {
            for entry in std::fs::read_dir(next).unwrap() {
                let path = entry.unwrap().path();
                let meta = std::fs::symlink_metadata(&path).unwrap();
                if meta.is_dir() {
                    stack.push(path);
                } else if meta.is_symlink()
                    && let Ok(resolved) = path.canonicalize()
                {
                    assert!(resolved.starts_with(&canonical));
                }
            }
        }
    }
    std::fs::remove_dir_all(&dir).unwrap();
});
