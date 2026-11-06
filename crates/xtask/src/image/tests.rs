//! The checks of `image-recipe` on made-up input: the secret-looking keys, the pin URLs, and
//! every refusal of the pins and steps files, each by its full message.

use std::path::Path;

use super::*;

/// The pins file of the tests: every pin, both platforms, with `extra` lines added to the
/// `linux-x86_64.crun` pin, and the tools of [`TOOLS`] for each platform.
fn pins_text(extra: &str) -> String {
    let pin = |n: u8| {
        format!(
            "{{ url = \"https://example.org/f{n}\", sha256 = \"{}\" }}",
            hex(n)
        )
    };
    format!(
        "[linux-x86_64]\nbase = {}\nhostkeeper = {}\njs-agent = {}\n[linux-x86_64.crun]\nurl = \"https://example.org/crun\"\nsha256 = \"{}\"\n{extra}\n\
         [linux-x86_64.tools]\n{TOOLS}\n\
         [linux-arm64]\nbase = {}\ncrun = {}\nhostkeeper = {}\njs-agent = {}\n\
         [linux-arm64.tools]\n{TOOLS}\n",
        pin(1),
        pin(2),
        pin(3),
        hex(4),
        pin(5),
        pin(6),
        pin(7),
        pin(8),
    )
}

/// The tools table of each platform in [`pins_text`]: two packages.
const TOOLS: &str = "qemu-utils = { url = \"https://example.org/pool/qemu-utils_1_amd64.deb\", sha256 = \"9999999999999999999999999999999999999999999999999999999999999999\" }\n\
                     cloud-image-utils = { url = \"https://example.org/pool/cloud-image-utils_1_all.deb\", sha256 = \"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc\" }";

/// A digest of 64 hex digits, all `n`.
fn hex(n: u8) -> String {
    format!("{n:x}").repeat(64)
}

/// The message of `parse_pins` on `text`.
fn pins_error(text: &str) -> String {
    parse_pins(Path::new("pins.toml"), text)
        .expect_err("refused")
        .to_string()
}

/// The message of `parse_crun_steps` on `text`.
fn steps_error(text: &str) -> String {
    parse_crun_steps(Path::new("steps.toml"), text)
        .expect_err("refused")
        .to_string()
}

/// Asserts that a key naming a token, a password or another secret is secret-looking, in any
/// case and with `-` or `_`, and that a key naming a file or a path, or an ordinary key, is not.
#[test]
fn secret_looking_keys_are_named_by_their_words() {
    for key in [
        "token",
        "--token",
        "API_TOKEN",
        "api-key",
        "apikey",
        "client_secret",
        "Password",
        "db_passwd",
        "passphrase",
        "aws_access_key_id",
        "private-key",
        "PrivateKey",
        "AccessKey",
        "Credential",
        "bearer",
        "Authorization",
    ] {
        assert!(secret_looking(key), "{key}");
    }
    for key in [
        "token-file",
        "--token-file",
        "TOKEN_PATH",
        "url",
        "sha256",
        "ExecStart",
        "--server",
        "Options",
    ] {
        assert!(!secret_looking(key), "{key}");
    }
}

/// Asserts that the keys of a line are its `NAME=` assignments and its `--flags`, without the
/// dashes, and that a word with neither (a path, prose, a single-dash flag) is no key.
#[test]
fn keys_of_a_line_are_assignments_and_flags() {
    assert_eq!(
        keys_of("ExecStart=/usr/bin/a --server x --token-file /etc/js-agent/token -q"),
        ["ExecStart", "server", "token-file"]
    );
    assert_eq!(
        keys_of("Environment=\"JS_TOKEN=abc\" A=1"),
        ["Environment", "JS_TOKEN", "A"]
    );
    assert_eq!(
        keys_of("# the token is in /etc/js-agent/token"),
        Vec::<&str>::new()
    );
    assert_eq!(keys_of(""), Vec::<&str>::new());
}

