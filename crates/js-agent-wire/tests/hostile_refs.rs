//! Image references, label keys and mount paths: the grammars a hostile start has to
//! pass, each test saying the defect it catches.

mod common;

use common::{broken, expect, hello, job, long, map};
use js_agent_wire::limits::*;
use js_agent_wire::refusal;
use js_agent_wire::{Hello, JobSpec, Mount, Rule};

fn image_error(image: &str) -> js_agent_wire::ValidationError {
    broken(&job(), |j| j.image = image.into(), JobSpec::validate)
}

fn hex(len: usize) -> String {
    "0123456789abcdef".chars().cycle().take(len).collect()
}

/// A reference a container tool would read as a local transport and a host path is
/// refused: `oci:`, `dir:`, `docker-archive:`, `docker://`, `containers-storage:` and the
/// other transports, even where the rest would parse as a tag. It catches the peer
/// choosing a path on the agent's disk instead of a registry image.
#[test]
fn an_image_cannot_name_a_transport() {
    for image in [
        "oci:/var/lib/x",
        "dir:/etc",
        "docker-archive:/tmp/a.tar",
        "docker://x",
        "docker://registry.example.com/x",
        "containers-storage:foo",
        "oci-archive:/a",
        "docker-daemon:img",
        "ostree:x@/y",
        "sif:/x",
        "tarball:/x",
        "docker:x",
        "oci:latest",
        "dir:5000/x",
    ] {
        expect(
            image_error(image),
            "image",
            Rule::BadChars,
            refusal::INVALID_IMAGE,
        );
    }
}

/// The grammar `name[:tag][@digest]`: a bad component, port, tag or digest is refused; each
/// line below is a different defect (empty or doubled `/`, an empty or malformed port, an
/// empty, over-long or badly started tag, a short, upper-case or unnamed digest, a path
/// component in upper case or ending in a separator, an IPv6 host, and a second `@`).
#[test]
fn an_image_follows_the_reference_grammar() {
    let sha = format!("sha256:{}", hex(64));
    let tag_too_long = format!("a:{}", long(129));
    let short_digest = format!("a@sha256:{}", hex(31));
    let upper_digest = format!("a@sha256:{}", hex(64).to_uppercase());
    let upper_algorithm = format!("a@Sha256:{}", hex(64));
    let no_algorithm = format!("a@:{}", hex(64));
    let two_digests = format!("a@{sha}@{sha}");
    let bad_algorithm = format!("a@sha..256:{}", hex(64));
    for image in [
        "a//b",
        "a/",
        "/a",
        "a:",
        "a:-1",
        "a:.b",
        "a:b c",
        "a@",
        "a@sha256",
        "a@sha256:",
        "a@sha256:xyz",
        "A_/x",
        "a/B",
        "a/b/C",
        "a/b__",
        "a___b",
        "a-.b",
        "a..b",
        "a.-b",
        "-a",
        "a-",
        ".a",
        "a:b:c",
        "a:b/c",
        "host:/x",
        "host:1:2/x",
        "[::1]:5000/x",
        "ho st/x",
        "é/x",
        "a/é",
        tag_too_long.as_str(),
        short_digest.as_str(),
        upper_digest.as_str(),
        upper_algorithm.as_str(),
        no_algorithm.as_str(),
        two_digests.as_str(),
        bad_algorithm.as_str(),
    ] {
        expect(
            image_error(image),
            "image",
            Rule::BadChars,
            refusal::INVALID_IMAGE,
        );
    }
}

/// Real references pass: bare names, tags, digests (the shortest and an unusual
/// algorithm), registries with and without a port, an upper-case domain, an IP domain,
/// `_`, `__` and `-` runs in a path, and a tag of exactly 128 bytes. It catches a grammar
/// so strict it refuses what a registry serves.
#[test]
fn real_image_references_pass() {
    let sha = format!("sha256:{}", hex(64));
    let tag_at_cap = format!("a:{}", long(128));
    let shortest = format!("a@sha256:{}", hex(32));
    let multi = format!("a@sha512:{}", hex(128));
    let odd_algorithm = format!("a@multi+hash.v2_x-y:{}", hex(40));
    let everything = format!("host:5000/a/b/c:tag@{sha}");
    let tag_then_digest = format!("a:v1@{sha}");
    for image in [
        "alpine",
        "alpine:3.19",
        "a/b",
        "ghcr.io/org/app:v1",
        "registry.example.com:5000/team/app:v1",
        "localhost:5000/x",
        "localhost/x",
        "Registry.Example.COM/x",
        "10.0.0.1:5000/x",
        "team_a/app__x--y.z",
        "a:_x",
        "a:1.2-rc_1",
        "a.b/c",
        "a-b.c/d",
        tag_at_cap.as_str(),
        shortest.as_str(),
        multi.as_str(),
        odd_algorithm.as_str(),
        everything.as_str(),
        tag_then_digest.as_str(),
    ] {
        let mut spec = job();
        spec.image = image.into();
        assert_eq!(spec.validate(), Ok(()), "{image}");
    }
}

