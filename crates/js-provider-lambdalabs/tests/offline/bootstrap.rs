//! The instance bootstrap: the rendered document, every refusal of an input, and the
//! properties of the script that no input can change (no token, a gated service, a
//! checked download).

use js_provider_lambdalabs::{Bootstrap, BootstrapError, Fault};

const GOLDEN: &str = include_str!("../fixtures/bootstrap_user_data.sh");

const URL: &str =
    "https://github.com/example-org/jobscheduler/releases/download/v0.1.0/js-agent-x86_64-linux";
const SHA: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn good() -> Bootstrap<'static> {
    Bootstrap {
        agent_url: URL,
        agent_sha256: SHA,
        server_url: "https://lease.example.org:8443",
        pool: "lambda-gpu",
        ca_cert: "/etc/js-agent/ca.pem",
        token_file: "/etc/js-agent/token",
        state_root: "/var/lib/js-agent/state",
        scratch_root: "/mnt/scratch/js-agent",
        container_bin: "/usr/bin/docker",
    }
}

/// `good()` with the input `field` set to `value`.
fn with<'a>(field: &str, value: &'a str) -> Bootstrap<'a> {
    let mut b: Bootstrap<'a> = good();
    match field {
        "agent_url" => b.agent_url = value,
        "agent_sha256" => b.agent_sha256 = value,
        "server_url" => b.server_url = value,
        "pool" => b.pool = value,
        "ca_cert" => b.ca_cert = value,
        "token_file" => b.token_file = value,
        "state_root" => b.state_root = value,
        "scratch_root" => b.scratch_root = value,
        "container_bin" => b.container_bin = value,
        other => panic!("no input {other}"),
    }
    b
}

fn field(field: &'static str, fault: Fault) -> BootstrapError {
    BootstrapError::Field { field, fault }
}

fn render(b: &Bootstrap<'_>) -> String {
    b.render().unwrap().as_str().to_owned()
}

#[test]
fn a_full_document_renders_to_the_golden_file() {
    assert_eq!(render(&good()), GOLDEN);
}

#[test]
fn rendering_is_deterministic_and_reads_nothing_else() {
    assert_eq!(good().render().unwrap(), good().render().unwrap());
}

#[test]
fn each_input_lands_in_the_document_and_a_different_input_changes_it() {
    let other = Bootstrap {
        pool: "other-pool",
        container_bin: "/usr/local/bin/podman",
        ..good()
    };
    let text = render(&other);
    assert!(text.contains("POOL='other-pool'\n"));
    assert!(text.contains("CONTAINER_BIN='/usr/local/bin/podman'\n"));
    assert_ne!(text, GOLDEN);
}

#[test]
fn a_url_that_is_not_https_with_a_host_is_refused() {
    for name in ["agent_url", "server_url"] {
        let cases = [
            ("http://host.example/a", Fault::NotHttps),
            ("HTTPS://host.example/a", Fault::NotHttps),
            ("ftp://host.example/a", Fault::NotHttps),
            ("host.example/a", Fault::NotHttps),
            ("https://", Fault::NoHost),
            ("https:///a", Fault::NoHost),
            ("https://:443/a", Fault::NoHost),
            ("", Fault::Empty),
        ];
        for (url, fault) in cases {
            let b = with(name, url);
            assert_eq!(b.render(), Err(field(name, fault)), "{name} {url:?}");
        }
    }
}

