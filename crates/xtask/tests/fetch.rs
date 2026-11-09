//! `fetch` against real HTTPS: a local server with its own CA, answering each path with
//! a canned response, and the `cargo xtask fetch` binary for its flags and exit codes.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use rcgen::{CertificateParams, CertifiedIssuer, IsCa, KeyPair};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use xtask::fetch::{self, Sha256, Unpack};

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");
const TOOL: &[u8] = b"a pinned tool";

/// An empty directory of its own for one test.
/// Removed when the returned value is dropped.
fn scratch(name: &str) -> js_testkit::dir::TestDir {
    js_testkit::dir::TestDir::under(
        env!("CARGO_TARGET_TMPDIR"),
        &format!("xtask-fetch-https-{name}"),
    )
}

/// A canned HTTP/1.1 response.
fn response(status: &str, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    let mut r = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\n", body.len());
    for (name, value) in headers {
        r.push_str(&format!("{name}: {value}\r\n"));
    }
    r.push_str("Connection: close\r\n\r\n");
    [r.into_bytes(), body.to_vec()].concat()
}

/// The path of a request read from `stream` (its head, up to the blank line), or `None`
/// when the client went away first (a handshake it refused).
fn request_path(stream: impl std::io::Read) -> Option<String> {
    let mut lines = BufReader::new(stream).lines();
    let first = lines.next()?.ok()?;
    for line in lines {
        if line.ok()?.is_empty() {
            break;
        }
    }
    Some(first.split(' ').nth(1)?.to_owned())
}

/// Serves `routes` (path, response) forever on a loopback port, over TLS with a
/// certificate from a fresh CA when `tls`, else plain: the base URL, and the
/// CA's certificate.
fn serve(
    tls: bool,
    routes: impl Fn(&str, &str) -> Vec<u8> + Send + 'static,
) -> (String, CertificateDer<'static>) {
    let mut params = CertificateParams::new(Vec::<String>::new()).expect("CA params");
    params.is_ca = IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca = CertifiedIssuer::self_signed(params, KeyPair::generate().expect("key")).expect("CA");
    let key = KeyPair::generate().expect("key");
    let cert = CertificateParams::new(vec!["127.0.0.1".to_owned()])
        .expect("params")
        .signed_by(&key, &ca)
        .expect("sign");
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("versions")
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.der().clone()],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
        )
        .expect("server config");
    let config = Arc::new(config);
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let base = format!("{}://127.0.0.1:{port}", if tls { "https" } else { "http" });
    let origin = base.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let stream = stream.expect("accept");
            if tls {
                let conn = rustls::ServerConnection::new(Arc::clone(&config)).expect("conn");
                let mut tls = rustls::StreamOwned::new(conn, stream);
                // A handshake the client abandons (an untrusted certificate) ends here.
                let Some(path) = request_path(&mut tls) else {
                    continue;
                };
                let _ = tls.write_all(&routes(&origin, &path));
                tls.conn.send_close_notify();
                let _ = tls.flush();
            } else {
                let mut stream = stream;
                let Some(path) = request_path(&mut stream) else {
                    continue;
                };
                let _ = stream.write_all(&routes(&origin, &path));
            }
        }
    });
    (base, ca.der().clone())
}

/// Asserts that a download follows redirects (every GitHub release download answers 302
/// to its storage host) and returns the final body, not the redirect's own.
#[test]
fn a_download_follows_redirects_and_returns_the_body() {
    let (base, ca) = serve(true, |origin, path| match path {
        "/release" => response(
            "302 Found",
            &[("Location", &format!("{origin}/blob"))],
            b"go",
        ),
        _ => response("200 OK", &[], TOOL),
    });
    let got = fetch::download(&format!("{base}/release"), &[ca]).expect("download");
    assert_eq!(got, TOOL);
}

/// Asserts that a status other than 2xx is an error that names the status.
#[test]
fn a_status_that_is_not_2xx_is_an_error() {
    let (base, ca) = serve(true, |_, _| {
        response("404 Not Found", &[], b"no such release")
    });
    let url = format!("{base}/missing");
    let Err(fetch::Error::Download { url: got, why }) = fetch::download(&url, &[ca]) else {
        panic!("a 404 was not an error");
    };
    assert_eq!(got, url);
    assert!(why.contains("404"), "{why}");
}

