//! `cargo xtask image-recipe` on the repository's own pins and files, and on copies of them with
//! one thing changed: what each platform's recipe holds, that two runs give the same bytes, that
//! secret material is refused before anything is written, that the units the image enables and
//! the user-data reader agree with the recipe, that the crun host setup is the runbook's, and
//! that the seed and the boot check's user data follow the recipe.

use std::path::{Path, PathBuf};
use std::process::Command;

use js_testkit::dir::TestDir;
use serde_json::Value;
use xtask::image::{
    self, ARCHES, Args, BAKED, CRUN_RUNBOOK, CRUN_STEPS, ENABLED_UNITS, PINS, READER_UNIT,
    RECIPE_PATH, USER_DATA_PATHS,
};

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");

/// The repository root.
fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// A scratch directory of this test binary.
fn scratch(name: &str) -> TestDir {
    TestDir::under(env!("CARGO_TARGET_TMPDIR"), name)
}

/// Runs the verb with `root` as the repository and `out` as the output.
fn run(root: &Path, out: &Path) -> Result<String, image::Error> {
    image::run(&Args {
        out: out.to_owned(),
        root: root.to_owned(),
        pins: PathBuf::from(PINS),
    })
}

/// The text of `rel` under `dir`.
fn text(dir: &Path, rel: &str) -> String {
    std::fs::read_to_string(dir.join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
}

/// The recipe of `platform` under `out`, parsed.
fn recipe(out: &Path, platform: &str) -> Value {
    serde_json::from_str(&text(out, &format!("{platform}/recipe.json"))).expect("recipe is JSON")
}

/// The recipes of the repository, written into a fresh directory.
fn repo_recipes(name: &str) -> (TestDir, Vec<Value>) {
    let out = scratch(name);
    run(&repo(), &out).expect("image-recipe");
    let recipes = ARCHES.iter().map(|a| recipe(&out, a.platform())).collect();
    (out, recipes)
}

/// The strings of the JSON array `value`.
fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .expect("array")
        .iter()
        .map(|v| v.as_str().expect("string").to_owned())
        .collect()
}