#[test]
fn nothing_a_shell_or_systemd_reads_as_syntax_is_accepted() {
    let bad = [
        "https://user@host.example/a",
        "https://host.example/a?x=1",
        "https://host.example/a#frag",
        "https://host.example/a b",
        "https://host.example/$(id)",
        "https://host.example/`id`",
        "https://host.example/a;b",
        "https://host.example/a|b",
        "https://host.example/a&b",
        "https://host.example/a'b",
        "https://host.example/a\"b",
        "https://host.example/a\\b",
        "https://host.example/a%20b",
        "https://host.example/a\nb",
        "https://host.example/a@b",
        "https://hóst.example/a",
    ];
    for name in ["agent_url", "server_url"] {
        for url in bad {
            let b = with(name, url);
            assert_eq!(
                b.render(),
                Err(field(name, Fault::Character)),
                "{name} {url:?}"
            );
        }
    }
    let bad_paths = [
        "/a b", "/a;b", "/a$b", "/a`b`", "/a'b", "/a\"b", "/a\\b", "/a|b", "/a&b", "/a%b", "/a\nb",
        "/a:b", "/a*b", "/a=b", "/a(b)",
    ];
    for name in [
        "ca_cert",
        "token_file",
        "state_root",
        "scratch_root",
        "container_bin",
    ] {
        for path in bad_paths {
            let b = with(name, path);
            assert_eq!(
                b.render(),
                Err(field(name, Fault::Character)),
                "{name} {path:?}"
            );
        }
    }
    for pool in ["a/b", "a b", "a;b", "a:b", "a$b"] {
        assert_eq!(
            with("pool", pool).render(),
            Err(field("pool", Fault::Character)),
            "{pool:?}"
        );
    }
}

#[test]
fn a_path_must_be_absolute_and_plain() {
    let cases = [
        ("etc/js-agent", Fault::NotAbsolute),
        ("./token", Fault::NotAbsolute),
        ("token", Fault::NotAbsolute),
        ("/", Fault::Component),
        ("/a/", Fault::Component),
        ("//a", Fault::Component),
        ("/a//b", Fault::Component),
        ("/a/./b", Fault::Component),
        ("/a/../b", Fault::Component),
        ("/..", Fault::Component),
        ("", Fault::Empty),
    ];
    for name in [
        "ca_cert",
        "token_file",
        "state_root",
        "scratch_root",
        "container_bin",
    ] {
        for (path, fault) in cases {
            let b = with(name, path);
            assert_eq!(b.render(), Err(field(name, fault)), "{name} {path:?}");
        }
    }
}

#[test]
fn a_digest_must_be_64_lowercase_hex_digits() {
    let upper = SHA.to_uppercase();
    let non_hex = "g".repeat(64);
    let long_sha = format!("{SHA}0");
    let bad = [
        "",
        "0123",
        &SHA[..63],
        long_sha.as_str(),
        upper.as_str(),
        non_hex.as_str(),
        "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    ];
    for digest in bad {
        assert_eq!(
            with("agent_sha256", digest).render(),
            Err(field("agent_sha256", Fault::Digest)),
            "{digest:?}"
        );
    }
}

#[test]
fn a_value_over_256_bytes_is_refused_and_256_is_not() {
    let head = "https://host.example/";
    let fits = format!(
        "{head}{}",
        "ab-".repeat(80).get(..256 - head.len()).unwrap()
    );
    assert_eq!(fits.len(), 256);
    assert!(with("agent_url", &fits).render().is_ok());
    let long = format!("{fits}-");
    assert_eq!(
        with("agent_url", &long).render(),
        Err(field("agent_url", Fault::TooLong))
    );
    let long_pool = "p-".repeat(129);
    assert_eq!(
        with("pool", &long_pool).render(),
        Err(field("pool", Fault::TooLong))
    );
}

#[test]
fn a_token_shaped_input_is_refused_wherever_it_is_put() {
    // A join token is printable ASCII of at least 32 bytes.
    let token = "Zq3RkT8vYw1LmN6pXc2HdB9sJf4GaE7uVt5oKi0y";
    let in_url = format!("https://lease.example.org/{token}");
    let in_path = format!("/etc/js-agent/{token}");
    for name in ["agent_url", "server_url"] {
        assert_eq!(
            with(name, &in_url).render(),
            Err(field(name, Fault::TokenShaped)),
            "{name}"
        );
        // Tokens with other characters are refused as characters.
        let query = format!("https://lease.example.org/?token={token}");
        assert_eq!(
            with(name, &query).render(),
            Err(field(name, Fault::Character))
        );
        let user = format!("https://{token}@lease.example.org/");
        assert_eq!(
            with(name, &user).render(),
            Err(field(name, Fault::Character))
        );
    }
    for name in [
        "ca_cert",
        "token_file",
        "state_root",
        "scratch_root",
        "container_bin",
    ] {
        assert_eq!(
            with(name, &in_path).render(),
            Err(field(name, Fault::TokenShaped)),
            "{name}"
        );
    }
    assert_eq!(
        with("pool", token).render(),
        Err(field("pool", Fault::TokenShaped))
    );
    // A token that is not a path or a URL is refused as that.
    assert_eq!(
        with("token_file", token).render(),
        Err(field("token_file", Fault::TokenShaped))
    );
    assert_eq!(
        with("agent_sha256", token).render(),
        Err(field("agent_sha256", Fault::Digest))
    );
    // Base64 text, with its '+', '/' and '='.
    assert!(
        with("pool", "Zq3RkT8vYw1L+mN6pXc2HdB9sJf4GaE7/uVt5oKi0y==")
            .render()
            .is_err()
    );
    // 31 letters and digits are a name, 32 are not.
    let name31 = "a".repeat(31);
    let name32 = "a".repeat(32);
    assert!(with("pool", &name31).render().is_ok());
    assert!(with("pool", &name32).render().is_err());
    // Separators break a run: a long name made of short words is a name.
    assert!(with("pool", &format!("{name31}-{name31}")).render().is_ok());
}

#[test]
fn the_document_holds_no_token_shaped_run_except_the_digest() {
    let text = render(&good());
    let runs: Vec<&str> = text
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|part| part.len() >= 32)
        .collect();
    assert_eq!(runs, [SHA]);
    for secret_word in ["Bearer", "ssh-rsa", "PRIVATE KEY", "password"] {
        assert!(!text.contains(secret_word), "{secret_word}");
    }
}

