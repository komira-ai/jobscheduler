//! Probe cases for the layer decoder.

use std::io::Read;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::Instant;

use js_bundle::{Decoder, compress};

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

fn sample(rng: &mut Rng, size: usize) -> Vec<u8> {
    let words: [&[u8]; 8] = [b"fn main", b"let x = ", b"return ", b"\x00\x01\x02\x03", b"hello world ", b"/usr/lib/", b"0123456789", b"\n\n"];
    let mut out = Vec::new();
    while out.len() < size {
        if rng.below(7) == 0 {
            out.push(rng.step() as u8);
        } else {
            out.extend_from_slice(words[rng.below(words.len())]);
        }
    }
    out
}

#[test]
fn damaged_frames() {
    let mut rng = Rng(0xA5A5_1234_9876_5432);
    let mut panics = 0u32;
    let mut ok = 0u32;
    let mut err = 0u32;
    let mut worst = std::time::Duration::ZERO;
    let mut worst_out = 0usize;
    let mut first_panic = String::new();
    std::panic::set_hook(Box::new(|_| {}));
    for round in 0..40 {
        let tar = sample(&mut rng, 20_000 + round * 5_000);
        let blob = compress(&tar);
        for _ in 0..1500 {
            let mut b = blob.clone();
            match rng.below(4) {
                0 | 1 => {
                    for _ in 0..1 + rng.below(3) {
                        let span = if rng.below(2) == 0 { 64.min(b.len()) } else { b.len() };
                        let at = rng.below(span);
                        b[at] ^= 1 << rng.below(8);
                    }
                }
                2 => {
                    let at = rng.below(b.len());
                    b[at] = rng.step() as u8;
                }
                _ => b.truncate(1 + rng.below(b.len())),
            }
            let t = Instant::now();
            let r = catch_unwind(AssertUnwindSafe(|| {
                let mut src = &b[..];
                let mut dec = Decoder::new(&mut src)?;
                let mut out = Vec::new();
                let mut buf = [0u8; 4096];
                loop {
                    let n = dec.read(&mut buf).map_err(|e| js_bundle::StreamError::Read(e.to_string()))?;
                    if n == 0 {
                        break;
                    }
                    out.extend_from_slice(&buf[..n]);
                    if out.len() > (64 << 20) {
                        break;
                    }
                }
                Ok::<usize, js_bundle::StreamError>(out.len())
            }));
            let el = t.elapsed();
            if el > worst {
                worst = el;
            }
            match r {
                Err(p) => {
                    panics += 1;
                    if first_panic.is_empty() {
                        first_panic = p.downcast_ref::<String>().cloned().or_else(|| p.downcast_ref::<&str>().map(|s| (*s).to_owned())).unwrap_or_default();
                    }
                }
                Ok(Ok(n)) => {
                    ok += 1;
                    worst_out = worst_out.max(n);
                }
                Ok(Err(_)) => err += 1,
            }
        }
    }
    let _ = std::panic::take_hook();
    panic!("REPORT panics={panics} ok={ok} err={err} worst={worst:?} worst_out={worst_out} first_panic={first_panic}");
}