/// Asserts that a redirect from HTTPS to plain HTTP is refused.
#[test]
fn a_redirect_to_plain_http_is_refused() {
    let (plain, _) = serve(false, |_, _| response("200 OK", &[], TOOL));
    let (base, ca) = serve(true, move |_, _| {
        response("302 Found", &[("Location", &format!("{plain}/blob"))], b"")
    });
    let result = fetch::download(&format!("{base}/release"), &[ca]);
    let Err(fetch::Error::Download { why, .. }) = result else {
        panic!("{result:?}");
    };
    assert!(why.contains("https"), "{why}");
}

/// Asserts that a certificate the given roots do not trust is refused: the host's roots
/// do not include this test CA.
#[test]
fn a_certificate_the_roots_do_not_trust_is_refused() {
    let (base, _) = serve(true, |_, _| response("200 OK", &[], TOOL));
    let roots = fetch::host_roots();
    let result = fetch::download(&format!("{base}/blob"), &roots);
    assert!(
        matches!(result, Err(fetch::Error::Download { .. })),
        "{result:?}"
    );
}

/// Asserts that `fetch` checks the hash before installing, and reports success only for a
/// file it wrote.
#[test]
fn fetch_installs_a_matching_download_and_refuses_a_mismatch() {
    let (base, ca) = serve(true, |_, _| response("200 OK", &[], TOOL));
    let dir = scratch("fetch");
    let out = dir.join("bin").join("tool");
    let url = format!("{base}/tool");
    let line = fetch::fetch(
        &url,
        &Sha256::of(TOOL),
        &Unpack::Raw,
        &out,
        None,
        std::slice::from_ref(&ca),
    )
    .expect("fetch");
    assert_eq!(
        line,
        format!(
            "ok: {url} has SHA-256 {}; installed {}",
            Sha256::of(TOOL),
            out.display()
        )
    );
    assert_eq!(std::fs::read(&out).expect("read"), TOOL);
    let dir = scratch("fetch-mismatch");
    let other = dir.join("tool");
    let pinned = Sha256::of(b"another tool");
    let result = fetch::fetch(&url, &pinned, &Unpack::Raw, &other, None, &[ca]);
    assert!(
        matches!(result, Err(fetch::Error::Mismatch { .. })),
        "{result:?}"
    );
    assert!(!other.exists() && !fetch::part_path(&other).exists());
}

/// `cargo xtask fetch` with `args`: its exit code and stderr.
fn xtask_fetch(args: &[&str]) -> (Option<i32>, String) {
    let out = Command::new(XTASK)
        .arg("fetch")
        .args(args)
        .output()
        .expect("run xtask");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

const PIN: &str = "9f12ed4c49936e09b48bf862b595cde2fe64fcbd9d74dfacac6131ca824c8d5f";

/// Asserts that the verb refuses plain HTTP, exits 1 and writes nothing.
#[test]
fn the_verb_refuses_plain_http_with_exit_1_and_writes_nothing() {
    let dir = scratch("cli-http");
    let out = dir.join("tool");
    let url = "http://127.0.0.1:9/tool";
    let (code, err) = xtask_fetch(&[
        "--url",
        url,
        "--sha256",
        PIN,
        "--out",
        out.to_str().expect("utf-8"),
    ]);
    assert_eq!(code, Some(1), "{err}");
    assert!(
        err.starts_with(&format!("xtask: download {url}: ")),
        "{err}"
    );
    assert!(!out.exists() && !fetch::part_path(&out).exists());
}

/// Asserts that a malformed pin, a tar without `--member` and a `--member` without a tar
/// all exit 2 (the usage status) before any download.
#[test]
fn flags_that_do_not_fit_together_exit_2() {
    let dir = scratch("cli-usage");
    let out = dir.join("tool");
    let out = out.to_str().expect("utf-8");
    let url = "https://127.0.0.1:9/tool";
    for (extra, says) in [
        (&["--sha256", "ABC"][..], "not a SHA-256"),
        (
            &["--sha256", PIN, "--extract", "tar.gz"][..],
            "needs --member",
        ),
        (
            &["--sha256", PIN, "--member", "a"][..],
            "needs --extract tar.gz",
        ),
        (&["--sha256", PIN, "--extract", "rar"][..], "invalid value"),
    ] {
        let mut args = vec!["--url", url, "--out", out];
        args.extend_from_slice(extra);
        let (code, err) = xtask_fetch(&args);
        assert_eq!(code, Some(2), "{extra:?}: {err}");
        assert!(err.contains(says), "{extra:?}: {err}");
    }
    assert!(!Path::new(out).exists());
}

/// A gzip tar holding `tool-1.0/tool` with `TOOL` in it: what the cache keeps is this
/// archive, not the file taken out of it.
fn tool_tar_gz() -> Vec<u8> {
    let mut b = tar::Builder::new(Vec::new());
    let mut h = tar::Header::new_gnu();
    h.set_mode(0o755);
    h.set_size(TOOL.len() as u64);
    b.append_data(&mut h, "tool-1.0/tool", TOOL).expect("file");
    let tar = b.into_inner().expect("tar");
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gz.write_all(&tar).expect("gzip");
    gz.finish().expect("gzip")
}

fn member() -> Unpack {
    Unpack::TarGz {
        member: "tool-1.0/tool".into(),
    }
}

/// Serves `body` with `status` at every path over TLS, counting the requests: the base URL,
/// the CA and the count.
fn counted(
    status: &'static str,
    body: Vec<u8>,
) -> (String, CertificateDer<'static>, Arc<AtomicUsize>) {
    let count = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&count);
    let (base, ca) = serve(true, move |_, _| {
        seen.fetch_add(1, Ordering::SeqCst);
        response(status, &[], &body)
    });
    (format!("{base}/tool.tar.gz"), ca, count)
}

