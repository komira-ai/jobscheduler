//! [`fetch_cached`] and [`first_pinned`] against a fake transport: what a cache hit, a cache miss
//! and every way either can fail install, keep, download and wait for. Pauses are recorded, not
//! slept, and nothing is timed. `tests/fetch.rs` runs the same verb over real HTTPS.

use std::cell::RefCell;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::tests::{TOOL, listing, scratch, tar, tar_xz, toolchain, tree, xz};
use super::*;

/// The first URL tried: a community mirror.
const ONE: &str = "https://mirror.example/zig/zig-x86_64-linux-1.0.tar.xz";
/// The second: the project's own download host.
const TWO: &str = "https://origin.example/download/1.0/zig-x86_64-linux-1.0.tar.xz";

/// A fake transport: answers each URL from `answers` (the first entry for that URL; a refused
/// connection for any other), and records every URL asked for and every pause.
struct Transport {
    answers: Vec<(String, Result<Vec<u8>, String>)>,
    asked: RefCell<Vec<String>>,
    paused: RefCell<Vec<Duration>>,
}

impl Transport {
    fn new(answers: &[(&str, Result<&[u8], String>)]) -> Transport {
        Transport {
            answers: answers
                .iter()
                .map(|(url, answer)| {
                    let answer = answer.clone().map(<[u8]>::to_vec);
                    ((*url).to_owned(), answer)
                })
                .collect(),
            asked: RefCell::new(Vec::new()),
            paused: RefCell::new(Vec::new()),
        }
    }

    fn get(&self, url: &str) -> Result<Vec<u8>, String> {
        self.asked.borrow_mut().push(url.to_owned());
        self.answers.iter().find(|(u, _)| u == url).map_or(
            Err(format!("download {url}: connection refused")),
            |(_, a)| a.clone(),
        )
    }

    /// [`fetch_cached`] of `ONE` then `TWO`, pinned to `sha256`, unpacked as `unpack`, into
    /// `out`, through `cache`.
    fn fetch_pinned(
        &self,
        sha256: &Sha256,
        unpack: &Unpack,
        out: &Path,
        cache: &Path,
    ) -> Result<String, Error> {
        let urls = [ONE.to_owned(), TWO.to_owned()];
        let req = Cached {
            urls: &urls,
            sha256,
            unpack,
            out,
            cache,
        };
        fetch_cached(&req, &|url| self.get(url), &|d| {
            self.paused.borrow_mut().push(d)
        })
    }

    /// [`Transport::fetch_pinned`] of `TOOL`, as is.
    fn fetch(&self, out: &Path, cache: &Path) -> Result<String, Error> {
        self.fetch_pinned(&Sha256::of(TOOL), &Unpack::Raw, out, cache)
    }

    fn asked(&self) -> Vec<String> {
        self.asked.borrow().clone()
    }

    fn paused(&self) -> Vec<Duration> {
        self.paused.borrow().clone()
    }
}

/// `out` and `cache` paths in a scratch directory of its own named `name`.
fn paths(name: &str) -> (PathBuf, PathBuf) {
    let dir = scratch(&format!("cache-{name}"));
    (
        dir.join("bin").join("zig"),
        dir.join("pinned").join("zig.tar.xz"),
    )
}

/// `cache` written with `bytes`, its directory created.
fn seed(cache: &Path, bytes: &[u8]) {
    std::fs::create_dir_all(cache.parent().expect("parent")).expect("mkdir");
    std::fs::write(cache, bytes).expect("seed the cache");
}

fn file_mode(path: &Path) -> u32 {
    std::fs::metadata(path).expect("stat").permissions().mode() & 0o777
}

/// Asserts the failure the CI runs failed on: the first URL's transfer gives up (`timeout:
/// global`, as ureq says it), the second URL is asked next in the same round with no pause, and
/// its pinned bytes are kept (0644, no part file) and installed (0755); the line names the
/// timed-out attempt.
#[test]
fn a_first_url_that_times_out_gives_way_to_the_next_in_the_same_round() {
    let (out, cache) = paths("timeout");
    let net = Transport::new(&[
        (ONE, Err(format!("download {ONE}: timeout: global"))),
        (TWO, Ok(TOOL)),
    ]);
    let line = net.fetch(&out, &cache).expect("the second URL");
    assert_eq!(
        line,
        format!(
            "ok: {TWO} has SHA-256 {}, after 1 failed: {ONE}: download {ONE}: timeout: global; \
             kept it at {} and installed {}",
            Sha256::of(TOOL),
            cache.display(),
            out.display()
        )
    );
    assert_eq!(net.asked(), [ONE, TWO]);
    assert_eq!(net.paused(), [Duration::ZERO]);
    assert_eq!(std::fs::read(&cache).expect("cache"), TOOL);
    assert_eq!(file_mode(&cache), 0o644);
    assert!(!part_path(&cache).exists());
    assert_eq!(std::fs::read(&out).expect("out"), TOOL);
    assert_eq!(file_mode(&out), 0o755);
}

