//! The flags of the node-local mount providers (`--mount-provider NAME=SOCKET`,
//! `--mount-provider-user NAME=USER` and `--mount-provider-wait SECS`, `docs/design/mount-providers.md`, section 4): checked when
//! the agent starts, before it says hello, and on Linux taken by a crun agent only (a macOS
//! agent takes them for its native runtime). The hello's `mount-provider.<name>` is
//! `jobs::crun_gpu_hello`'s.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use clap::Parser as _;
use js_agent::Cli;
use js_agent::lifecycle::{DEFAULT_PROVIDER_WAIT_SECS, Local, MountProviders, UserNames};

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
            "--mount-provider-user",
            "cache-2=4242",
            "--mount-provider-wait",
            "5",
        ],
    )
    .unwrap();
    assert_eq!(
        given.providers.sockets(),
        [
            ("tier".to_owned(), tier, 0),
            ("cache-2".to_owned(), cache, 4242)
        ]
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
        [("tier".to_owned(), PathBuf::from("tier.sock"), 0)]
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
            vec!["--mount-provider-user".to_owned(), "tier=7".to_owned()],
            "--mount-provider-user tier: names no --mount-provider".to_owned(),
        ),
        (
            vec![
                "--mount-provider".to_owned(),
                at("tier"),
                "--mount-provider-user".to_owned(),
                "tier=7".to_owned(),
                "--mount-provider-user".to_owned(),
                "tier=8".to_owned(),
            ],
            "--mount-provider-user tier: given twice".to_owned(),
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

/// Asserts that a provider's user given by name is the uid the passwd-format file gives that
/// name, read once for every name, and that a name the file lacks, a line whose uid is not a
/// number, and a file that cannot be read are refused naming the flag and the file; a uid is
/// taken without reading the file. Catches a name taken for root, a provider's socket judged
/// against another user's uid, and a missing file taken as no users.
#[test]
fn a_providers_user_by_name_is_its_uid_in_the_passwd_file() {
    let dir = Scratch::new("life-provider-users");
    let passwd = dir.write(
        "passwd",
        "root:x:0:0:root:/root:/bin/sh\nbroken\ntierstore:x:991:991::/var/lib/tierstore:/usr/sbin/nologin\nodd:x:many:1::/:/bin/sh\ncache:x:992:992::/:/bin/sh\n",
        0o644,
    );
    let sockets = [
        ("tier".to_owned(), dir.path("tier.sock")),
        ("cache".to_owned(), dir.path("cache.sock")),
    ];
    let users = [
        ("tier".to_owned(), "tierstore".to_owned()),
        ("cache".to_owned(), "cache".to_owned()),
    ];
    let given =
        MountProviders::from_flags(&sockets, &users, None, UserNames::Passwd(&passwd)).unwrap();
    assert_eq!(
        given.sockets(),
        [
            ("tier".to_owned(), dir.path("tier.sock"), 991),
            ("cache".to_owned(), dir.path("cache.sock"), 992)
        ]
    );
    let missing = dir.path("no-passwd");
    let by_uid = [("tier".to_owned(), "991".to_owned())];
    let taken =
        MountProviders::from_flags(&sockets, &by_uid, None, UserNames::Passwd(&missing)).unwrap();
    assert_eq!(taken.sockets()[0].2, 991);
    let refused = |user: &str, file: &Path| {
        let users = [("tier".to_owned(), user.to_owned())];
        MountProviders::from_flags(&sockets, &users, None, UserNames::Passwd(file)).err()
    };
    assert_eq!(
        refused("nobody-here", &passwd),
        Some(format!(
            "--mount-provider-user tier: no user \"nobody-here\" in {}",
            passwd.display()
        ))
    );
    assert_eq!(
        refused("odd", &passwd),
        Some(format!(
            "--mount-provider-user tier: no user \"odd\" in {}",
            passwd.display()
        ))
    );
    let unread = refused("tierstore", &missing).unwrap();
    assert!(
        unread.starts_with(&format!(
            "--mount-provider-user tier: {}: ",
            missing.display()
        )),
        "{unread}"
    );
}

/// Asserts that a provider's user given by name on a Mac is the uid the directory service
/// gives that name, asked once for each name and never for a uid, and that a name the service
/// does not know and a service that cannot be asked are refused naming the flag, by exact
/// equality. Catches a Mac's service account (absent from `/etc/passwd`) taken for root, and
/// an error of the service taken as no user.
#[test]
fn a_providers_user_by_name_on_a_mac_is_its_uid_in_the_directory_service() {
    use std::cell::RefCell;

    let dir = Scratch::new("life-provider-directory");
    let sockets = [
        ("tier".to_owned(), dir.path("tier.sock")),
        ("cache".to_owned(), dir.path("cache.sock")),
    ];
    let asked = RefCell::new(Vec::new());
    let directory = |name: &str| {
        asked.borrow_mut().push(name.to_owned());
        match name {
            "_tierstore" => Ok(Some(398)),
            "broken" => Err(std::io::Error::other("opendirectoryd is not running")),
            _ => Ok(None),
        }
    };
    let users = [
        ("tier".to_owned(), "_tierstore".to_owned()),
        ("cache".to_owned(), "7".to_owned()),
    ];
    let given =
        MountProviders::from_flags(&sockets, &users, None, UserNames::Directory(&directory))
            .unwrap();
    assert_eq!(
        given.sockets(),
        [
            ("tier".to_owned(), dir.path("tier.sock"), 398),
            ("cache".to_owned(), dir.path("cache.sock"), 7)
        ]
    );
    assert_eq!(*asked.borrow(), ["_tierstore"]);
    let refused = |user: &str| {
        let users = [("tier".to_owned(), user.to_owned())];
        MountProviders::from_flags(&sockets, &users, None, UserNames::Directory(&directory)).err()
    };
    assert_eq!(
        refused("_nobody"),
        Some("--mount-provider-user tier: no user \"_nobody\" in the directory service".to_owned())
    );
    assert_eq!(
        refused("broken"),
        Some(
            "--mount-provider-user tier: the directory service: opendirectoryd is not running"
                .to_owned()
        )
    );
}