fn label_error(key: &str) -> js_agent_wire::ValidationError {
    broken(&job(), |j| j.labels = map(&[(key, "v")]), JobSpec::validate)
}

/// A key with a doubled or leading `/`, a second `/`, a dotted-out path, a prefix
/// that is not a DNS name, or a name that does not start with a letter or digit is refused
/// as a malformed key. It catches `a/`, `a//b` and `a/../b` reaching a cloud resource as a
/// label key.
#[test]
fn label_keys_are_a_name_or_prefix_and_name() {
    for key in [
        "/a", "a//b", "a/../b", "a/b/c", "a/-b", "a/.b", ".a/b", "a../b", "a.b./c", "-a.b/c",
        "a-/c", "A.b/c", "a_b/c", "é/x", "a/é", ".", "..",
    ] {
        expect(
            label_error(key),
            "labels",
            Rule::BadChars,
            refusal::INVALID_JOB,
        );
    }
}

/// The empty key and a key with nothing after its `/` (`a/`, `/`) are empty, a name over the
/// cap is too long, and a bare `jobscheduler.dev`
/// (the reserved prefix without its slash), a key under the prefix, and one with more
/// path after it are reserved. It catches a caller writing the scheduler's own label
/// without the slash that the prefix test looks for.
#[test]
fn label_key_edges_and_the_reserved_prefix() {
    for key in ["", "a/", "/"] {
        let err = label_error(key);
        expect(err, "labels", Rule::Empty, refusal::INVALID_JOB);
    }
    expect(
        label_error(&long(MAX_LABEL_KEY_LEN + 1)),
        "labels",
        Rule::TooLong,
        refusal::INVALID_JOB,
    );
    for key in [
        "jobscheduler.dev",
        "jobscheduler.dev/",
        "jobscheduler.dev/tenant",
        "jobscheduler.dev/a/b",
    ] {
        expect(
            label_error(key),
            "labels",
            Rule::Reserved,
            refusal::RESERVED_LABEL,
        );
    }
    let on_a_hello = broken(
        &hello(),
        |h| h.labels = map(&[("jobscheduler.dev", "v")]),
        Hello::validate,
    );
    expect(
        on_a_hello,
        "labels",
        Rule::Reserved,
        refusal::RESERVED_LABEL,
    );
    let on_a_hello = broken(
        &hello(),
        |h| h.labels = map(&[("a//b", "v")]),
        Hello::validate,
    );
    expect(on_a_hello, "labels", Rule::BadChars, refusal::INVALID_HELLO);
}

/// Plain keys, a prefix and a name, a longer prefix, a key that is exactly the cap, and a
/// prefix that merely starts like the reserved one pass. It catches a key grammar so strict
/// it refuses ordinary labels.
#[test]
fn ordinary_label_keys_pass() {
    let at_cap = format!(
        "{}.{}/{}",
        long(MAX_DNS_LABEL_LEN),
        long(2),
        long(MAX_LABEL_KEY_LEN - MAX_DNS_LABEL_LEN - 4)
    );
    let mut keys = vec![
        "team",
        "example.com/team",
        "a.b-c.d/e_f.g-h",
        "k8s.io/name",
        "a/b",
        "a-b/c",
        "x1/y",
        "jobscheduler.devx/y",
        "xjobscheduler.dev/y",
        "jobscheduler.dev.example/y",
    ];
    keys.push(&at_cap);
    for key in keys {
        let mut spec = job();
        spec.labels = map(&[(key, "v")]);
        assert_eq!(spec.validate(), Ok(()), "{key}");
    }
}

fn mount(rel: &str) -> Mount {
    Mount {
        root_name: "cache".into(),
        rel: rel.into(),
        ro: true,
    }
}