/// Asserts that a cached file that matches the pin is unpacked and installed, and that
/// nothing is downloaded: a cache hit survives a release-download outage.
#[test]
fn a_cached_file_that_matches_is_installed_without_a_download() {
    let archive = tool_tar_gz();
    let (url, ca, requests) = counted("504 Gateway Timeout", Vec::new());
    let dir = scratch("cache-hit");
    let cache = dir.join("pinned").join("tool.tar.gz");
    std::fs::create_dir_all(dir.join("pinned")).expect("mkdir");
    std::fs::write(&cache, &archive).expect("seed the cache");
    let out = dir.join("bin").join("tool");
    let pin = Sha256::of(&archive);
    let line = fetch::fetch(&url, &pin, &member(), &out, Some(&cache), &[ca]).expect("fetch");
    assert_eq!(
        line,
        format!(
            "ok: the cached {} of {url} has SHA-256 {pin}; installed {}",
            cache.display(),
            out.display()
        )
    );
    assert_eq!(std::fs::read(&out).expect("read"), TOOL);
    assert_eq!(requests.load(Ordering::SeqCst), 0);
    assert_eq!(std::fs::read(&cache).expect("cache"), archive);
}

/// Asserts that a cached file whose SHA-256 is not the pin is refused, naming the cache
/// file and both digests, and that nothing is downloaded in its place or installed.
#[test]
fn a_cached_file_that_does_not_match_is_refused_without_a_download() {
    let archive = tool_tar_gz();
    let (url, ca, requests) = counted("200 OK", archive.clone());
    let dir = scratch("cache-mismatch");
    let cache = dir.join("tool.tar.gz");
    std::fs::write(&cache, b"another file").expect("seed the cache");
    let out = dir.join("bin").join("tool");
    let pin = Sha256::of(&archive);
    let err = fetch::fetch(&url, &pin, &member(), &out, Some(&cache), &[ca]).unwrap_err();
    assert_eq!(
        err.to_string(),
        format!(
            "the cached {cache:?} has SHA-256 {}, not the pinned {pin}; nothing was installed",
            Sha256::of(b"another file")
        )
    );
    assert_eq!(requests.load(Ordering::SeqCst), 0);
    assert!(!out.exists() && !fetch::part_path(&out).exists());
    assert_eq!(std::fs::read(&cache).expect("cache"), b"another file");
}

/// Asserts that a cached file that matches the pin but lacks the member is refused as the
/// download would be, with nothing downloaded or installed.
#[test]
fn a_cached_file_that_matches_but_does_not_unpack_is_refused() {
    let archive = tool_tar_gz();
    let (url, ca, requests) = counted("200 OK", archive.clone());
    let dir = scratch("cache-no-member");
    let cache = dir.join("tool.tar.gz");
    std::fs::write(&cache, &archive).expect("seed the cache");
    let out = dir.join("tool");
    let other = Unpack::TarGz {
        member: "tool-1.0/other".into(),
    };
    let err = fetch::fetch(
        &url,
        &Sha256::of(&archive),
        &other,
        &out,
        Some(&cache),
        &[ca],
    )
    .unwrap_err();
    assert_eq!(err, fetch::Error::NoMember("tool-1.0/other".into()));
    assert_eq!(requests.load(Ordering::SeqCst), 0);
    assert!(!out.exists());
}