/// Asserts that a download whose SHA-256 is not the pin is refused as one failed attempt, named
/// with its digest, and never kept or installed: the next URL's pinned bytes are.
#[test]
fn a_mirror_serving_other_bytes_is_refused_and_the_next_url_is_kept() {
    let (out, cache) = paths("mirror-mismatch");
    let wrong: &[u8] = b"a tarball of another version";
    let net = Transport::new(&[(ONE, Ok(wrong)), (TWO, Ok(TOOL))]);
    let line = net.fetch(&out, &cache).expect("the second URL");
    assert_eq!(
        line,
        format!(
            "ok: {TWO} has SHA-256 {}, after 1 failed: {ONE}: SHA-256 {}, not the pinned one; \
             kept it at {} and installed {}",
            Sha256::of(TOOL),
            Sha256::of(wrong),
            cache.display(),
            out.display(),
        )
    );
    assert_eq!(net.asked(), [ONE, TWO]);
    assert_eq!(std::fs::read(&cache).expect("cache"), TOOL);
    assert_eq!(std::fs::read(&out).expect("out"), TOOL);
}

/// Asserts that a URL that errors (its multi-line error flattened to one line) and one serving
/// other bytes are passed over round after round, with exactly the pauses of `WAITS`, until the
/// pinned bytes arrive in the last round; the line lists the 5 failed attempts in order.
#[test]
fn a_miss_tries_each_url_in_turn_and_waits_between_rounds() {
    let (out, cache) = paths("rounds");
    let wrong: &[u8] = b"a truncated tarball";
    let served = RefCell::new(0);
    let asked = RefCell::new(Vec::new());
    let paused = RefCell::new(Vec::new());
    let get = |url: &str| {
        asked.borrow_mut().push(url.to_owned());
        if url == ONE {
            return Err(format!("download {ONE}: timeout: global\r\nafter 300 s"));
        }
        *served.borrow_mut() += 1;
        Ok(if *served.borrow() < 3 { wrong } else { TOOL }.to_vec())
    };
    let urls = [ONE.to_owned(), TWO.to_owned()];
    let req = Cached {
        urls: &urls,
        sha256: &Sha256::of(TOOL),
        unpack: &Unpack::Raw,
        out: &out,
        cache: &cache,
    };
    let line = fetch_cached(&req, &get, &|d| paused.borrow_mut().push(d)).expect("third round");
    let timed_out = format!("{ONE}: download {ONE}: timeout: global  after 300 s");
    let other = format!("{TWO}: SHA-256 {}, not the pinned one", Sha256::of(wrong));
    let failed = [&timed_out, &other, &timed_out, &other, &timed_out].map(String::as_str);
    assert_eq!(
        line,
        format!(
            "ok: {TWO} has SHA-256 {}, after 5 failed: {}; kept it at {} and installed {}",
            Sha256::of(TOOL),
            failed.join("; "),
            cache.display(),
            out.display()
        )
    );
    assert_eq!(*asked.borrow(), [ONE, TWO, ONE, TWO, ONE, TWO]);
    assert_eq!(*paused.borrow(), WAITS);
    assert_eq!(std::fs::read(&cache).expect("cache"), TOOL);
}

/// Asserts that when no URL gives the pinned bytes in any round (a refused connection, and a
/// mirror serving bytes of another SHA-256 every time) the error is one line listing every
/// attempt in order, by exact equality, after exactly the pauses of `WAITS` (0 s, 10 s, 30 s);
/// neither the cache file nor `out` is written. Pins `ATTEMPT_TIMEOUT` (300 s).
#[test]
fn a_miss_no_url_serves_is_refused_with_each_attempt_and_writes_nothing() {
    let (out, cache) = paths("unavailable");
    let net = Transport::new(&[(TWO, Ok(b"not the pin"))]);
    let refused = net.fetch(&out, &cache).expect_err("refused");
    let mut attempts = Vec::new();
    for _ in WAITS {
        attempts.push(format!("{ONE}: download {ONE}: connection refused"));
        attempts.push(format!(
            "{TWO}: SHA-256 {}, not the pinned one",
            Sha256::of(b"not the pin")
        ));
    }
    assert_eq!(
        refused.to_string(),
        format!(
            "{cache:?} was not in the actions cache, and no download was the pinned file: {}; \
             nothing was installed",
            attempts.join("; ")
        )
    );
    assert!(!refused.to_string().contains('\n'));
    assert_eq!(
        refused,
        Error::NotCached {
            cache: cache.clone(),
            attempts
        }
    );
    assert_eq!(net.paused(), WAITS);
    assert_eq!(
        WAITS,
        [
            Duration::ZERO,
            Duration::from_secs(10),
            Duration::from_secs(30)
        ]
    );
    assert_eq!(ATTEMPT_TIMEOUT, Duration::from_secs(300));
    assert!(!cache.exists() && !part_path(&cache).exists());
    assert!(!out.exists());
}

