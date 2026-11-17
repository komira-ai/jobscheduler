//! The flags of the node-local mount providers (`--mount-provider NAME=SOCKET` and
//! `--mount-provider-wait SECS`, `docs/design/mount-providers.md`, section 4): checked when
//! the agent starts, before it says hello, and taken by a crun agent only. The hello's
//! `mount-provider.<name>` is `jobs::crun_gpu_hello`'s.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use clap::Parser as _;
use js_agent::Cli;
use js_agent::lifecycle::{DEFAULT_PROVIDER_WAIT_SECS, Local};

use crate::support::{Scratch, code, linux_host, run_scripted, welcome};

/// The job flags `extra`, checked as the agent checks them, over a private scratch root of
/// their own under `dir`.
fn local(dir: &Scratch, extra: &[&str]) -> Result<Local, String> {
    let scratch = dir.path("scratch");
    fs::create_dir_all(&scratch).unwrap();
    fs::set_permissions(&scratch, fs::Permissions::from_mode(0o700)).unwrap();
    let scratch = scratch.display().to_string();
    let mut argv = vec![
        "js-agent",
        "--server",
        "http://127.0.0.1:1",
        "--token-file",
        "/nonexistent/token",
        "--pool",
        "p",
        "--scratch-root",
        &scratch,
    ];
    argv.extend(extra.iter().copied());
    Local::from_cli(&Cli::try_parse_from(argv).unwrap(), 0)
}

/// Asserts that good provider flags reach the job flags as they were given, in order: each
/// name with its socket, the wait in seconds as milliseconds (`--mount-provider-wait`, or the
/// default 120 s), and a socket named by a bare file name (in the agent's own directory) taken;
/// and that no flag means no provider. Catches a flag that is read and dropped, two providers
/// merged or swapped, and a wait that is not the one given.
#[test]
fn good_mount_provider_flags_reach_the_job_flags_in_order() {
    let dir = Scratch::new("life-provider-flags-good");
    let tier = dir.path("tier.sock");
    let cache = dir.path("cache.sock");
    let (tier_flag, cache_flag) = (
        format!("tier={}", tier.display()),
        format!("cache-2={}", cache.display()),
    );
    let given = local(
        &dir,
        &[
            "--mount-provider",
            &tier_flag,
            "--mount-provider",
            &cache_flag,
            "--mount-provider-wait",
            "5",
        ],
    )
    .unwrap();
    assert_eq!(
        given.providers.sockets(),
        [("tier".to_owned(), tier), ("cache-2".to_owned(), cache)]
    );
    assert_eq!(given.providers.wait_ms(), 5000);
    assert!(!given.providers.is_empty());

    let by_default = local(&dir, &["--mount-provider", &tier_flag]).unwrap();
    assert_eq!(
        by_default.providers.wait_ms(),
        DEFAULT_PROVIDER_WAIT_SECS * 1000
    );
    assert_eq!(DEFAULT_PROVIDER_WAIT_SECS, 120);

    let bare = local(&dir, &["--mount-provider", "tier=tier.sock"]).unwrap();
    assert_eq!(
        bare.providers.sockets(),
        [("tier".to_owned(), PathBuf::from("tier.sock"))]
    );

    let none = local(&dir, &[]).unwrap();
    assert!(none.providers.is_empty());
    assert!(none.providers.sockets().is_empty());
}

/// Asserts that each wrong provider flag is refused naming the flag, by the full text: a name
/// the wire would refuse (capitals, a leading digit, too long), a name twice, a socket whose
/// directory is not there, a wait with no provider, and a wait outside 1 to 900 seconds
/// (either end taken). Catches a provider found wrong only when its first job arrives.
#[test]
fn a_wrong_mount_provider_flag_is_refused_naming_the_flag() {
    let dir = Scratch::new("life-provider-flags-bad");
    let base = dir.path("sockets");
    fs::create_dir(&base).unwrap();
    let at = |name: &str| format!("{name}={}/{name}.sock", base.display());
    let missing = format!("tier={}/missing/tier.sock", base.display());
    let long = "a".repeat(33);
    let cases: Vec<(Vec<String>, String)> = vec![
        (
            vec!["--mount-provider".to_owned(), at("Tier")],
            "--mount-provider \"Tier\": not a usable provider name".to_owned(),
        ),
        (
            vec!["--mount-provider".to_owned(), at("1tier")],
            "--mount-provider \"1tier\": not a usable provider name".to_owned(),
        ),
        (
            vec!["--mount-provider".to_owned(), at(&long)],
            format!("--mount-provider {long:?}: not a usable provider name"),
        ),
        (
            vec![
                "--mount-provider".to_owned(),
                at("tier"),
                "--mount-provider".to_owned(),
                at("tier"),
            ],
            "--mount-provider tier: given twice".to_owned(),
        ),
        (
            vec!["--mount-provider".to_owned(), missing],
            format!(
                "--mount-provider tier: {}/missing is not an existing directory",
                base.display()
            ),
        ),
        (
            vec!["--mount-provider-wait".to_owned(), "5".to_owned()],
            "--mount-provider-wait: needs --mount-provider".to_owned(),
        ),
        (
            vec![
                "--mount-provider".to_owned(),
                at("tier"),
                "--mount-provider-wait".to_owned(),
                "0".to_owned(),
            ],
            "--mount-provider-wait: 0 is not from 1 to 900 seconds".to_owned(),
        ),
        (
            vec![
                "--mount-provider".to_owned(),
                at("tier"),
                "--mount-provider-wait".to_owned(),
                "901".to_owned(),
            ],
            "--mount-provider-wait: 901 is not from 1 to 900 seconds".to_owned(),
        ),
    ];
    for (extra, text) in cases {
        let words: Vec<&str> = extra.iter().map(String::as_str).collect();
        let refused = local(&dir, &words).err();
        assert_eq!(refused.as_deref(), Some(text.as_str()), "{extra:?}");
    }
    for edge in ["1", "900"] {
        let socket = at("tier");
        let all = [
            "--mount-provider",
            socket.as_str(),
            "--mount-provider-wait",
            edge,
        ];
        assert!(local(&dir, &all).is_ok(), "{edge}");
    }
}

/// Asserts that the agent ends with status 2 before it registers when an agent that runs no
/// job (`--no-jobs`) is given a provider flag, naming the flag, and when an operator tries to
/// claim a provider's capability by hand. Catches provider flags taken by an agent that has no
/// runtime to give a job a tree with, and a host that claims a provider it was not given.
#[test]
fn provider_flags_are_refused_without_jobs_and_the_capability_cannot_be_set_by_hand() {
    let dir = Scratch::new("life-provider-flags-refused");
    let given = format!("tier={}", dir.path("tier.sock").display());
    let cases: [(&[&str], &str); 3] = [
        (
            &["--no-jobs", "--mount-provider", &given],
            "--mount-provider: not with --no-jobs (an agent that runs no job)",
        ),
        (
            &["--no-jobs", "--mount-provider-wait", "5"],
            "--mount-provider-wait: not with --no-jobs (an agent that runs no job)",
        ),
        (
            &["--capability", "mount-provider.tier=true"],
            "--capability: mount-provider.tier is detected and cannot be set",
        ),
    ];
    for (extra, text) in cases {
        let (ran, sent, _) = run_scripted(extra, &linux_host(), vec![welcome(1)], 1);
        assert_eq!(ran.status(), code(2), "{extra:?}: {}", ran.err);
        assert!(ran.err.contains(text), "{extra:?}: {}", ran.err);
        assert!(sent.borrow().is_empty(), "{extra:?}");
    }
}
