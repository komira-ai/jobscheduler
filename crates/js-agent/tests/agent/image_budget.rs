//! `--image-budget-gib`: the flag is checked as `--bundle-budget-gib` is, belongs to the
//! crun runtime, and leaves the part of the budget the image store does not hold out of the
//! disk the hello offers. (Linux only: only Linux runs containers.)

use clap::Parser;
use js_agent::Cli;
use js_testkit::dir::TestDir;

use crate::support::{code, linux_host, run_scripted, welcome};

const GIB: u64 = 1 << 30;

/// The flags of a crun agent that pass every check before the image budget's.
const CRUN: [&str; 8] = [
    "--runtime",
    "crun",
    "--job-user",
    "jsjob",
    "--state-root",
    "/s",
    "--scratch-root",
    "/r",
];

/// Runs the agent with `extra`; returns (status, stderr, whether it sent anything).
fn run_agent(extra: &[&str]) -> (String, String, bool) {
    let (ran, sent, _) = run_scripted(extra, &linux_host(), vec![welcome(1)], 1);
    let sent = !sent.borrow().is_empty();
    (ran.status(), ran.err, sent)
}

/// Proves a budget of 0 stops a crun agent with status 2 before anything is sent, naming
/// the flag, as `--bundle-budget-gib 0` stops a Mac agent; and that the flag with a runtime
/// other than crun is refused by name. Catches a 0 taken as "evict everything", and a flag
/// accepted and ignored by a runtime that has no image store.
#[test]
fn a_zero_budget_and_a_budget_without_crun_are_refused() {
    let zero = [&CRUN[..], &["--image-budget-gib", "0"][..]].concat();
    let (status, err, sent) = run_agent(&zero);
    assert_eq!(status, code(2), "{err}");
    assert!(
        err.contains("js-agent: --image-budget-gib: above 0 (leave it out for no budget)"),
        "{err}"
    );
    assert!(!sent);
    let engine = [
        "--runtime",
        "docker",
        "--container-bin",
        "/usr/bin/docker",
        "--allow-container-engine",
        "--image-budget-gib",
        "4",
    ];
    let (status, err, sent) = run_agent(&engine);
    assert_eq!(status, code(2), "{err}");
    assert!(
        err.contains("js-agent: --image-budget-gib: needs --runtime crun"),
        "{err}"
    );
    assert!(!sent);
}

/// Proves a value that is not a whole number of GiB is refused by the parser as it is for
/// `--bundle-budget-gib`: the same kind of error, naming the flag.
#[test]
fn a_budget_that_is_not_a_number_is_refused_as_the_bundle_budget_is() {
    let parse = |flag: &str, value: &str| {
        let args = [
            "js-agent",
            "--server",
            "http://127.0.0.1:1",
            "--pool",
            "p",
            flag,
            value,
        ];
        Cli::try_parse_from(args).map(|_| ()).unwrap_err()
    };
    for value in ["abc", "1.5", "", "-1"] {
        let image = parse("--image-budget-gib", value);
        let bundle = parse("--bundle-budget-gib", value);
        assert_eq!(image.kind(), bundle.kind(), "{value:?}");
        let said = image.to_string();
        // A negative value is read as an unknown flag, for both alike.
        let named = value == "-1" || said.contains("--image-budget-gib <GIB>");
        assert!(named, "{said}");
    }
    let given = Cli::try_parse_from([
        "js-agent",
        "--server",
        "http://127.0.0.1:1",
        "--pool",
        "p",
        "--image-budget-gib",
        "7",
    ])
    .unwrap();
    assert_eq!(given.image_budget_gib, Some(7));
}

/// The hello's `disk-gib` of a Linux host whose scratch root's volume has 100 GiB free, with
/// `extra` flags, and its warnings.
fn offered(root: &std::path::Path, extra: &[&str]) -> (String, Vec<String>) {
    let shown = root.display().to_string();
    let mut args = vec!["js-agent", "--server", "http://127.0.0.1:1", "--pool", "p"];
    args.extend(["--scratch-root", &shown]);
    args.extend(extra);
    let cli = Cli::try_parse_from(args).unwrap();
    let mut host = linux_host();
    let row = format!(
        "Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/x 9 9 {} 11% /\n",
        100 * GIB / 1024
    );
    host.commands = host
        .commands
        .with(&format!("/bin/df -P -k {shown}"), Ok(&row));
    let offer = js_agent::offer(&cli, &host.platform, &host.commands, &host.files, "0", None)
        .expect("an offer");
    (offer.hello.capabilities["disk-gib"].clone(), offer.warnings)
}

/// Proves the hello offers the free disk less the part of the budget the image store
/// (`oci/oci-images` beside the scratch root) does not hold: the whole budget for an empty
/// or missing store, less what a held image takes, all of it with a warning for a store that
/// cannot be read, and the free disk as it was without the flag. Catches a budget that the
/// hello offers to jobs, and images counted twice.
#[test]
fn the_hello_leaves_out_the_unused_image_budget() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "image-budget-hello");
    let root = dir.path().join("scratch");
    std::fs::create_dir_all(&root).unwrap();
    assert_eq!(offered(&root, &[]), ("100".to_owned(), Vec::new()));
    let budget = ["--image-budget-gib", "20"];
    assert_eq!(offered(&root, &budget), ("80".to_owned(), Vec::new()));
    let store = dir.path().join("oci").join("oci-images");
    let rootfs = store.join("a".repeat(64)).join("rootfs");
    std::fs::create_dir_all(&rootfs).unwrap();
    std::fs::File::create(rootfs.join("big"))
        .unwrap()
        .set_len(5 * GIB)
        .unwrap();
    // A little less than 15 GiB is kept back (the image takes a little more than 5); the
    // offer rounds down.
    assert_eq!(offered(&root, &budget), ("85".to_owned(), Vec::new()));
    std::fs::remove_dir_all(&store).unwrap();
    std::fs::write(&store, b"").unwrap();
    let (gib, warnings) = offered(&root, &budget);
    assert_eq!(gib, "80");
    assert_eq!(
        warnings,
        [format!(
            "--image-budget-gib: cannot read the bytes the image store {} holds (Not a \
             directory (os error 20)); its whole budget is kept back from jobs",
            store.display()
        )]
    );
}
