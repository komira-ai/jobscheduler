//! The registry entries: which spellings are accepted, and which names an entry covers.

use clap::Parser;
use js_agent::Cli;
use js_agent::bundle::{Registry, Settings};

use super::fake::FakeHost;
use crate::support::platform;

/// The registries a farm host's flags give, or the refusal.
fn parsed(public: &[&str], private: &[&str]) -> Result<Vec<Registry>, String> {
    let mut args = vec![
        "js-agent",
        "--server",
        "http://127.0.0.1:1",
        "--pool",
        "macs",
        "--bundle-store",
        "/Volumes/work/bundles",
        "--scratch-root",
        "/Volumes/work/scratch",
        "--bundle-budget-gib",
        "1",
        "--bundle-trust",
        "digest",
    ];
    for entry in public {
        args.extend(["--bundle-registry-allow", entry]);
    }
    for entry in private {
        args.extend(["--bundle-registry-allow-private", entry]);
    }
    let cli = Cli::try_parse_from(args).unwrap();
    let host = FakeHost::new();
    Settings::from_cli(&cli, &platform("macos", "aarch64"), &*host)
        .map(|settings| settings.unwrap().registries)
}

/// Accepted spellings: a host and a namespace, with or without the trailing slash, a port,
/// nested namespaces, and a host in any case (kept lower case). It catches a spelling the
/// design gives (`ghcr.io/komira-ai/`) that is refused.
#[test]
fn a_host_and_a_namespace_are_accepted() {
    let cases = [
        ("ghcr.io/komira-ai/", "ghcr.io", "komira-ai"),
        ("ghcr.io/komira-ai", "ghcr.io", "komira-ai"),
        ("GHCR.io/komira-ai", "ghcr.io", "komira-ai"),
        ("localhost:5000/x/y", "localhost:5000", "x/y"),
        ("reg.example.com/a_b.c-d", "reg.example.com", "a_b.c-d"),
        ("my-reg.example.com/x", "my-reg.example.com", "x"),
    ];
    for (text, host, prefix) in cases {
        let found = parsed(&[text], &[]).unwrap();
        assert_eq!(
            (found[0].host.as_str(), found[0].prefix.as_str()),
            (host, prefix),
            "{text}"
        );
    }
}

/// Refused spellings: a bare host (it would admit every account's packages), an empty
/// namespace, an upper case or empty component, a port that is empty, too long or not
/// digits, and a host that is not a host. The private flag applies the same rules and names
/// its own flag. It catches a bare host taken for a namespace and a malformed entry that
/// would match nothing without telling the operator.
#[test]
fn a_bare_host_and_malformed_entries_are_refused() {
    let cases = [
        ("ghcr.io", "names a bare host"),
        (
            "ghcr.io/",
            "has a namespace that is empty or not lower case",
        ),
        (
            "ghcr.io//x",
            "has a namespace that is empty or not lower case",
        ),
        (
            "ghcr.io/Komira",
            "has a namespace that is empty or not lower case",
        ),
        (
            "ghcr.io/x y",
            "has a namespace that is empty or not lower case",
        ),
        ("ghcr.io:/x", "has a host that is not a registry host"),
        ("ghcr.io:123456/x", "has a host that is not a registry host"),
        ("ghcr.io:80a/x", "has a host that is not a registry host"),
        ("bad_host/x", "has a host that is not a registry host"),
        ("a..b/x", "has a host that is not a registry host"),
        ("/x", "has a host that is not a registry host"),
    ];
    for (text, said) in cases {
        let public = parsed(&[text], &[]).unwrap_err();
        assert!(
            public.starts_with(&format!("--bundle-registry-allow: {text:?} {said}")),
            "{public}"
        );
        let private = parsed(&[], &[text]).unwrap_err();
        assert!(
            private.starts_with("--bundle-registry-allow-private: "),
            "{private}"
        );
    }
}

/// Which names an entry covers: the same host (any case) and a repository that is the prefix
/// or below it, whatever the tag. It catches a prefix that matches half a component
/// (`komira-ai-evil` under `komira-ai`), a name on another host, a name with no host, a
/// tag read as part of the path and a host that is a prefix of another.
#[test]
fn an_entry_covers_its_namespace_and_nothing_beside_it() {
    let entry = parsed(&["ghcr.io/komira-ai"], &[]).unwrap().remove(0);
    let covered = [
        "ghcr.io/komira-ai/js-bundles/toolchain",
        "ghcr.io/komira-ai/js-bundles/toolchain:1.2",
        "GHCR.IO/komira-ai/x",
        "ghcr.io/komira-ai",
        "ghcr.io/komira-ai:latest",
    ];
    for name in covered {
        assert!(entry.covers(name), "{name}");
    }
    let outside = [
        "ghcr.io/komira-ai-evil/x",
        "ghcr.io/komira-ai:x/y",
        "ghcr.io/other/komira-ai/x",
        "ghcr.io/komira/x",
        "docker.io/komira-ai/x",
        "ghcr.io.evil.com/komira-ai/x",
        "toolchain",
        "komira-ai/toolchain",
        "ghcr.io",
    ];
    for name in outside {
        assert!(!entry.covers(name), "{name}");
    }
}
