//! The step `Sign the programs` of `sign-code` (`.github/workflows/js-agent-macos-pkg.yml`), run
//! as it is committed, with bash as the runner runs it (`bash --noprofile --norc -e`), every
//! system tool it calls by absolute path standing in as `host-shim` (src/bin/host-shim.rs): the
//! shim records each call and gives the answer a test sets, and `base64`, `awk`, `cat` and `cmp`
//! are passed to the real programs, so the step's own decisions are what is tested.
//!
//! The import of Apple's intermediate passes on a runner without it and on one that holds it
//! already (`security` answers exactly that the item exists), and on nothing else; the identity
//! must still be listed as valid by `find-identity -v`, once, by its exact name, and valid for 30
//! days; only then is each program signed. Whatever happens, the keychain is deleted
//! last and the job's files are removed, and neither secret reaches the log.

mod crun_host;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crun_host::{SHIM, esc, install_program, profile_env, read, real, rule};
use xtask::release::mac_programs::{APPLICATION_IDENTITY, PROGRAMS};

const WORKFLOW: &str = ".github/workflows/js-agent-macos-pkg.yml";

/// What `security import` printed on the hosted runner of the first signing run on main, which
/// holds Apple's Developer ID G2 intermediate already.
const DUPLICATE: &str =
    "security: SecKeychainItemImport: The specified item already exists in the keychain.";

/// The secrets the test gives the step. Neither may be printed.
const P12: &str = "bm90IGEgcmVhbCBrZXkK";
const P12_PASSWORD: &str = "p12-password-not-for-the-log";

/// What the fake `uuidgen` answers: the keychain's password.
const KEYCHAIN_PASSWORD: &str = "0F5E1D9C-7B6A-4C3E-9D2F-8A1B0C4D6E7F";

/// The SHA-1 the fake `find-identity` lists the identity under.
const SHA1: &str = "0123456789ABCDEF0123456789ABCDEF01234567";

/// The system tools the step calls by absolute path, each a stand-in.
const TOOLS: [&str; 10] = [
    "security", "uuidgen", "base64", "curl", "shasum", "awk", "openssl", "codesign", "cat", "cmp",
];

/// The step's `run:` script and the plain values of its `env:` (its secrets left out).
fn step() -> (String, Vec<(String, String)>) {
    let text = read(WORKFLOW);
    let lines: Vec<&str> = text.lines().collect();
    let start = lines
        .iter()
        .position(|l| *l == "      - name: Sign the programs")
        .expect("the step");
    let mut env = Vec::new();
    let mut script = String::new();
    let mut in_run = false;
    for line in &lines[start + 1..] {
        let indent = line.len() - line.trim_start().len();
        if !line.is_empty() && indent <= 6 {
            break;
        }
        if in_run {
            script.push_str(line.get(10..).unwrap_or(""));
            script.push('\n');
        } else if line.trim() == "run: |" {
            in_run = true;
        } else if indent == 10 && !line.trim_start().starts_with('#') {
            let (key, value) = line.trim().split_once(": ").expect("KEY: value");
            if !value.contains("secrets.") {
                env.push((key.to_owned(), value.trim_matches('"').to_owned()));
            }
        }
    }
    (script, env)
}

/// `script` with each system tool called by absolute path (`/usr/bin/`, `/usr/sbin/`, `/bin/`)
/// called from `shims` instead. The one absolute path that is an argument, the tool the key is
/// imported for (`-T /usr/bin/codesign`), stays as it is.
fn to_shims(script: &str, shims: &Path) -> String {
    let mut out = String::new();
    let mut rest = script;
    while !rest.is_empty() {
        let at_word = out.is_empty() || out.ends_with([' ', '\n', '(', '|', '!']);
        let prefix = ["/usr/bin/", "/usr/sbin/", "/bin/"]
            .into_iter()
            .find(|p| rest.starts_with(p));
        match prefix {
            Some(p) if at_word && !out.ends_with("-T ") => {
                out.push_str(&format!("{}/", shims.display()));
                rest = &rest[p.len()..];
            }
            _ => {
                let c = rest.chars().next().expect("a character");
                out.push(c);
                rest = &rest[c.len_utf8()..];
            }
        }
    }
    out
}