/// Asserts that on a miss the download is checked, kept at the cache path as the archive
/// it came as (mode 0644, its directory created), and the member installed.
#[test]
fn a_miss_downloads_keeps_the_archive_and_installs() {
    let archive = tool_tar_gz();
    let (url, ca, requests) = counted("200 OK", archive.clone());
    let dir = scratch("cache-miss");
    let cache = dir.join("pinned").join("tool.tar.gz");
    let out = dir.join("bin").join("tool");
    let pin = Sha256::of(&archive);
    let line = fetch::fetch(&url, &pin, &member(), &out, Some(&cache), &[ca]).expect("fetch");
    assert_eq!(
        line,
        format!(
            "ok: {url} has SHA-256 {pin}; kept it at {} and installed {}",
            cache.display(),
            out.display()
        )
    );
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    assert_eq!(std::fs::read(&cache).expect("cache"), archive);
    let mode = std::fs::metadata(&cache)
        .expect("meta")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o644);
    assert_eq!(std::fs::read(&out).expect("out"), TOOL);
    assert!(!fetch::part_path(&cache).exists());
}

/// Asserts that on a miss a download that is not the pinned file is neither kept nor
/// installed, so the cache never saves it.
#[test]
fn a_miss_whose_download_does_not_match_keeps_nothing() {
    let (url, ca, _) = counted("200 OK", b"a different archive".to_vec());
    let dir = scratch("cache-miss-mismatch");
    let cache = dir.join("pinned").join("tool.tar.gz");
    let out = dir.join("tool");
    let pin = Sha256::of(&tool_tar_gz());
    let err = fetch::fetch(&url, &pin, &member(), &out, Some(&cache), &[ca]).unwrap_err();
    assert_eq!(
        err,
        fetch::Error::Mismatch {
            expected: pin,
            actual: Sha256::of(b"a different archive"),
        }
    );
    assert!(!cache.exists() && !fetch::part_path(&cache).exists());
    assert!(!out.exists());
}

/// Asserts that on a miss a download that matches but does not unpack is not kept.
#[test]
fn a_miss_whose_download_does_not_unpack_keeps_nothing() {
    let archive = tool_tar_gz();
    let (url, ca, _) = counted("200 OK", archive.clone());
    let dir = scratch("cache-miss-no-member");
    let cache = dir.join("tool.tar.gz");
    let out = dir.join("tool");
    let other = Unpack::TarGz {
        member: "nope".into(),
    };
    let err = fetch::fetch(
        &url,
        &Sha256::of(&archive),
        &other,
        &out,
        Some(&cache),
        &[ca],
    )
    .unwrap_err();
    assert_eq!(err, fetch::Error::NoMember("nope".into()));
    assert!(!cache.exists() && !out.exists());
}

/// Asserts that a cold cache with the release download failing (a 504, as in an outage) is
/// one error line naming the cache path and the download's failure, with nothing kept or
/// installed.
#[test]
fn a_miss_whose_download_fails_says_so_in_one_line() {
    let (url, ca, requests) = counted("504 Gateway Timeout", b"upstream timed out".to_vec());
    let dir = scratch("cache-miss-504");
    let cache = dir.join("tool.tar.gz");
    let out = dir.join("tool");
    let pin = Sha256::of(&tool_tar_gz());
    let err = fetch::fetch(&url, &pin, &member(), &out, Some(&cache), &[ca]).unwrap_err();
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    assert_eq!(
        err.to_string(),
        format!(
            "{cache:?} was not in the actions cache, and the download failed: \
             download {url}: http status: 504; nothing was installed"
        )
    );
    assert!(!cache.exists() && !out.exists());
}

/// Asserts that a cache path that exists but cannot be read as a file (a directory) is an
/// error naming it, with nothing downloaded or installed.
#[test]
fn a_cache_path_that_cannot_be_read_is_an_error() {
    let (url, ca, requests) = counted("200 OK", tool_tar_gz());
    let dir = scratch("cache-dir");
    let cache = dir.join("tool.tar.gz");
    std::fs::create_dir_all(&cache).expect("mkdir");
    let out = dir.join("tool");
    let err = fetch::fetch(
        &url,
        &Sha256::of(&tool_tar_gz()),
        &member(),
        &out,
        Some(&cache),
        &[ca],
    )
    .unwrap_err();
    let fetch::Error::Cache { cache: named, why } = &err else {
        panic!("{err:?}");
    };
    assert_eq!(named, &cache);
    assert_eq!(err.to_string(), format!("the cache file {cache:?}: {why}"));
    assert_eq!(requests.load(Ordering::SeqCst), 0);
    assert!(!out.exists());
}