/// Asserts that a pin URL is HTTPS with a host and a file, and that each other form is refused
/// with its own reason.
#[test]
fn pin_urls_are_https_with_no_user_query_or_fragment() {
    assert_eq!(check_url("https://example.org/a/b.deb"), Ok(()));
    for (url, why) in [
        ("http://example.org/a", "must be an https:// URL"),
        ("ftp://example.org/a", "must be an https:// URL"),
        (
            "https://example.org/a b",
            "must be printable ASCII with no space",
        ),
        (
            "https://example.org/\u{e9}",
            "must be printable ASCII with no space",
        ),
        (
            "https://example.org/a?X-Amz-Signature=1",
            "must not carry a query or a fragment (a signed URL holds a credential)",
        ),
        (
            "https://example.org/a#b",
            "must not carry a query or a fragment (a signed URL holds a credential)",
        ),
        (
            "https://user:pw@example.org/a",
            "must not name a user (a credential has no place in a pin)",
        ),
        ("https://example.org", "must name a host and a file"),
        ("https://example.org/", "must name a host and a file"),
        ("https:///a", "must name a host and a file"),
    ] {
        assert_eq!(check_url(url), Err(why), "{url}");
    }
}

/// Asserts that the pins of the tests parse, each platform to its own pins.
#[test]
fn a_whole_pins_file_parses() {
    let pins = parse_pins(Path::new("pins.toml"), &pins_text("")).expect("pins");
    let pin = |url: &str, n: u8| Pin {
        url: url.to_owned(),
        sha256: hex(n).parse().expect("sha"),
    };
    assert_eq!(pins.of(Arch::X86_64).base, pin("https://example.org/f1", 1));
    let tools: Vec<(&str, &str)> = pins
        .of(Arch::Arm64)
        .tools
        .iter()
        .map(|(name, pin)| (name.as_str(), pin.url.as_str()))
        .collect();
    assert_eq!(
        tools,
        [
            (
                "cloud-image-utils",
                "https://example.org/pool/cloud-image-utils_1_all.deb"
            ),
            (
                "qemu-utils",
                "https://example.org/pool/qemu-utils_1_amd64.deb"
            ),
        ]
    );
    assert_eq!(
        pins.of(Arch::X86_64).crun,
        pin("https://example.org/crun", 4)
    );
    assert_eq!(
        pins.of(Arch::X86_64).hostkeeper,
        pin("https://example.org/f2", 2)
    );
    assert_eq!(
        pins.of(Arch::X86_64).js_agent,
        pin("https://example.org/f3", 3)
    );
    assert_eq!(pins.of(Arch::Arm64).base, pin("https://example.org/f5", 5));
    assert_eq!(
        pins.of(Arch::Arm64).js_agent,
        pin("https://example.org/f8", 8)
    );
}

/// Asserts that a secret-looking key is refused by its dotted name wherever it is (a pin, a
/// platform, the top, a table in an array), before any check of the fields.
#[test]
fn a_secret_looking_pins_key_is_refused_by_name() {
    let message = |at: &str, key: &str| {
        format!(
            "pins.toml: {at}: the key {key:?} looks like secret material, and a machine image \
             carries none (its configuration comes from the provider's user data at boot)"
        )
    };
    assert_eq!(
        pins_error(&pins_text("api_token = \"t\"")),
        message("linux-x86_64.crun.api_token", "api_token")
    );
    assert_eq!(
        pins_error("password = 1\n"),
        message("password", "password")
    );
    assert_eq!(
        pins_error("[linux-arm64]\nsecret = { a = 1 }\n"),
        message("linux-arm64.secret", "secret")
    );
    assert_eq!(
        pins_error("x = [1, { bearer = \"b\" }]\n"),
        message("x.bearer", "bearer")
    );
}

