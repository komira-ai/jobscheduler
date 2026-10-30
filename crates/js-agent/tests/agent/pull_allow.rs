//! `PullAllow`: how a job's image is matched against the `--container-pull-allow` entries.
//! The same grammar and name rules as `js_oci::Allow` and `js_oci::Location`, so these
//! tests pin the cases where a reading of "registry plus namespace" goes wrong.

use js_agent::pull_allow::{EntryError, MAX_ENTRIES, Outside, PullAllow};

const DIGEST: &str = "@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn list(entries: &[&str]) -> PullAllow {
    let owned: Vec<String> = entries.iter().map(|entry| (*entry).to_owned()).collect();
    PullAllow::parse(&owned).unwrap_or_else(|why| panic!("{entries:?}: {why}"))
}

/// Whether `image`, pinned by a digest as the wire requires, is admitted by `entries`.
fn admitted(entries: &[&str], image: &str) -> bool {
    list(entries).check(&format!("{image}{DIGEST}")).is_ok()
}

/// Asserts that an empty list admits every image, including one that does not parse as a
/// registry path. It catches a default that refuses jobs on an agent started without the
/// flag.
#[test]
fn an_empty_list_admits_every_image() {
    let none = PullAllow::default();
    assert!(none.is_empty());
    for image in [
        "ghcr.io/komira-ai/x",
        "x",
        "evil.example:5000/a/b:tag",
        "UPPER/Case",
        "a//b",
    ] {
        assert_eq!(none.check(&format!("{image}{DIGEST}")), Ok(()), "{image}");
    }
}

/// Asserts that an entry covers its namespace by whole path components: the namespace
/// itself and below, not a sibling that starts with the same letters. It catches a
/// text-prefix match, which would let `komira-ai-evil` ride on `komira-ai`.
#[test]
fn a_prefix_covers_whole_path_components_only() {
    let entry = ["ghcr.io/komira-ai/"];
    assert!(admitted(&entry, "ghcr.io/komira-ai/kbf"));
    assert!(admitted(&entry, "ghcr.io/komira-ai/kbf/task"));
    assert!(admitted(&entry, "ghcr.io/komira-ai"));
    assert!(!admitted(&entry, "ghcr.io/komira-ai-evil/kbf"));
    assert!(!admitted(&entry, "ghcr.io/komira-aievil"));
    assert!(!admitted(&entry, "ghcr.io/komira/kbf"));
    assert!(!admitted(&entry, "ghcr.io/other/komira-ai/kbf"));
    assert!(!admitted(&entry, "ghcr.io/x"));
    // The entry with and without the trailing slash is the same entry.
    assert!(admitted(&["ghcr.io/komira-ai"], "ghcr.io/komira-ai/kbf"));
    assert!(!admitted(
        &["ghcr.io/komira-ai"],
        "ghcr.io/komira-ai-evil/kbf"
    ));
}

/// Asserts that another registry with the same path is refused, and that a registry that
/// merely contains the allowed host's name (a suffix or prefix of it) is too. It catches a
/// host comparison by substring.
#[test]
fn the_registry_must_be_the_same_host() {
    let entry = ["ghcr.io/komira-ai/"];
    assert!(!admitted(&entry, "quay.io/komira-ai/kbf"));
    assert!(!admitted(&entry, "evilghcr.io/komira-ai/kbf"));
    assert!(!admitted(&entry, "ghcr.io.evil.example/komira-ai/kbf"));
    assert!(!admitted(&entry, "komira-ai/kbf"), "that is docker.io");
}

/// Asserts that a registry with a port is a different registry from the same host without
/// one, that the default port 443 is the same as none, and that the port in an entry is
/// matched exactly. It catches a port that is dropped before the comparison.
#[test]
fn a_port_is_part_of_the_registry() {
    let with_port = ["registry.example:5000/team/"];
    assert!(admitted(&with_port, "registry.example:5000/team/app"));
    assert!(!admitted(&with_port, "registry.example/team/app"));
    assert!(!admitted(&with_port, "registry.example:5001/team/app"));
    assert!(!admitted(&with_port, "registry.example:50000/team/app"));
    let plain = ["registry.example/team/"];
    assert!(admitted(&plain, "registry.example/team/app"));
    assert!(admitted(&plain, "registry.example:443/team/app"));
    assert!(!admitted(&plain, "registry.example:5000/team/app"));
    assert!(admitted(
        &["registry.example:443/team"],
        "registry.example/team/app"
    ));
}

/// Asserts that a tag, a digest and both are ignored when matching, and that the port of a
/// registry is not read as a tag. It catches a name cut at the wrong colon.
#[test]
fn the_tag_and_the_digest_do_not_change_the_match() {
    let entry = ["registry.example:5000/team/"];
    for image in [
        "registry.example:5000/team/app",
        "registry.example:5000/team/app:1.2",
        "registry.example:5000/team/app:1.2@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    ] {
        assert_eq!(list(&entry).check(image), Ok(()), "{image}");
    }
    let other = "registry.example:5000/other/app:team";
    assert!(
        list(&entry).check(other).is_err(),
        "a tag named like the namespace is still a tag"
    );
}