#[test]
fn paths_that_overlap_are_refused() {
    let cases = [
        // (state_root, scratch_root, token_file, first, second)
        ("/data/a", "/data/a", "/etc/t", "state_root", "scratch_root"),
        (
            "/data/a/b",
            "/data/a",
            "/etc/t",
            "state_root",
            "scratch_root",
        ),
        (
            "/data/a",
            "/data/a/b",
            "/etc/t",
            "state_root",
            "scratch_root",
        ),
        ("/data/a", "/data/b", "/data/a", "token_file", "state_root"),
        (
            "/data/a",
            "/data/b",
            "/data/a/t",
            "token_file",
            "state_root",
        ),
        (
            "/data/a",
            "/data/b",
            "/data/b",
            "token_file",
            "scratch_root",
        ),
        (
            "/data/a",
            "/data/b",
            "/data/b/x/t",
            "token_file",
            "scratch_root",
        ),
    ];
    for (state_root, scratch_root, token_file, first, second) in cases {
        let b = Bootstrap {
            state_root,
            scratch_root,
            token_file,
            ..good()
        };
        assert_eq!(
            b.render(),
            Err(BootstrapError::Overlap { first, second }),
            "{state_root} {scratch_root} {token_file}"
        );
    }
}

#[test]
fn paths_that_only_share_a_text_prefix_do_not_overlap() {
    let b = Bootstrap {
        state_root: "/data/a",
        scratch_root: "/data/a2",
        token_file: "/data/a3/token",
        ..good()
    };
    assert!(b.render().is_ok());
    let b = Bootstrap {
        state_root: "/data/a2",
        scratch_root: "/data/a",
        token_file: "/data/ab",
        ..good()
    };
    assert!(b.render().is_ok());
}

#[test]
fn the_service_starts_only_when_the_token_file_exists() {
    let text = render(&good());
    let service = text.split("[Service]").next().unwrap();
    let unit = service.rsplit("[Unit]").next().unwrap();
    assert!(unit.contains("ConditionPathExists=$TOKEN_FILE\n"));
    assert!(text.contains("TOKEN_FILE='/etc/js-agent/token'\n"));
    // A path unit starts it when the file appears, and only the path unit is enabled.
    assert!(text.contains("[Path]\nPathExists=$TOKEN_FILE\nUnit=js-agent.service\n"));
    assert!(text.contains("systemctl enable --now js-agent.path\n"));
    assert!(!text.contains("enable --now js-agent.service"));
    assert!(!text.contains("start js-agent"));
    // The unit has no way to make a token: it never writes the token file.
    assert!(!text.contains(">\"$TOKEN_FILE\""));
    assert!(!text.contains("> \"$TOKEN_FILE\""));
    assert!(!text.contains(">$TOKEN_FILE"));
    assert!(!text.contains("tee"));
}

