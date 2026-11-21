//! The package built by Apple's `pkgbuild` and `productbuild` from stand-in files, and read
//! back by `pkg check-macos` with `pkgutil` and `lsbom`. Runs on a Mac only.
#![cfg(target_os = "macos")]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use clap::Parser;
use js_testkit::dir::TestDir;
use xtask::macpkg::{CheckArgs, Error, IDENTIFIER, StageArgs, check, stage};

const VERSION: &str = "1.2.3";

#[derive(Parser)]
struct Stage {
    #[command(flatten)]
    args: StageArgs,
}

#[derive(Parser)]
struct Check {
    #[command(flatten)]
    args: CheckArgs,
}

fn staged(name: &str) -> (TestDir, StageArgs) {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), name);
    let at = dir.path().to_path_buf();
    let bin = at.join("bin");
    std::fs::create_dir(&bin).unwrap();
    for program in ["js-agent", "js-mac-session", "js-mac-provision"] {
        std::fs::write(bin.join(program), format!("#!/bin/sh\n# {program}\n")).unwrap();
    }
    for file in ["LICENSE", "agent.plist", "session.plist", "pf-pool.plist"] {
        std::fs::write(at.join(file), format!("{file}\n")).unwrap();
    }
    let path = |leaf: &str| at.join(leaf).to_str().unwrap().to_owned();
    let args = Stage::parse_from([
        "stage",
        "--version",
        VERSION,
        "--bin-dir",
        &path("bin"),
        "--license",
        &path("LICENSE"),
        "--agent-plist",
        &path("agent.plist"),
        "--session-plist",
        &path("session.plist"),
        "--pf-pool-plist",
        &path("pf-pool.plist"),
        "--out",
        &path("out"),
    ])
    .args;
    (dir, args)
}

fn tool(program: &str, args: &[&str]) {
    let out = Command::new(program).args(args).output().unwrap();
    assert!(
        out.status.success(),
        "{program} {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Builds the product archive from a staged directory, as the workflow does.
fn build(out: &Path, product: &Path) {
    let component = out.join("component.pkg");
    tool(
        "pkgbuild",
        &[
            "--root",
            out.join("root").to_str().unwrap(),
            "--identifier",
            IDENTIFIER,
            "--version",
            VERSION,
            "--install-location",
            "/",
            "--ownership",
            "recommended",
            component.to_str().unwrap(),
        ],
    );
    tool(
        "productbuild",
        &[
            "--package",
            component.to_str().unwrap(),
            product.to_str().unwrap(),
        ],
    );
}

fn check_args(product: &Path) -> CheckArgs {
    Check::parse_from([
        "check",
        "--pkg",
        product.to_str().unwrap(),
        "--version",
        VERSION,
    ])
    .args
}

#[test]
fn the_package_pkgbuild_makes_from_the_staged_directory_is_the_package() {
    let (dir, args) = staged("macpkg-real-good");
    stage(&args).unwrap();
    let product = dir.path().join("js-agent.pkg");
    build(&args.out, &product);
    let line = check(&check_args(&product)).unwrap();
    assert!(line.contains("1.2.3"), "{line}");
}

#[test]
fn a_file_added_to_the_payload_after_staging_is_refused() {
    let (dir, args) = staged("macpkg-real-extra");
    stage(&args).unwrap();
    let extra = args
        .out
        .join("root/Library/Application Support/js-agent/extra");
    std::fs::write(&extra, "x").unwrap();
    std::fs::set_permissions(&extra, std::fs::Permissions::from_mode(0o644)).unwrap();
    let product = dir.path().join("js-agent.pkg");
    build(&args.out, &product);
    match check(&check_args(&product)) {
        Err(Error::Refused(found)) => {
            assert_eq!(
                found,
                ["not part of the package: Library/Application Support/js-agent/extra"]
            );
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_package_built_with_a_script_is_refused() {
    let (dir, args) = staged("macpkg-real-script");
    stage(&args).unwrap();
    let scripts = args.out.join("scripts");
    std::fs::create_dir(&scripts).unwrap();
    std::fs::write(scripts.join("postinstall"), "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(
        scripts.join("postinstall"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let product = dir.path().join("js-agent.pkg");
    let component = args.out.join("component.pkg");
    tool(
        "pkgbuild",
        &[
            "--root",
            args.out.join("root").to_str().unwrap(),
            "--scripts",
            scripts.to_str().unwrap(),
            "--identifier",
            IDENTIFIER,
            "--version",
            VERSION,
            "--install-location",
            "/",
            "--ownership",
            "recommended",
            component.to_str().unwrap(),
        ],
    );
    tool(
        "productbuild",
        &[
            "--package",
            component.to_str().unwrap(),
            product.to_str().unwrap(),
        ],
    );
    match check(&check_args(&product)) {
        Err(Error::Refused(found)) => {
            assert_eq!(
                found,
                ["the package holds scripts, and must hold none: [\"postinstall\"]"]
            );
        }
        other => panic!("{other:?}"),
    }
}