/// What a run of the step did.
struct Ran {
    status: Option<i32>,
    /// What it printed, standard output and error together, as the job's log shows them.
    log: String,
    /// The calls of the stand-ins, one `<tool> <arguments>` per line.
    calls: Vec<String>,
    /// The job's temporary directory after the run.
    temp: PathBuf,
    /// Where the stand-ins wrote what they were given on standard input.
    given: PathBuf,
    _scratch: js_testkit::dir::TestDir,
}

/// The answers of one run: `import` is what `security import` of the intermediate answers (its
/// output and status), `identities` what `find-identity -v` lists, `expiring` whether the
/// identity expires within 30 days.
struct Answers<'a> {
    import: (&'a str, u8),
    identities: &'a str,
    expiring: bool,
}

/// What `find-identity -v -p codesigning` lists for a keychain that holds the identity, valid.
fn valid_identity() -> String {
    format!("  1) {SHA1} \"{APPLICATION_IDENTITY}\"\n     1 valid identities found\n")
}

/// A runner without the intermediate, with the identity valid for longer than 30 days.
fn healthy() -> Answers<'static> {
    Answers {
        import: ("1 certificate imported.\n", 0),
        identities: "",
        expiring: false,
    }
}

/// Runs the step against stand-ins that answer `answers`.
fn run(name: &str, answers: &Answers) -> Ran {
    let scratch = js_testkit::dir::TestDir::under(
        env!("CARGO_TARGET_TMPDIR"),
        &format!("sign-code-run-{name}"),
    );
    let dir = scratch.path().to_path_buf();
    let (shims, path, temp, given, work) = (
        dir.join("shims"),
        dir.join("path"),
        dir.join("temp"),
        dir.join("given"),
        dir.join("work"),
    );
    for sub in [&shims, &path, &temp, &given, &work.join("programs")] {
        std::fs::create_dir_all(sub).expect("directory");
    }
    for tool in TOOLS {
        install_program(Path::new(SHIM), &shims.join(tool));
    }
    // The one program the step runs by name.
    std::os::unix::fs::symlink(real("rm"), path.join("rm")).expect("rm");
    let t = |file: &str| temp.join(file).display().to_string();
    let keychain = t("js-agent-code-signing.keychain-db");
    let identities = if answers.identities.is_empty() {
        valid_identity()
    } else {
        answers.identities.to_owned()
    };
    let real_tool = |tool: &str| format!("!{}", real(tool).display());
    let ok = |tool: &str, args: &str, out: &str| rule(tool, args, 0, &esc(out), &[]);
    let rules = [
        ok("uuidgen", "", &format!("{KEYCHAIN_PASSWORD}\n")),
        ok(
            "security",
            &format!("create-keychain -p {KEYCHAIN_PASSWORD} {keychain}"),
            "",
        ),
        ok(
            "security",
            &format!("set-keychain-settings -lut 3600 {keychain}"),
            "",
        ),
        ok(
            "security",
            &format!("unlock-keychain -p {KEYCHAIN_PASSWORD} {keychain}"),
            "",
        ),
        rule("base64", "--decode", 0, &real_tool("base64"), &[]),
        ok(
            "security",
            &format!(
                "import {} -P {P12_PASSWORD} -T /usr/bin/codesign -f pkcs12 -k {keychain}",
                t("application.p12")
            ),
            "1 identity imported.\n",
        ),
        ok(
            "security",
            &format!(
                "set-key-partition-list -S apple-tool:,apple: -s -k {KEYCHAIN_PASSWORD} {keychain}"
            ),
            "",
        ),
        ok(
            "security",
            &format!("list-keychains -d user -s {keychain}"),
            "",
        ),
        rule(
            "curl",
            &format!(
                "--fail --silent --show-error --proto =https --tlsv1.2 --output {} https://www.apple.com/certificateauthority/DeveloperIDG2CA.cer",
                t("DeveloperIDG2CA.cer")
            ),
            0,
            "",
            &[format!("write={}=a certificate", t("DeveloperIDG2CA.cer"))],
        ),
        rule(
            "shasum",
            "-a 256 --check",
            0,
            &esc(&format!("{}: OK\n", t("DeveloperIDG2CA.cer"))),
            &[format!("stdin={}", given.join("shasum").display())],
        ),
        rule(
            "security",
            &format!(
                "import {} -t cert -f x509 -k {keychain}",
                t("DeveloperIDG2CA.cer")
            ),
            answers.import.1,
            &esc(answers.import.0),
            &[],
        ),
        ok(
            "security",
            &format!("find-identity -v -p codesigning {keychain}"),
            &identities,
        ),
        rule("awk", "*", 0, &real_tool("awk"), &[]),
        ok(
            "security",
            &format!("find-certificate -c {APPLICATION_IDENTITY} -p {keychain}"),
            "-----BEGIN CERTIFICATE-----\nYSBjZXJ0aWZpY2F0ZQ==\n-----END CERTIFICATE-----\n",
        ),
        rule(
            "openssl",
            "x509 -noout -checkend 2592000",
            u8::from(answers.expiring),
            if answers.expiring {
                "Certificate will expire\\n"
            } else {
                "Certificate will not expire\\n"
            },
            &[format!("stdin={}", given.join("openssl").display())],
        ),
        ok("codesign", "*", ""),
        rule("cat", "*", 0, &real_tool("cat"), &[]),
        rule("cmp", "*", 0, &real_tool("cmp"), &[]),
        ok("security", &format!("delete-keychain {keychain}"), ""),
    ];
    std::fs::write(dir.join("answers"), rules.join("\n") + "\n").expect("answers");
    let (script, step_env) = step();
    let script_file = dir.join("step.sh");
    std::fs::write(&script_file, to_shims(&script, &shims)).expect("the step");
    let out = Command::new(real("bash"))
        .args(["--noprofile", "--norc", "-e"])
        .arg(&script_file)
        .current_dir(&work)
        .env_clear()
        .env("PATH", &path)
        .env("HOST_SHIM_DIR", &dir)
        .env("RUNNER_TEMP", &temp)
        .env("MACOS_APPLICATION_P12", P12)
        .env("MACOS_APPLICATION_P12_PASSWORD", P12_PASSWORD)
        .envs(step_env)
        .envs(profile_env())
        .stdin(Stdio::null())
        .output()
        .expect("run bash");
    let calls = std::fs::read_to_string(dir.join("calls")).unwrap_or_default();
    Ran {
        status: out.status.code(),
        log: format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
        calls: piped_in_order(calls.lines().map(str::to_owned).collect()),
        temp,
        given,
        _scratch: scratch,
    }
}