/// Whether `s` is 64 lowercase hex digits.
fn is_sha256(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// A copy of the repository's inputs of the verb (the pins, the steps and the baked files).
fn copied_root(name: &str) -> TestDir {
    let root = scratch(name);
    let sources = BAKED.iter().map(|b| b.source).chain([PINS, CRUN_STEPS]);
    for rel in sources {
        let to = root.join(rel);
        std::fs::create_dir_all(to.parent().expect("parent")).expect("directory");
        std::fs::copy(repo().join(rel), to).expect("copy");
    }
    root
}

/// Asserts that a recipe is written for linux-x86_64 and linux-arm64, each for its own
/// `images` key, and that each names its base image and the crun, hostkeeper and js-agent
/// archives by an HTTPS URL of that platform and a 64-hex SHA-256 equal to the pins file's.
#[test]
fn image_recipe_covers_both_arches() {
    let (_out, recipes) = repo_recipes("covers");
    let pins = image::parse_pins(Path::new(PINS), &text(&repo(), PINS)).expect("pins");
    let arch_words = [("x86_64", "amd64"), ("arm64", "aarch64")];
    for ((arch, recipe), (key, other)) in ARCHES.iter().zip(&recipes).zip(arch_words) {
        assert_eq!(recipe["platform"], arch.platform());
        assert_eq!(recipe["images_key"], key);
        let p = pins.of(*arch);
        let archives = recipe["archives"].as_array().expect("archives");
        let names: Vec<&str> = archives
            .iter()
            .map(|a| a["name"].as_str().expect("name"))
            .collect();
        assert_eq!(names, ["crun", "hostkeeper", "js-agent"]);
        let expected = [
            (&recipe["base"], &p.base),
            (&archives[0], &p.crun),
            (&archives[1], &p.hostkeeper),
            (&archives[2], &p.js_agent),
        ];
        for (listed, pin) in expected {
            let url = listed["url"].as_str().expect("url");
            let sha = listed["sha256"].as_str().expect("sha256");
            assert!(url.starts_with("https://"), "{url}");
            assert!(
                url.contains(key) || url.contains(other),
                "{url} is not for {key}"
            );
            assert!(is_sha256(sha), "{sha}");
            assert_eq!(
                (url, sha),
                (pin.url.as_str(), pin.sha256.to_string().as_str())
            );
        }
    }
}

/// Asserts that two runs over the same inputs write byte-identical files, all four of each
/// platform, and that the line the verb prints names each recipe's SHA-256.
#[test]
fn image_recipe_is_deterministic() {
    let (first, second) = (scratch("same-1"), scratch("same-2"));
    let line = run(&repo(), &first).expect("first");
    run(&repo(), &second).expect("second");
    let mut digests = Vec::new();
    for arch in ARCHES {
        for name in ["recipe.json", "user-data", "meta-data", "base.sha256"] {
            let rel = format!("{}/{name}", arch.platform());
            let (a, b) = (
                std::fs::read(first.join(&rel)),
                std::fs::read(second.join(&rel)),
            );
            assert_eq!(a.expect("first file"), b.expect("second file"), "{rel}");
        }
        let bytes = std::fs::read(first.join(arch.platform()).join("recipe.json")).expect("read");
        digests.push(format!(
            "{} (recipe sha256 {})",
            arch.platform(),
            xtask::fetch::Sha256::of(&bytes)
        ));
    }
    assert_eq!(
        line,
        format!(
            "image-recipe: wrote {} under {}",
            digests.join(" and "),
            first.display()
        )
    );
}

/// Asserts that a token-like key in the pins file, or in a unit file the image bakes, makes the
/// verb fail naming the file, where in it, and the key, and that it then wrote nothing; and that
/// the program prints that message on stderr and exits 1.
#[test]
fn image_recipe_rejects_secret_material() {
    let tail = "looks like secret material, and a machine image carries none (its configuration \
                comes from the provider's user data at boot)";

    let root = copied_root("secret-pin");
    let pins = root.join(PINS);
    let text = std::fs::read_to_string(&pins).expect("pins");
    let changed = text.replacen(
        "[linux-arm64]\n",
        "[linux-arm64]\njoin_token = \"t0k3n\"\n",
        1,
    );
    assert_ne!(changed, text);
    std::fs::write(&pins, changed).expect("write");
    let out = root.join("out");
    let refused = run(&root, &out).expect_err("refused");
    assert_eq!(
        refused.to_string(),
        format!(
            "{}: linux-arm64.join_token: the key \"join_token\" {tail}",
            pins.display()
        )
    );
    assert!(!out.exists(), "nothing is written");

    let root = copied_root("secret-unit");
    let unit = root.join("packaging/machine-image/js-agent-user-data.service");
    let text = std::fs::read_to_string(&unit).expect("unit");
    let line = text
        .lines()
        .position(|l| l == "[Service]")
        .expect("[Service]")
        + 2;
    let changed = text.replace("[Service]\n", "[Service]\nEnvironment=JS_API_TOKEN=abc\n");
    std::fs::write(&unit, changed).expect("write");
    let out = root.join("out");
    let message = format!(
        "{}: line {line}: the key \"JS_API_TOKEN\" {tail}",
        unit.display()
    );
    assert_eq!(run(&root, &out).expect_err("refused").to_string(), message);
    assert!(!out.exists(), "nothing is written");

    let program = Command::new(XTASK)
        .args(["image-recipe", "--out"])
        .arg(&out)
        .arg("--root")
        .arg(&*root)
        .output()
        .expect("xtask");
    assert_eq!(program.status.code(), Some(1));
    assert_eq!(String::from_utf8_lossy(&program.stdout), "");
    assert_eq!(
        String::from_utf8_lossy(&program.stderr),
        format!("xtask: image-recipe: {message}\n")
    );
    assert!(!out.exists(), "nothing is written");
}

/// Asserts that the recipe enables js-agent's crun unit, hostkeeper's unit, the user-data reader
/// and the scratch mount, that the build's last `systemctl enable` enables exactly those, that the
/// reader is baked as a unit and asserts every required user-data path (and no optional one),
/// and that the agent's unit requires the reader and starts after it.
#[test]
fn image_recipe_units_enable_agent_and_hostkeeper() {
    let (_out, recipes) = repo_recipes("units");
    for recipe in &recipes {
        let enabled = strings(&recipe["enabled_units"]);
        assert_eq!(enabled, ENABLED_UNITS);
        for unit in ["js-agent-crun.service", "hostkeeper.service", READER_UNIT] {
            assert!(enabled.iter().any(|u| u == unit), "{unit} is enabled");
        }
        let build: Vec<Vec<String>> = recipe["build"]
            .as_array()
            .expect("build")
            .iter()
            .map(strings)
            .collect();
        let enables: Vec<&Vec<String>> = build
            .iter()
            .filter(|c| c[..2] == ["systemctl", "enable"] && c[2] != "--now")
            .collect();
        assert_eq!(enables.len(), 1);
        assert_eq!(enables[0][2..], ENABLED_UNITS);

        assert_eq!(recipe["user_data"]["reader"], READER_UNIT);
        let file = |path: &str| -> String {
            let files = recipe["files"].as_array().expect("files");
            let found = files.iter().find(|f| f["path"] == path);
            found.unwrap_or_else(|| panic!("{path} is baked"))["text"]
                .as_str()
                .expect("text")
                .to_owned()
        };
        let reader = file(&format!("/etc/systemd/system/{READER_UNIT}"));
        let asserted: Vec<&str> = reader
            .lines()
            .filter_map(|l| l.strip_prefix("AssertPathExists="))
            .collect();
        let required: Vec<&str> = USER_DATA_PATHS
            .iter()
            .filter(|p| p.required)
            .map(|p| p.path)
            .collect();
        assert_eq!(asserted, required);
        let listed: Vec<String> = recipe["user_data"]["paths"]
            .as_array()
            .expect("paths")
            .iter()
            .map(|p| p["path"].as_str().expect("path").to_owned())
            .collect();
        let all: Vec<&str> = USER_DATA_PATHS.iter().map(|p| p.path).collect();
        assert_eq!(listed, all);
        assert!(reader.contains("\nAfter=cloud-init.service\n"));
        assert!(reader.contains("\nBefore=js-agent-crun.service hostkeeper.service\n"));
        assert!(reader.contains("\nWantedBy=multi-user.target\n"));

        for unit in [READER_UNIT, image::SCRATCH_UNIT] {
            file(&format!("/etc/systemd/system/{unit}"));
        }
        let drop_in = file("/etc/systemd/system/js-agent-crun.service.d/10-image.conf");
        assert!(drop_in.contains(&format!("\nRequires={READER_UNIT}\nAfter={READER_UNIT}\n")));
    }
}

/// The rows of the table "What it sets up" of the crun runbook: its `step` and `what` columns.
fn runbook_rows() -> Vec<(String, String)> {
    let runbook = text(&repo(), CRUN_RUNBOOK);
    let table = runbook
        .split("## What it sets up\n")
        .nth(1)
        .and_then(|rest| rest.split("\n\n").find(|block| block.starts_with('|')))
        .expect("the table");
    table
        .lines()
        .skip(2)
        .map(|row| {
            let cells: Vec<&str> = row.split(" | ").collect();
            let step = cells[0].trim_start_matches("| ");
            (step.to_owned(), cells[1].to_owned())
        })
        .collect()
}

/// The step functions of `crun_host_setup.sh`'s `setup_pass`, in order.
fn script_steps() -> Vec<String> {
    let script = text(&repo(), "packaging/js-agent/crun_host_setup.sh");
    let line = script
        .lines()
        .find_map(|l| l.trim().strip_prefix("for step in "))
        .expect("setup_pass loop");
    let list = line.split("; do").next().expect("list");
    list.split_whitespace().map(str::to_owned).collect()
}

/// Asserts that the crun host setup of each recipe is the steps file's list, that the runbook's
/// table is the same list row for row (step and what), that the script runs the steps' functions
/// in that order, that the runbook cites the steps file, and that the recipe makes the steps
/// with one apply of the baked script, given the baked unit and tmpfiles file by their SHA-256.
#[test]
fn image_recipe_matches_crun_runbook() {
    let steps =
        image::parse_crun_steps(Path::new(CRUN_STEPS), &text(&repo(), CRUN_STEPS)).expect("steps");
    let listed: Vec<(String, String)> = steps
        .iter()
        .map(|s| (s.name.clone(), s.what.clone()))
        .collect();
    assert_eq!(runbook_rows(), listed);
    assert!(text(&repo(), CRUN_RUNBOOK).contains(&format!("`{CRUN_STEPS}`")));

    let functions: Vec<String> = steps.iter().map(|s| s.function.clone()).collect();
    let run_by_script: Vec<String> = script_steps()
        .into_iter()
        .filter(|f| functions.contains(f))
        .collect();
    assert_eq!(run_by_script, functions);
    assert_eq!(
        script_steps().len(),
        functions.len() + 2,
        "preflight and reload besides"
    );

    let (_out, recipes) = repo_recipes("crun");
    let sha = |rel: &str| xtask::fetch::Sha256::of(text(&repo(), rel).as_bytes()).to_string();
    for recipe in &recipes {
        let setup = &recipe["crun_host_setup"];
        assert_eq!(setup["runbook"], CRUN_RUNBOOK);
        assert_eq!(setup["steps_source"], CRUN_STEPS);
        let in_recipe: Vec<(String, String, String)> = setup["steps"]
            .as_array()
            .expect("steps")
            .iter()
            .map(|s| {
                let field = |k: &str| s[k].as_str().expect("field").to_owned();
                (field("name"), field("what"), field("function"))
            })
            .collect();
        let expected: Vec<(String, String, String)> = steps
            .iter()
            .map(|s| (s.name.clone(), s.what.clone(), s.function.clone()))
            .collect();
        assert_eq!(in_recipe, expected);

        let command = strings(&setup["command"]);
        assert_eq!(
            command,
            [
                image::SETUP_SCRIPT,
                "--apply",
                "--server",
                image::SERVER_PLACEHOLDER,
                "--pool",
                image::POOL_PLACEHOLDER,
                "--unit",
                image::CRUN_UNIT,
                "--unit-sha256",
                &sha("packaging/js-agent/js-agent-crun.service"),
                "--tmpfiles",
                image::CRUN_TMPFILES,
                "--tmpfiles-sha256",
                &sha("packaging/js-agent/tmpfiles-crun.conf"),
            ]
        );
        let build: Vec<Vec<String>> = recipe["build"]
            .as_array()
            .expect("build")
            .iter()
            .map(strings)
            .collect();
        assert_eq!(build.iter().filter(|c| **c == command).count(), 1);
        let files = recipe["files"].as_array().expect("files");
        let script = files
            .iter()
            .find(|f| f["path"] == image::SETUP_SCRIPT)
            .expect("the script is baked");
        assert_eq!(script["mode"], "0755");
        assert_eq!(
            script["sha256"],
            sha("packaging/js-agent/crun_host_setup.sh")
        );
    }
}

/// Asserts that each platform's seed is the recipe for cloud-init: a `#cloud-config` whose body
/// is JSON, writing every baked file with its mode, the archives' SHA-256 list and the recipe
/// itself, running `set -eu` and then the recipe's build commands, and powering off; that the
/// last command prints the build's console line; and that `meta-data` and `base.sha256` name the
/// platform and the base image's digest.
#[test]
fn image_recipe_seed_runs_the_recipe() {
    let (out, recipes) = repo_recipes("seed");
    for (arch, recipe) in ARCHES.iter().zip(&recipes) {
        let dir = arch.platform();
        let seed = text(&out, &format!("{dir}/user-data"));
        let body = seed.strip_prefix("#cloud-config\n").expect("cloud-config");
        let seed: Value = serde_json::from_str(body).expect("JSON body");

        let runcmd = seed["runcmd"].as_array().expect("runcmd");
        assert_eq!(runcmd[0], "set -eu");
        assert_eq!(runcmd[1..], recipe["build"].as_array().expect("build")[..]);
        assert_eq!(
            runcmd.last().expect("last"),
            &serde_json::json!(["printf", "js-image-build-%s\\n", "done"])
        );
        assert_eq!(
            seed["power_state"],
            serde_json::json!({"mode": "poweroff", "condition": true})
        );

        let written = seed["write_files"].as_array().expect("write_files");
        let files = recipe["files"].as_array().expect("files");
        assert_eq!(written.len(), files.len() + 2);
        for (w, f) in written.iter().zip(files) {
            assert_eq!(
                (&w["path"], &w["permissions"], &w["content"], &w["owner"]),
                (
                    &f["path"],
                    &f["mode"],
                    &f["text"],
                    &serde_json::json!("root:root")
                )
            );
        }
        let sums: String = recipe["archives"]
            .as_array()
            .expect("archives")
            .iter()
            .map(|a| {
                format!(
                    "{}  /var/tmp/js-image/{}\n",
                    a["sha256"].as_str().expect("sha"),
                    a["file"].as_str().expect("file")
                )
            })
            .collect();
        assert_eq!(written[files.len()]["path"], "/var/tmp/js-image/SHA256SUMS");
        assert_eq!(written[files.len()]["content"], sums);
        assert_eq!(written[files.len() + 1]["path"], RECIPE_PATH);
        assert_eq!(
            written[files.len() + 1]["content"],
            text(&out, &format!("{dir}/recipe.json"))
        );

        assert_eq!(
            text(&out, &format!("{dir}/meta-data")),
            format!("instance-id: js-image-build-{dir}\nlocal-hostname: js-image-build-{dir}\n")
        );
        assert_eq!(
            text(&out, &format!("{dir}/base.sha256")),
            format!(
                "{}  base.img\n",
                recipe["base"]["sha256"].as_str().expect("sha")
            )
        );
    }
}

/// Asserts that the boot check's fake user data writes every required user-data path of the
/// recipe, and that its agent command line is the baked crun unit's with only the server (a
/// loopback one) and the pool changed, so the check runs the unit the image carries.
#[test]
fn the_boot_check_user_data_is_the_baked_unit_with_its_own_server() {
    let check = text(&repo(), "packaging/machine-image/check/user-data");
    assert!(check.starts_with("#cloud-config\n"));
    for path in USER_DATA_PATHS.iter().filter(|p| p.required) {
        assert!(
            check.contains(&format!("  - path: {}\n", path.path)),
            "{}",
            path.path
        );
    }
    let unit = text(&repo(), "packaging/js-agent/js-agent-crun.service");
    let line = |text: &str| -> String {
        let found = text
            .lines()
            .map(str::trim_start)
            .find(|l| l.starts_with("ExecStart=/"));
        found.expect("ExecStart").to_owned()
    };
    let expected = line(&unit)
        .replace(
            "--server SET-THE-SERVER-WITH-SYSTEMCTL-EDIT",
            "--server http://127.0.0.1:7300",
        )
        .replace("--pool default", "--pool image-check");
    assert_eq!(line(&check), expected);
    assert!(check.contains("printf 'js-image-check-%s\\n' passed\n"));
    assert!(
        !check.contains("js-image-check-passed"),
        "the console line is made at run time"
    );
}

/// Asserts that a missing input is named (the pins file, a baked file, the steps file), and that
/// an output that cannot be written is named: a platform directory that is a file, and a recipe
/// path that is a directory.
#[test]
fn image_recipe_names_what_it_cannot_read_or_write() {
    let root = copied_root("unreadable");
    let out = root.join("out");
    let missing = |rel: &str| {
        let path = root.join(rel);
        let saved = std::fs::read(&path).expect("read");
        std::fs::remove_file(&path).expect("remove");
        let message = run(&root, &out).expect_err("refused").to_string();
        std::fs::write(&path, saved).expect("restore");
        assert_eq!(
            message,
            format!(
                "cannot read {}: No such file or directory (os error 2)",
                path.display()
            )
        );
    };
    missing(PINS);
    missing("packaging/machine-image/scratch.mount");
    missing(CRUN_STEPS);
    assert!(!out.exists(), "nothing is written");

    let steps = root.join(CRUN_STEPS);
    let saved = std::fs::read(&steps).expect("read");
    std::fs::write(&steps, "[[step]]\nname = \"packages\"\n").expect("write");
    assert_eq!(
        run(&root, &out).expect_err("refused").to_string(),
        format!("{}: step[0].what: is missing", steps.display())
    );
    std::fs::write(&steps, saved).expect("restore");
    assert!(!out.exists(), "nothing is written");

    std::fs::write(&out, "").expect("a file where the output goes");
    let message = run(&root, &out).expect_err("refused").to_string();
    let dir = out.join("linux-x86_64");
    assert_eq!(
        message,
        format!(
            "cannot write {}: Not a directory (os error 20)",
            dir.display()
        )
    );
    std::fs::remove_file(&out).expect("remove");

    let recipe = out.join("linux-arm64/recipe.json");
    std::fs::create_dir_all(&recipe).expect("a directory where the recipe goes");
    let message = run(&root, &out).expect_err("refused").to_string();
    assert_eq!(
        message,
        format!(
            "cannot write {}: Is a directory (os error 21)",
            recipe.display()
        )
    );
}

/// Asserts that the program writes the recipes and prints the verb's line, exit status 0.
#[test]
fn the_program_writes_the_recipes() {
    let out = scratch("program");
    let program = Command::new(XTASK)
        .args(["image-recipe", "--out"])
        .arg(&*out)
        .arg("--root")
        .arg(repo())
        .output()
        .expect("xtask");
    assert_eq!(program.status.code(), Some(0), "{program:?}");
    let printed = String::from_utf8(program.stdout).expect("utf-8");
    assert!(
        printed.starts_with("image-recipe: wrote linux-x86_64 (recipe sha256 "),
        "{printed}"
    );
    for arch in ARCHES {
        assert!(out.join(arch.platform()).join("recipe.json").is_file());
    }
}

/// Asserts that `--pins` selects the pins file the recipes are made from: given another file
/// under `--root`, whose js-agent pin of linux-x86_64 differs, the program writes that pin into
/// the linux-x86_64 recipe and seed, and the default pins file, still under the root, is not read
/// (its pin appears nowhere).
#[test]
fn the_pins_flag_selects_another_pins_file() {
    let root = copied_root("pins-flag");
    let default = text(&root, PINS);
    let pins = image::parse_pins(Path::new(PINS), &default).expect("pins");
    let old = pins.of(image::Arch::X86_64).js_agent.clone();
    let (url, sha) = (
        "https://example.org/js-agent-9.9.9-x86_64-unknown-linux-musl.tar.gz",
        "9".repeat(64),
    );
    let other = default
        .replacen(&old.url, url, 1)
        .replacen(&old.sha256.to_string(), &sha, 1);
    assert_ne!(other, default);
    let rel = "packaging/machine-image/other-pins.toml";
    std::fs::write(root.join(rel), other).expect("write");
    let out = root.join("out");
    let program = Command::new(XTASK)
        .args(["image-recipe", "--out"])
        .arg(&out)
        .arg("--root")
        .arg(&*root)
        .args(["--pins", rel])
        .output()
        .expect("xtask");
    assert_eq!(program.status.code(), Some(0), "{program:?}");
    let recipe = recipe(&out, "linux-x86_64");
    let agent = &recipe["archives"][2];
    assert_eq!(agent["name"], "js-agent");
    assert_eq!(
        (&agent["url"], &agent["sha256"]),
        (&Value::from(url), &Value::from(sha.as_str()))
    );
    for platform in ["linux-x86_64", "linux-arm64"] {
        for name in ["recipe.json", "user-data"] {
            let written = text(&out, &format!("{platform}/{name}"));
            assert!(!written.contains(&old.url), "{platform}/{name}");
        }
    }
    assert!(
        text(&out, "linux-x86_64/user-data")
            .contains(&format!("{sha}  /var/tmp/js-image/js-agent.tar.gz"))
    );
    assert_eq!(
        text(&root, PINS),
        default,
        "the default pins file is left as it was"
    );
}

/// Asserts that the machine-image workflow starts only by hand (no pull request, push or
/// schedule), holds only read access to the repository and no secret, pins every action by a
/// commit SHA, runs on GitHub-hosted runners, writes the recipe with the verb, and runs no command
/// that publishes or pushes an image (no cloud or registry tool).
#[test]
fn the_machine_image_workflow_is_by_hand_and_publishes_nothing() {
    let workflow = text(&repo(), ".github/workflows/machine-image.yml");
    let on: Vec<&str> = workflow
        .split("\non:\n")
        .nth(1)
        .expect("on:")
        .lines()
        .take_while(|l| l.starts_with(' '))
        .filter(|l| l.starts_with("  ") && !l.starts_with("   "))
        .collect();
    assert_eq!(on, ["  workflow_dispatch:"]);
    let code: String = workflow
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .map(|l| format!("{l}\n"))
        .collect();
    for word in [
        "pull_request",
        "push:",
        "schedule:",
        "workflow_run",
        "secrets.",
        "id-token",
    ] {
        assert!(!code.contains(word), "{word}");
    }
    assert!(workflow.contains("\npermissions: {}\n"));
    assert!(workflow.contains("\n    permissions:\n      contents: read\n    steps:\n"));
    assert_eq!(workflow.matches("permissions:").count(), 2);
    for line in workflow.lines().filter(|l| l.contains("uses: ")) {
        let pinned = line.split('@').nth(1).expect("@");
        let sha = pinned.split(' ').next().expect("sha");
        assert!(
            sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_hexdigit()),
            "{line}"
        );
    }
    assert!(workflow.contains(
        "runs-on: ${{ inputs.arch == 'arm64' && 'ubuntu-24.04-arm' || 'ubuntu-24.04' }}"
    ));
    assert!(workflow.contains("run: cargo xtask image-recipe --out target/machine-image\n"));
    let runs: Vec<&str> = workflow
        .lines()
        .filter_map(|l| l.trim_start().strip_prefix("run: "))
        .collect();
    for run in &runs {
        for tool in [
            "aws ", "gcloud", "gsutil", "az ", "docker", "podman", "oras", "skopeo", "gh ",
        ] {
            assert!(!run.contains(tool), "{run}");
        }
    }
    assert!(runs.iter().all(|r| !r.contains('\n')));
}