/// Asserts that when the checked download cannot be kept (the cache path's directory is a
/// file), the fetch fails naming the cache path and installs nothing.
#[test]
fn a_download_that_cannot_be_kept_installs_nothing() {
    let archive = tool_tar_gz();
    let (url, ca, _) = counted("200 OK", archive.clone());
    let dir = scratch("cache-unwritable");
    let blocker = dir.join("pinned");
    std::fs::write(&blocker, b"a file, not a directory").expect("write");
    let cache = blocker.join("tool.tar.gz");
    let out = dir.join("tool");
    let err = fetch::fetch(
        &url,
        &Sha256::of(&archive),
        &member(),
        &out,
        Some(&cache),
        &[ca],
    )
    .unwrap_err();
    assert!(
        matches!(&err, fetch::Error::Cache { cache: named, .. } if named == &cache),
        "{err:?}"
    );
    assert!(!out.exists());
}

/// Asserts that the verb with `--cache` installs a matching cached file and exits 0 without
/// downloading: its URL is plain HTTP, which a download refuses.
#[test]
fn the_verb_installs_a_cached_file_with_exit_0() {
    let dir = scratch("cli-cache-hit");
    let cache = dir.join("tool");
    std::fs::write(&cache, TOOL).expect("seed the cache");
    let out = dir.join("bin").join("tool");
    let (plain, _) = serve(false, |_, _| response("200 OK", &[], b"not this"));
    let pin = Sha256::of(TOOL).to_string();
    let (code, err) = xtask_fetch(&[
        "--url",
        &format!("{plain}/tool"),
        "--sha256",
        &pin,
        "--out",
        out.to_str().expect("utf-8"),
        "--cache",
        cache.to_str().expect("utf-8"),
    ]);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(std::fs::read(&out).expect("out"), TOOL);
}

/// Asserts that the verb with a cold `--cache` and a failing download exits 1 with exactly
/// one line on stderr, saying the file was not in the cache and the download failed.
#[test]
fn the_verb_with_a_cold_cache_and_a_failed_download_exits_1_in_one_line() {
    let dir = scratch("cli-cache-cold");
    let cache = dir.join("pinned").join("tool.tar.gz");
    let out = dir.join("tool");
    let (plain, _) = serve(false, |_, _| response("200 OK", &[], TOOL));
    let url = format!("{plain}/tool");
    let (code, err) = xtask_fetch(&[
        "--url",
        &url,
        "--sha256",
        PIN,
        "--out",
        out.to_str().expect("utf-8"),
        "--cache",
        cache.to_str().expect("utf-8"),
    ]);
    assert_eq!(code, Some(1), "{err}");
    assert_eq!(err.lines().count(), 1, "{err}");
    let head = format!(
        "xtask: {cache:?} was not in the actions cache, and the download failed: download {url}: "
    );
    assert!(err.starts_with(&head), "{err}");
    assert!(err.ends_with("; nothing was installed\n"), "{err}");
    assert!(!cache.exists() && !out.exists());
}

/// Asserts that when the file cannot be installed (`--out`'s directory is a file), the fetch
/// fails naming `--out`, whether the archive came from the cache or from a download; after
/// the download the checked archive is still kept, as the later save would find it.
#[test]
fn an_install_that_fails_is_an_error_on_a_hit_and_on_a_miss() {
    let archive = tool_tar_gz();
    let pin = Sha256::of(&archive);
    for hit in [true, false] {
        let (url, ca, requests) = counted("200 OK", archive.clone());
        let dir = scratch(&format!("cache-install-{hit}"));
        let cache = dir.join("tool.tar.gz");
        if hit {
            std::fs::write(&cache, &archive).expect("seed the cache");
        }
        let blocker = dir.join("bin");
        std::fs::write(&blocker, b"a file, not a directory").expect("write");
        let out = blocker.join("tool");
        let err = fetch::fetch(&url, &pin, &member(), &out, Some(&cache), &[ca]).unwrap_err();
        assert!(
            matches!(&err, fetch::Error::Install { path, .. } if path == &out),
            "{hit}: {err:?}"
        );
        assert_eq!(requests.load(Ordering::SeqCst), usize::from(!hit));
        assert_eq!(std::fs::read(&cache).expect("cache"), archive);
    }
}