/// Asserts that a cache file holding the pinned bytes is installed (0755) with no download and
/// no pause, the line says it came from the cache, and the cache file is left as it was.
#[test]
fn a_cache_hit_installs_the_cached_file_without_the_network() {
    let (out, cache) = paths("hit");
    seed(&cache, TOOL);
    let net = Transport::new(&[(ONE, Ok(b"never asked"))]);
    let line = net.fetch(&out, &cache).expect("hit");
    assert_eq!(
        line,
        format!(
            "ok: the cached {} has SHA-256 {}; nothing was downloaded; installed {}",
            cache.display(),
            Sha256::of(TOOL),
            out.display()
        )
    );
    assert_eq!(net.asked(), Vec::<String>::new());
    assert_eq!(net.paused(), Vec::<Duration>::new());
    assert_eq!(std::fs::read(&out).expect("out"), TOOL);
    assert_eq!(file_mode(&out), 0o755);
    assert_eq!(std::fs::read(&cache).expect("cache"), TOOL);
}

/// Asserts that a restored cache file whose SHA-256 is not the pin is refused, its full message
/// naming both digests; no download is made in its place, nothing is installed, and the file is
/// left as it is.
#[test]
fn a_cached_file_of_other_bytes_is_refused_without_a_download() {
    let (out, cache) = paths("cached-mismatch");
    let other: &[u8] = b"a tarball of another version";
    seed(&cache, other);
    let net = Transport::new(&[(ONE, Ok(TOOL))]);
    let refused = net.fetch(&out, &cache).expect_err("refused");
    assert_eq!(
        refused.to_string(),
        format!(
            "the cached {cache:?} has SHA-256 {}, not the pinned {}; nothing was installed",
            Sha256::of(other),
            Sha256::of(TOOL)
        )
    );
    assert_eq!(net.asked(), Vec::<String>::new());
    assert!(!out.exists());
    assert_eq!(std::fs::read(&cache).expect("cache"), other);
}

/// Asserts that a cached file that is the pin but lacks the member is that unpack error, not a
/// mismatch, with no download and nothing installed.
#[test]
fn a_cached_file_that_does_not_unpack_is_refused_without_a_download() {
    let (out, cache) = paths("cached-no-member");
    let archive = xz(&tar());
    seed(&cache, &archive);
    let net = Transport::new(&[(ONE, Ok(&archive))]);
    let refused = net
        .fetch_pinned(&Sha256::of(&archive), &tar_xz("nope"), &out, &cache)
        .expect_err("refused");
    assert_eq!(refused, Error::NoMember(PathBuf::from("nope")));
    assert_eq!(net.asked(), Vec::<String>::new());
    assert!(!out.exists());
}

/// Asserts that a cache path that cannot be read (a directory stands there) is `Error::Cache`
/// naming it, before any download, with nothing installed.
#[test]
fn a_cache_that_cannot_be_read_is_refused_before_a_download() {
    let (out, cache) = paths("cache-dir");
    std::fs::create_dir_all(&cache).expect("a directory where the file goes");
    let net = Transport::new(&[(ONE, Ok(TOOL))]);
    let refused = net.fetch(&out, &cache).expect_err("refused");
    let Error::Cache { cache: named, why } = &refused else {
        panic!("{refused:?}");
    };
    assert_eq!(named, &cache);
    assert_eq!(
        refused.to_string(),
        format!("the cache file {cache:?}: {why}")
    );
    assert_eq!(net.asked(), Vec::<String>::new());
    assert!(!out.exists());
}