/// `calls` with the two ends of each pipe of the step in the order they are written. Both ends
/// start together, so either may record its call first; nothing else is reordered.
fn piped_in_order(mut calls: Vec<String>) -> Vec<String> {
    let pipes = [
        ("security find-identity ", "awk "),
        ("security find-certificate ", "openssl "),
    ];
    for at in 1..calls.len() {
        let swapped = pipes
            .iter()
            .any(|(left, right)| calls[at - 1].starts_with(right) && calls[at].starts_with(left));
        if swapped {
            calls.swap(at - 1, at);
        }
    }
    calls
}

/// The calls of a run whose import was answered: the keychain, the identity, the intermediate,
/// its check and import, and the print of the import's answer.
fn calls_to_the_import(temp: &Path) -> Vec<String> {
    let t = |file: &str| temp.join(file).display().to_string();
    let keychain = t("js-agent-code-signing.keychain-db");
    vec![
        "uuidgen ".to_owned(),
        format!("security create-keychain -p {KEYCHAIN_PASSWORD} {keychain}"),
        format!("security set-keychain-settings -lut 3600 {keychain}"),
        format!("security unlock-keychain -p {KEYCHAIN_PASSWORD} {keychain}"),
        "base64 --decode".to_owned(),
        format!(
            "security import {} -P {P12_PASSWORD} -T /usr/bin/codesign -f pkcs12 -k {keychain}",
            t("application.p12")
        ),
        format!(
            "security set-key-partition-list -S apple-tool:,apple: -s -k {KEYCHAIN_PASSWORD} {keychain}"
        ),
        format!("security list-keychains -d user -s {keychain}"),
        format!(
            "curl --fail --silent --show-error --proto =https --tlsv1.2 --output {} https://www.apple.com/certificateauthority/DeveloperIDG2CA.cer",
            t("DeveloperIDG2CA.cer")
        ),
        "shasum -a 256 --check".to_owned(),
        format!(
            "security import {} -t cert -f x509 -k {keychain}",
            t("DeveloperIDG2CA.cer")
        ),
        format!("cat {}", t("import-intermediate.txt")),
    ]
}