/// Asserts each refusal of a pins file that holds no secret: not TOML, an unknown key at each
/// level, a missing platform, pin or field, a value of the wrong form, and a bad URL or digest.
#[test]
fn each_malformed_pins_file_is_refused_with_its_field() {
    assert_eq!(
        pins_error("[linux-x86_64"),
        "pins.toml: TOML parse error at line 1, column 14\n  |\n1 | [linux-x86_64\n  |              ^\nunclosed table, expected `]`"
    );
    assert_eq!(
        pins_error(&format!("{}[linux-riscv64]\n", pins_text(""))),
        "pins.toml: linux-riscv64: is not a known key"
    );
    assert_eq!(
        pins_error("[linux-arm64]\n"),
        "pins.toml: linux-x86_64: is missing"
    );
    assert_eq!(
        pins_error("linux-x86_64 = 1\n"),
        "pins.toml: linux-x86_64: must be a table"
    );
    let x86 = pins_text("");
    assert_eq!(
        pins_error(&x86.replace("js-agent = {", "js-agnet = {")),
        "pins.toml: linux-x86_64.js-agnet: is not a known key"
    );
    assert_eq!(
        pins_error(&x86.replacen("base = {", "bass = {", 1)),
        "pins.toml: linux-x86_64.bass: is not a known key"
    );
    assert_eq!(
        pins_error(&pins_text("size = 1")),
        "pins.toml: linux-x86_64.crun.size: is not a known key"
    );
    assert_eq!(
        pins_error(&x86.replacen(
            &format!(
                "base = {{ url = \"https://example.org/f1\", sha256 = \"{}\" }}",
                hex(1)
            ),
            "base = 1",
            1
        )),
        "pins.toml: linux-x86_64.base: must be a table"
    );
    assert_eq!(
        pins_error(&x86.replace("url = \"https://example.org/crun\"\n", "")),
        "pins.toml: linux-x86_64.crun.url: is missing"
    );
    assert_eq!(
        pins_error(&x86.replace("url = \"https://example.org/crun\"", "url = 1")),
        "pins.toml: linux-x86_64.crun.url: must be a string"
    );
    assert_eq!(
        pins_error(&x86.replace("https://example.org/crun", "http://example.org/crun")),
        "pins.toml: linux-x86_64.crun.url: must be an https:// URL"
    );
    assert_eq!(
        pins_error(&x86.replace(&format!("sha256 = \"{}\"\n\n", hex(4)), "\n")),
        "pins.toml: linux-x86_64.crun.sha256: is missing"
    );
    assert_eq!(
        pins_error(&x86.replace(&hex(4), &"A".repeat(64))),
        format!(
            "pins.toml: linux-x86_64.crun.sha256: not a SHA-256: {:?} (expected 64 lowercase hex digits)",
            "A".repeat(64)
        )
    );
    for (name, n) in [("hostkeeper", 2), ("js-agent", 3)] {
        let line = format!(
            "{name} = {{ url = \"https://example.org/f{n}\", sha256 = \"{}\" }}\n",
            hex(n)
        );
        assert_eq!(
            pins_error(&x86.replacen(&line, "", 1)),
            format!("pins.toml: linux-x86_64.{name}: is missing")
        );
    }
    let crun = format!(
        "[linux-x86_64.crun]\nurl = \"https://example.org/crun\"\nsha256 = \"{}\"\n",
        hex(4)
    );
    assert_eq!(
        pins_error(&x86.replacen(&crun, "", 1)),
        "pins.toml: linux-x86_64.crun: is missing"
    );
    assert_eq!(
        pins_error(&x86.replace("[linux-arm64]\nbase = {", "[linux-arm64]\nbas = {")),
        "pins.toml: linux-arm64.bas: is not a known key"
    );
}

/// The steps file of the tests: one step, with `extra` lines added to it.
fn one_step(extra: &str) -> String {
    format!("[[step]]\nname = \"packages\"\nwhat = \"crun\"\nfunction = \"step_packages\"\n{extra}")
}

/// Asserts that a steps file parses to its steps in order.
#[test]
fn a_steps_file_parses_in_order() {
    let text = format!(
        "{}{}",
        one_step(""),
        one_step("").replace("packages", "user")
    );
    let steps = parse_crun_steps(Path::new("steps.toml"), &text).expect("steps");
    let names: Vec<&str> = steps.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["packages", "user"]);
    assert_eq!(steps[1].function, "step_user");
    assert_eq!(steps[0].what, "crun");
}