/// A rel with a backslash is refused wherever it sits, so `a\..\..` (one name to a Unix
/// path check, two climbs to a Windows one) gets the same answer on every OS; the same
/// strings with `/` are refused for climbing. It catches the verdict depending on the
/// platform the validator runs on.
#[test]
fn a_rel_means_the_same_on_every_os() {
    for rel in ["a\\..\\..", "..\\x", "a\\b", "C:\\x", "\\\\host\\share"] {
        let err = mount(rel).validate().unwrap_err();
        expect(err, "mounts.rel", Rule::BadChars, refusal::INVALID_MOUNT);
    }
    for rel in ["a/../..", "../x", "a/../b"] {
        let err = mount(rel).validate().unwrap_err();
        expect(err, "mounts.rel", Rule::Escapes, refusal::INVALID_MOUNT);
    }
}

/// A rel with a right-to-left override, a zero-width space, a line separator, a bidi
/// isolate or a tag character is refused like free text. It catches a path that reads as
/// another one in a log.
#[test]
fn a_rel_refuses_format_characters() {
    for c in ['\u{202E}', '\u{200B}', '\u{2028}', '\u{2066}', '\u{E0041}'] {
        let rel = format!("a{c}b");
        let err = mount(&rel).validate().unwrap_err();
        expect(err, "mounts.rel", Rule::BadChars, refusal::INVALID_MOUNT);
    }
    assert_eq!(mount("a\u{e9}\u{4e2d}/b").validate(), Ok(()));
}

/// The same root and rel twice is refused whether the two differ in `ro` or not; the same
/// rel under another root, and another rel under the same root, pass. It catches a mount
/// listed twice, one read-only and one writable, leaving the writable flag ambiguous.
#[test]
fn duplicate_mounts_are_refused() {
    let place = |root: &str, rel: &str, ro| Mount {
        root_name: root.into(),
        rel: rel.into(),
        ro,
    };
    let with = |mounts: Vec<Mount>| broken(&job(), |j| j.mounts = mounts, JobSpec::validate);
    let same = vec![place("cache", "x", true), place("cache", "x", false)];
    expect(
        with(same),
        "mounts",
        Rule::Duplicate,
        refusal::INVALID_MOUNT,
    );
    let twice = vec![place("cache", "x", true), place("cache", "x", true)];
    expect(
        with(twice),
        "mounts",
        Rule::Duplicate,
        refusal::INVALID_MOUNT,
    );
    let mut spec = job();
    spec.mounts = vec![
        place("cache", "x", true),
        place("data", "x", false),
        place("cache", "y", false),
    ];
    assert_eq!(spec.validate(), Ok(()));
}

/// A label name of exactly 63 bytes passes and one of 64 is too long, with or without a
/// prefix. It catches a name bounded only by the 128-byte whole-key cap, which would let a
/// 100-character name through to a cloud that limits a label name to 63.
#[test]
fn a_label_name_is_at_most_63_bytes() {
    let mut spec = job();
    spec.labels = map(&[
        (&long(MAX_LABEL_NAME_LEN), "v"),
        (&format!("p/{}", long(MAX_LABEL_NAME_LEN)), "v"),
    ]);
    assert_eq!(spec.validate(), Ok(()));
    for key in [
        long(MAX_LABEL_NAME_LEN + 1),
        format!("p/{}", long(MAX_LABEL_NAME_LEN + 1)),
    ] {
        expect(
            label_error(&key),
            "labels",
            Rule::TooLong,
            refusal::INVALID_JOB,
        );
    }
}

/// A prefix label of exactly 63 bytes passes and one of 64 is too long, in the first or a
/// later label. It catches a DNS-name prefix with no per-label cap, so that
/// `<64 x a>.example/n` is accepted though it is not a DNS name.
#[test]
fn each_dns_label_of_a_prefix_is_at_most_63_bytes() {
    let ok = long(MAX_DNS_LABEL_LEN);
    let over = long(MAX_DNS_LABEL_LEN + 1);
    let mut spec = job();
    spec.labels = map(&[
        (&format!("{ok}.example/n"), "v"),
        (&format!("example.{ok}/n"), "v"),
    ]);
    assert_eq!(spec.validate(), Ok(()));
    for key in [format!("{over}.example/n"), format!("example.{over}/n")] {
        expect(
            label_error(&key),
            "labels",
            Rule::TooLong,
            refusal::INVALID_JOB,
        );
    }
}