/// The calls after the import passed: the identity found among the valid ones, its expiry
/// checked, then each program signed with it under its identifier.
fn calls_after_the_import(temp: &Path) -> Vec<String> {
    let keychain = temp
        .join("js-agent-code-signing.keychain-db")
        .display()
        .to_string();
    let mut calls = vec![
        format!("security find-identity -v -p codesigning {keychain}"),
        format!(
            "awk -v name={APPLICATION_IDENTITY} index($0, \"\\\"\" name \"\\\"\") {{ print $2 }}"
        ),
        format!("security find-certificate -c {APPLICATION_IDENTITY} -p {keychain}"),
        "openssl x509 -noout -checkend 2592000".to_owned(),
    ];
    for p in PROGRAMS {
        calls.push(format!(
            "codesign --force --sign {SHA1} --keychain {keychain} --identifier {} --options runtime --timestamp programs/{}",
            p.identifier, p.name
        ));
    }
    calls
}

fn delete_keychain(temp: &Path) -> String {
    format!(
        "security delete-keychain {}",
        temp.join("js-agent-code-signing.keychain-db").display()
    )
}

/// What every run must leave: the keychain deleted by the last call, the key file, the
/// intermediate and the import's answer removed, and neither secret in the log.
fn assert_cleaned_up(name: &str, ran: &Ran) {
    assert_eq!(
        ran.calls.last(),
        Some(&delete_keychain(&ran.temp)),
        "{name}: {:?}",
        ran.calls
    );
    let left: Vec<_> = std::fs::read_dir(&ran.temp)
        .expect("the job's temporary directory")
        .map(|e| e.expect("entry").file_name())
        .collect();
    assert!(left.is_empty(), "{name}: {left:?}");
    assert!(!ran.log.contains(P12), "{name}: {}", ran.log);
    assert!(!ran.log.contains(P12_PASSWORD), "{name}: {}", ran.log);
}