/// Asserts that a pinned download that does not unpack (a member the archive lacks) is that
/// error, and is not kept, so the cache never saves it; the URL after it is not asked.
#[test]
fn a_pinned_download_that_does_not_unpack_is_not_kept() {
    let (out, cache) = paths("miss-no-member");
    let archive = xz(&tar());
    let net = Transport::new(&[(ONE, Ok(&archive))]);
    let refused = net
        .fetch_pinned(&Sha256::of(&archive), &tar_xz("nope"), &out, &cache)
        .expect_err("refused");
    assert_eq!(refused, Error::NoMember(PathBuf::from("nope")));
    assert_eq!(net.asked(), [ONE]);
    assert!(!cache.exists() && !part_path(&cache).exists());
    assert!(!out.exists());
}

/// Asserts that a pinned download that cannot be written to the cache (a directory stands where
/// its part file goes) is `Error::Cache` naming the cache, after the one download, with nothing
/// installed.
#[test]
fn a_pinned_download_that_cannot_be_kept_installs_nothing() {
    let (out, cache) = paths("cache-unwritable");
    std::fs::create_dir_all(part_path(&cache)).expect("a directory where the part file goes");
    let net = Transport::new(&[(ONE, Ok(TOOL))]);
    let refused = net.fetch(&out, &cache).expect_err("refused");
    assert!(
        matches!(&refused, Error::Cache { cache: named, .. } if *named == cache),
        "{refused:?}"
    );
    assert_eq!(net.asked(), [ONE]);
    assert!(!cache.exists());
    assert!(!out.exists());
}

/// Asserts that when the file cannot be installed (`out`'s directory is a file) the fetch fails
/// naming `out`, on a hit and on a miss; after the miss the checked download is still kept, for
/// the next run to hit.
#[test]
fn an_install_that_fails_is_an_error_on_a_hit_and_on_a_miss() {
    for hit in [true, false] {
        let (out, cache) = paths(&format!("install-{hit}"));
        if hit {
            seed(&cache, TOOL);
        }
        let blocker = out.parent().expect("bin");
        std::fs::create_dir_all(blocker.parent().expect("dir")).expect("mkdir");
        std::fs::write(blocker, b"a file, not a directory").expect("write");
        let net = Transport::new(&[(ONE, Ok(TOOL))]);
        let refused = net.fetch(&out, &cache).expect_err("refused");
        assert!(
            matches!(&refused, Error::Install { path, .. } if *path == out),
            "{hit}: {refused:?}"
        );
        assert_eq!(net.asked().len(), usize::from(!hit), "{hit}");
        assert_eq!(std::fs::read(&cache).expect("cache"), TOOL, "{hit}");
    }
}

/// Asserts the form the lint job fetches Zig in (`--extract tar.xz --tree`): on a miss the xz
/// tarball is kept as downloaded and its directory installed whole; on the next run's hit the
/// same tree is installed from the cache alone.
#[test]
fn a_cached_toolchain_is_installed_as_a_tree_on_a_miss_and_on_a_hit() {
    let (out, cache) = paths("tree");
    let archive = xz(&toolchain());
    let unpack = tree(Format::TarXz, "cc-1");
    let pin = Sha256::of(&archive);
    let net = Transport::new(&[(ONE, Ok(&archive))]);
    let line = net.fetch_pinned(&pin, &unpack, &out, &cache).expect("miss");
    assert!(line.starts_with(&format!("ok: {ONE} has SHA-256 {pin}; kept it at ")));
    assert_eq!(std::fs::read(&cache).expect("kept"), archive);
    let installed = listing(&out);
    std::fs::remove_dir_all(&out).expect("remove the tree");
    let again = Transport::new(&[]);
    let line = again
        .fetch_pinned(&pin, &unpack, &out, &cache)
        .expect("hit");
    assert!(line.starts_with("ok: the cached "), "{line}");
    assert_eq!(again.asked(), Vec::<String>::new());
    assert_eq!(listing(&out), installed);
    assert!(installed.contains(&("cc".to_owned(), 0o755, b"compiler".to_vec())));
}

/// Asserts that each attempt of a cached fetch is bounded by `ATTEMPT_TIMEOUT`, below the
/// `DOWNLOAD_TIMEOUT` of a plain fetch, and that the client is HTTPS only. Read from the
/// client's own configuration: no request, no clock.
#[test]
fn the_cached_fetch_client_gives_up_on_an_attempt_sooner_than_a_plain_fetch() {
    let attempt = agent(&[], ATTEMPT_TIMEOUT);
    assert_eq!(attempt.config().timeouts().global, Some(ATTEMPT_TIMEOUT));
    assert!(attempt.config().https_only());
    assert!(ATTEMPT_TIMEOUT < DOWNLOAD_TIMEOUT);
    assert_eq!(DOWNLOAD_TIMEOUT, Duration::from_secs(600));
}