/// Asserts the Docker Hub rules: the registry of a short name is `docker.io`, a
/// one-component name there is under `library/`, `docker.io/library/x` and `x` are the
/// same image, and a user's image is not under `library/`. It catches a short name that
/// slips past an entry for another registry, and an entry for `library/` that does not
/// cover the images people write without it.
#[test]
fn docker_hub_short_names_are_read_as_a_pull_reads_them() {
    let library = ["docker.io/library/"];
    assert!(admitted(&library, "docker.io/library/ubuntu"));
    assert!(admitted(&library, "ubuntu"));
    assert!(admitted(&library, "docker.io/ubuntu"));
    assert!(admitted(&library, "ubuntu:24.04"));
    assert!(!admitted(&library, "someone/tool"));
    assert!(!admitted(&library, "docker.io/someone/tool"));
    assert!(!admitted(&library, "ghcr.io/library/ubuntu"));
    // An entry for a user covers that user's images, short or with the host written.
    let someone = ["docker.io/someone"];
    assert!(admitted(&someone, "someone/tool"));
    assert!(admitted(&someone, "docker.io/someone/tool"));
    assert!(!admitted(&someone, "ubuntu"));
    // An entry cannot name a one-component image without `library/`: `x` is `library/x`.
    assert!(!admitted(&["docker.io/ubuntu"], "ubuntu"));
}

/// Asserts that the host is compared in lower case and the path is not folded: an upper
/// case registry is the same registry, and an upper case path is refused as unreadable
/// (an engine would refuse to pull it). It catches a case-sensitive host comparison, and
/// a path folded to lower case that would admit what the engine never pulls.
#[test]
fn case_is_folded_for_the_host_and_never_for_the_path() {
    assert!(admitted(&["GHCR.IO/komira-ai/"], "ghcr.io/komira-ai/kbf"));
    assert!(admitted(&["ghcr.io/komira-ai/"], "GHCR.io/komira-ai/kbf"));
    let entry = list(&["ghcr.io/komira-ai/"]);
    assert_eq!(
        entry.check(&format!("ghcr.io/Komira-AI/kbf{DIGEST}")),
        Err(Outside::Unreadable)
    );
    assert_eq!(
        entry.check(&format!("ghcr.io/komira-ai/KBF{DIGEST}")),
        Err(Outside::Unreadable)
    );
}

/// Asserts that `index.docker.io` is a registry of its own for the match, so an entry for
/// `docker.io` does not cover it and the reverse: the list fails closed when a name has two
/// spellings. It pins the js-oci reading so a change there is a change here.
#[test]
fn docker_hub_other_spelling_needs_its_own_entry() {
    assert!(!admitted(
        &["docker.io/library/"],
        "index.docker.io/library/ubuntu"
    ));
    assert!(admitted(
        &["index.docker.io/library/"],
        "index.docker.io/ubuntu"
    ));
    assert!(!admitted(
        &["index.docker.io/library/"],
        "docker.io/library/ubuntu"
    ));
}

/// Asserts that an image that is not a registry path is refused as unreadable when the
/// list is not empty: an empty component, `..`, a malformed registry. It catches a name
/// that an engine might read differently from the check.
#[test]
fn a_name_that_is_not_a_registry_path_is_outside() {
    let entry = list(&["ghcr.io/komira-ai/"]);
    for image in [
        "ghcr.io/komira-ai//kbf",
        "ghcr.io/komira-ai/../other/x",
        "ghcr.io/komira-ai/ kbf",
        "ghcr.io:notaport/komira-ai/x",
        "[::1/komira-ai/x",
        "",
    ] {
        assert_eq!(
            entry.check(&format!("{image}{DIGEST}")),
            Err(Outside::Unreadable),
            "{image:?}"
        );
    }
    assert_eq!(
        Outside::Unreadable.to_string(),
        "the image name is not a registry path"
    );
}

/// Asserts that several entries are any-of, and that the refusal names the registry and
/// the repository as a pull would see them (a short name shows as `docker.io:443/library/`).
/// It catches an entry list that is all-of, and an unhelpful refusal text.
#[test]
fn any_entry_admits_and_the_refusal_names_what_was_seen() {
    let entries = ["ghcr.io/komira-ai/", "registry.example:5000/team/"];
    assert!(admitted(&entries, "ghcr.io/komira-ai/kbf"));
    assert!(admitted(&entries, "registry.example:5000/team/app"));
    assert!(!admitted(&entries, "quay.io/x/y"));
    let said = list(&entries).check(&format!("ubuntu:24.04{DIGEST}"));
    assert_eq!(
        said,
        Err(Outside::NotListed(
            "docker.io:443/library/ubuntu".to_owned()
        ))
    );
    let text = said.unwrap_err().to_string();
    assert!(text.contains("--container-pull-allow"), "{text}");
    assert!(text.contains("docker.io:443/library/ubuntu"), "{text}");
}

/// Asserts that an entry that is not `HOST[:PORT]/NAMESPACE-PREFIX` is refused naming it:
/// a bare host (it would admit every account), a scheme, an empty or upper-case path, a
/// bad port, and empty text; and that more than the most entries are refused. It catches
/// an entry that parses to something wider than the operator wrote.
#[test]
fn bad_entries_are_refused() {
    for bad in [
        "ghcr.io",
        "ghcr.io/",
        "https://ghcr.io/komira-ai/",
        "ghcr.io//komira-ai",
        "ghcr.io/Komira-AI/",
        "ghcr.io:port/komira-ai/",
        "ghcr.io/komira ai/",
        "/komira-ai/",
        "",
    ] {
        let said = PullAllow::parse(&[bad.to_owned()]);
        assert_eq!(said, Err(EntryError::Entry(bad.to_owned())), "{bad:?}");
    }
    let good = vec!["ghcr.io/komira-ai/".to_owned(); MAX_ENTRIES];
    assert!(PullAllow::parse(&good).is_ok());
    let many = vec!["ghcr.io/komira-ai/".to_owned(); MAX_ENTRIES + 1];
    assert_eq!(PullAllow::parse(&many), Err(EntryError::Count));
    let mixed = ["ghcr.io/komira-ai/".to_owned(), "bare.example".to_owned()];
    assert_eq!(
        PullAllow::parse(&mixed),
        Err(EntryError::Entry("bare.example".to_owned())),
        "one bad entry refuses the list"
    );
}