#[test]
fn the_intermediate_imported_into_the_keychain_signs_the_programs() {
    // Catches: a step that fails on a runner without the intermediate, imports it anywhere but
    // the job's keychain, signs before the identity is found valid, or signs another program or
    // identifier than the table's.
    let ran = run("imported", &healthy());
    assert_eq!(ran.status, Some(0), "{}", ran.log);
    let mut want = calls_to_the_import(&ran.temp);
    want.extend(calls_after_the_import(&ran.temp));
    want.push(delete_keychain(&ran.temp));
    assert_eq!(ran.calls, want);
    assert_cleaned_up("imported", &ran);
    assert!(ran.log.contains("1 certificate imported.\n"), "{}", ran.log);
    // The intermediate is held to the pinned digest, and the identity's certificate is what
    // the expiry check reads.
    assert_eq!(
        std::fs::read_to_string(ran.given.join("shasum")).expect("the check"),
        format!(
            "f16cd3c54c7f83cea4bf1a3e6a0819c8aaa8e4a1528fd144715f350643d2df3a  {}\n",
            ran.temp.join("DeveloperIDG2CA.cer").display()
        )
    );
    assert_eq!(
        std::fs::read_to_string(ran.given.join("openssl")).expect("the certificate"),
        "-----BEGIN CERTIFICATE-----\nYSBjZXJ0aWZpY2F0ZQ==\n-----END CERTIFICATE-----\n"
    );
}

#[test]
fn a_runner_that_holds_the_intermediate_already_signs_the_programs() {
    // Catches: the duplicate answer of a hosted runner that holds Apple's intermediate failing
    // the job, as it failed the first signing run on main.
    let answer = format!("{DUPLICATE}\n");
    let ran = run(
        "already-exists",
        &Answers {
            import: (&answer, 1),
            ..healthy()
        },
    );
    assert_eq!(ran.status, Some(0), "{}", ran.log);
    let mut want = calls_to_the_import(&ran.temp);
    want.push(format!(
        "cmp -s - {}",
        ran.temp.join("import-intermediate.txt").display()
    ));
    want.extend(calls_after_the_import(&ran.temp));
    want.push(delete_keychain(&ran.temp));
    assert_eq!(ran.calls, want);
    assert_cleaned_up("already-exists", &ran);
    assert!(
        ran.log.contains(&format!(
            "{DUPLICATE}\nthe runner holds Apple's intermediate already\n"
        )),
        "{}",
        ran.log
    );
}

#[test]
fn any_other_failure_of_the_import_stops_the_job_before_signing_after_printing_the_answer() {
    // Catches: a judge that passes every failure of the import, or one that takes an answer
    // that only contains the duplicate (more said after it, a suffix, no line break) for it.
    let other = "security: SecKeychainItemImport: One or more parameters passed to a function were not valid.\n".to_owned();
    let more = format!(
        "{DUPLICATE}\nsecurity: SecKeychainItemImport: Unable to decode the provided data.\n"
    );
    let suffixed = format!("{DUPLICATE} (-25299)\n");
    let before = format!("warning: {DUPLICATE}\n");
    for (name, answer) in [
        ("other", other),
        ("more", more),
        ("suffixed", suffixed),
        ("prefixed", before),
        ("unterminated", DUPLICATE.to_owned()),
        ("empty", String::new()),
    ] {
        let ran = run(
            &format!("import-{name}"),
            &Answers {
                import: (&answer, 1),
                ..healthy()
            },
        );
        assert_eq!(ran.status, Some(1), "{name}: {}", ran.log);
        let mut want = calls_to_the_import(&ran.temp);
        want.push(format!(
            "cmp -s - {}",
            ran.temp.join("import-intermediate.txt").display()
        ));
        want.push(delete_keychain(&ran.temp));
        assert_eq!(ran.calls, want, "{name}");
        assert_cleaned_up(name, &ran);
        assert!(
            ran.log.contains(&format!(
                "{answer}::error::the import of Apple's intermediate failed with the answer above\n"
            )),
            "{name}: {}",
            ran.log
        );
    }
}