#[test]
fn the_unit_runs_the_agent_with_every_flag_the_inputs_name() {
    let text = render(&good());
    let exec = text
        .lines()
        .find(|line| line.starts_with("ExecStart="))
        .unwrap();
    assert_eq!(
        exec,
        "ExecStart=$AGENT_BIN --server $SERVER_URL --pool $POOL --ca-cert $CA_CERT \
         --token-file $TOKEN_FILE --container-bin $CONTAINER_BIN --state-root $STATE_ROOT \
         --scratch-root $SCRATCH_ROOT"
    );
    for assignment in [
        "SERVER_URL='https://lease.example.org:8443'",
        "POOL='lambda-gpu'",
        "CA_CERT='/etc/js-agent/ca.pem'",
        "TOKEN_FILE='/etc/js-agent/token'",
        "CONTAINER_BIN='/usr/bin/docker'",
        "STATE_ROOT='/var/lib/js-agent/state'",
        "SCRATCH_ROOT='/mnt/scratch/js-agent'",
    ] {
        assert!(text.contains(assignment), "{assignment}");
    }
}

#[test]
fn the_download_is_checked_before_it_is_installed() {
    let text = render(&good());
    let at = |needle: &str| text.find(needle).unwrap_or_else(|| panic!("no {needle:?}"));
    assert!(at("--proto '=https'") < at("sha256 mismatch"));
    assert!(at("sha256 mismatch") < at("mv -f \"$tmp\" \"$AGENT_BIN\""));
    assert!(text.contains("AGENT_SHA256='0123456789abcdef"));
    assert!(text.contains("nothing was installed"));
    // The checks of the image come before the download.
    for check in [
        "nvidia-smi >/dev/null",
        "nvidia-ctk",
        "info 2>&1",
        "no nvidia runtime",
    ] {
        assert!(at(check) < at("curl --fail"), "{check}");
    }
}

#[test]
fn the_steps_are_guarded_so_a_second_run_changes_nothing() {
    let text = render(&good());
    for guard in [
        "is already the expected build",
        "mkdir -p -m 0700",
        "cmp -s \"$new\" \"$1\"",
        "set -euo pipefail",
    ] {
        assert!(text.contains(guard), "{guard}");
    }
    // Directories that exist are checked, never changed.
    assert!(!text.contains("chmod 0700"));
    assert!(!text.contains("chown"));
}

#[test]
fn user_data_debug_shows_a_size_and_a_digest_and_never_the_text() {
    let data = good().render().unwrap();
    let shown = format!("{data:?}");
    assert_eq!(
        shown,
        format!("UserData({} bytes, sha256 {})", GOLDEN.len(), data.digest())
    );
    assert_eq!(data.digest().len(), 16);
    assert!(data.digest().bytes().all(|b| b.is_ascii_hexdigit()));
    assert!(!shown.contains("nvidia"));
    let other = Bootstrap {
        pool: "other",
        ..good()
    }
    .render()
    .unwrap();
    assert_ne!(other.digest(), data.digest());
}

#[test]
fn every_refusal_has_a_message_that_names_the_input() {
    let b = with("agent_url", "http://host.example/a");
    assert_eq!(
        b.render().unwrap_err().to_string(),
        "agent_url: must be an https:// URL"
    );
    let faults = [
        (Fault::Empty, "is empty"),
        (Fault::TooLong, "is longer than 256 bytes"),
        (Fault::Character, "holds a character that is not allowed"),
        (
            Fault::TokenShaped,
            "holds a run of 32 or more letters and digits, which looks like a token",
        ),
        (Fault::NotHttps, "must be an https:// URL"),
        (Fault::NoHost, "has no host"),
        (Fault::NotAbsolute, "must be an absolute path"),
        (Fault::Component, "has an empty, '.' or '..' component"),
        (Fault::Digest, "must be 64 lowercase hex digits"),
    ];
    for (fault, text) in faults {
        assert_eq!(fault.to_string(), text);
    }
    let overlap = BootstrapError::Overlap {
        first: "token_file",
        second: "state_root",
    };
    assert_eq!(
        overlap.to_string(),
        "token_file and state_root are the same path or one lies in the other"
    );
}