/// Asserts each refusal of a steps file: not TOML, an unknown key at the top or in a step, no
/// step, steps that are not tables, and a missing or non-string field.
#[test]
fn each_malformed_steps_file_is_refused_with_its_field() {
    assert_eq!(
        steps_error("x = "),
        "steps.toml: TOML parse error at line 1, column 5\n  |\n1 | x = \n  |     ^\nstring values must be quoted, expected literal string"
    );
    assert_eq!(
        steps_error("other = 1\n"),
        "steps.toml: other: is not a known key"
    );
    assert_eq!(steps_error(""), "steps.toml: step: is missing");
    assert_eq!(
        steps_error("step = []\n"),
        "steps.toml: step: must be an array of one or more tables"
    );
    assert_eq!(
        steps_error("step = 1\n"),
        "steps.toml: step: must be an array of one or more tables"
    );
    assert_eq!(
        steps_error("step = [1]\n"),
        "steps.toml: step[0]: must be a table"
    );
    assert_eq!(
        steps_error(&one_step("when = \"x\"\n")),
        "steps.toml: step[0].when: is not a known key"
    );
    assert_eq!(
        steps_error(&one_step("").replace("what = \"crun\"\n", "")),
        "steps.toml: step[0].what: is missing"
    );
    assert_eq!(
        steps_error(&one_step("").replace("name = \"packages\"\n", "")),
        "steps.toml: step[0].name: is missing"
    );
    assert_eq!(
        steps_error(&one_step("").replace("function = \"step_packages\"", "function = 1")),
        "steps.toml: step[0].function: must be a string"
    );
}

/// Asserts that the line of a baked file holding a secret-looking key is named, the first one
/// when there are several, and that a file with none passes.
#[test]
fn a_secret_looking_key_in_a_baked_file_is_refused_by_line() {
    let path = Path::new("a.service");
    assert!(refuse_secret_text(path, "[Service]\nExecStart=/a --token-file /t\n").is_ok());
    let refused = refuse_secret_text(path, "[Service]\nEnvironment=JS_TOKEN=x\nX=--password y\n")
        .expect_err("refused");
    assert_eq!(
        refused.to_string(),
        "a.service: line 2: the key \"JS_TOKEN\" looks like secret material, and a machine image \
         carries none (its configuration comes from the provider's user data at boot)"
    );
}

/// Asserts that the platforms are named as the pins file and the provider's `images` table name
/// them.
#[test]
fn platforms_are_named_for_the_pins_and_the_images_keys() {
    let names: Vec<(&str, &str)> = ARCHES
        .iter()
        .map(|a| (a.platform(), a.images_key()))
        .collect();
    assert_eq!(
        names,
        [("linux-x86_64", "x86_64"), ("linux-arm64", "arm64")]
    );
}

/// Asserts each refusal of a platform's tools: none, not a table, empty, a tool that is not a
/// pin, a URL that does not name a `.deb`, and two tools that download to the same file name.
#[test]
fn each_malformed_tools_table_is_refused_with_its_field() {
    let text = pins_text("");
    let x86_tools = format!("[linux-x86_64.tools]\n{TOOLS}\n");
    let with = |tools: &str| text.replacen(&x86_tools, tools, 1);
    assert_ne!(with(""), text);
    assert_eq!(
        pins_error(&with("")),
        "pins.toml: linux-x86_64.tools: is missing"
    );
    assert_eq!(
        pins_error(
            &text
                .replacen("[linux-x86_64]\n", "[linux-x86_64]\ntools = 1\n", 1)
                .replacen(&x86_tools, "", 1)
        ),
        "pins.toml: linux-x86_64.tools: must be a table"
    );
    assert_eq!(
        pins_error(&with("[linux-x86_64.tools]\n")),
        "pins.toml: linux-x86_64.tools: must pin one or more packages"
    );
    assert_eq!(
        pins_error(&with("[linux-x86_64.tools]\nqemu = 1\n")),
        "pins.toml: linux-x86_64.tools.qemu: must be a table"
    );
    let tool = |name: &str, url: &str| {
        format!("{name} = {{ url = \"{url}\", sha256 = \"{}\" }}\n", hex(9))
    };
    assert_eq!(
        pins_error(&with(&format!(
            "[linux-x86_64.tools]\n{}",
            tool("qemu", "https://example.org/qemu.tar.gz")
        ))),
        "pins.toml: linux-x86_64.tools.qemu.url: must name a .deb file"
    );
    assert_eq!(
        pins_error(&with(&format!(
            "[linux-x86_64.tools]\n{}{}",
            tool("a", "https://example.org/one/q.deb"),
            tool("b", "https://example.org/two/q.deb")
        ))),
        "pins.toml: linux-x86_64.tools.b.url: names q.deb, which another tool names too"
    );
}