#[test]
fn an_identity_not_listed_once_as_valid_by_its_exact_name_stops_the_job_before_signing() {
    // Catches: a chain that does not build (no valid identity) passing because the import of
    // the intermediate was taken as success; another identity, a longer name that starts with
    // the identity's, or two identities of that name passing.
    let installer = format!(
        "  1) {SHA1} \"Developer ID Installer: Komira AI, Inc. (U7RTX4XD3X)\"\n     1 valid identities found\n"
    );
    let longer =
        format!("  1) {SHA1} \"{APPLICATION_IDENTITY} 2\"\n     1 valid identities found\n");
    let twice = format!(
        "  1) {SHA1} \"{APPLICATION_IDENTITY}\"\n  2) 89ABCDEF0123456789ABCDEF0123456789ABCDEF \"{APPLICATION_IDENTITY}\"\n     2 valid identities found\n"
    );
    let duplicate = format!("{DUPLICATE}\n");
    for (name, identities) in [
        ("none", "     0 valid identities found\n".to_owned()),
        ("installer", installer),
        ("longer", longer),
        ("twice", twice),
    ] {
        let ran = run(
            &format!("identity-{name}"),
            &Answers {
                import: (&duplicate, 1),
                identities: &identities,
                expiring: false,
            },
        );
        assert_eq!(ran.status, Some(1), "{name}: {}", ran.log);
        let mut want = calls_to_the_import(&ran.temp);
        want.push(format!(
            "cmp -s - {}",
            ran.temp.join("import-intermediate.txt").display()
        ));
        want.extend(calls_after_the_import(&ran.temp).into_iter().take(2));
        want.push(delete_keychain(&ran.temp));
        assert_eq!(ran.calls, want, "{name}");
        assert_cleaned_up(name, &ran);
        assert!(
            ran.log.ends_with(&format!(
                "::error::the keychain does not hold exactly one valid {APPLICATION_IDENTITY} identity\n"
            )),
            "{name}: {}",
            ran.log
        );
    }
}

#[test]
fn an_identity_that_expires_within_30_days_stops_the_job_before_signing() {
    // Catches: a release signed with an identity about to expire, which would leave the Macs
    // without a renewed signature in time.
    let ran = run(
        "expiring",
        &Answers {
            expiring: true,
            ..healthy()
        },
    );
    assert_eq!(ran.status, Some(1), "{}", ran.log);
    let mut want = calls_to_the_import(&ran.temp);
    want.extend(calls_after_the_import(&ran.temp).into_iter().take(4));
    want.push(delete_keychain(&ran.temp));
    assert_eq!(ran.calls, want);
    assert_cleaned_up("expiring", &ran);
    assert!(
        ran.log.ends_with(&format!(
            "::error::the {APPLICATION_IDENTITY} certificate expires within 30 days: renew it\n"
        )),
        "{}",
        ran.log
    );
}

#[test]
fn every_system_tool_the_step_calls_is_a_stand_in_and_the_key_stays_for_codesign() {
    // Catches: a step that calls a tool by absolute path this test does not stand in for (it
    // would run the real one), and a mapping that moved the `-T /usr/bin/codesign` argument.
    let (script, _) = step();
    let shims = Path::new("/S");
    let mapped = to_shims(&script, shims);
    let mut called: Vec<&str> = mapped
        .match_indices("/S/")
        .map(|(at, _)| {
            let rest = &mapped[at + 3..];
            &rest[..rest
                .find(|c: char| !c.is_ascii_alphanumeric())
                .unwrap_or(rest.len())]
        })
        .collect();
    called.sort_unstable();
    called.dedup();
    let mut tools = TOOLS.to_vec();
    tools.sort_unstable();
    assert_eq!(called, tools);
    // No other absolute path to a program is left (one the mapping did not see as a command).
    assert_eq!(script.matches("-T /usr/bin/codesign").count(), 1);
    let rest = mapped.replacen("-T /usr/bin/codesign", "", 1);
    assert!(
        !rest.contains("/usr/") && !rest.contains("/bin/") && !rest.contains("/sbin/"),
        "{rest}"
    );
    assert_eq!(
        to_shims("x -T /usr/bin/codesign | /bin/cat (/usr/sbin/y", shims),
        "x -T /usr/bin/codesign | /S/cat (/S/y"
    );
}
