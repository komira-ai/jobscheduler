//! The optional pre-start fragment: absent means the document is what it was, present
//! means a oneshot unit the agent requires, and the text is checked and carried safely.

use js_provider_lambdalabs::{Bootstrap, BootstrapError, Fault, MAX_PRESTART_BYTES};

const GOLDEN: &str = include_str!("../fixtures/bootstrap_user_data.sh");

const URL: &str =
    "https://github.com/example-org/jobscheduler/releases/download/v0.1.0/js-agent-x86_64-linux";
const SHA: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// The example from the module docs.
const EXAMPLE: &str = "wg-quick up wg-agent\n";

const OPEN: &str = "put /usr/local/lib/js-agent/prestart.sh <<'JS_AGENT_PRESTART_END'\n";
const CLOSE: &str = "\nJS_AGENT_PRESTART_END\n";

fn with(prestart: &str) -> Bootstrap<'_> {
    Bootstrap {
        agent_url: URL,
        agent_sha256: SHA,
        server_url: "https://lease.example.org:8443",
        pool: "lambda-gpu",
        ca_cert: "/etc/js-agent/ca.pem",
        token_file: "/etc/js-agent/token",
        state_root: "/var/lib/js-agent/state",
        scratch_root: "/mnt/scratch/js-agent",
        scratch_gib: None,
        container_bin: "/usr/bin/docker",
        prestart,
    }
}

fn render(prestart: &str) -> String {
    with(prestart).render().unwrap().as_str().to_owned()
}

/// The text between `start` and the next `end`, both excluded.
fn between<'a>(text: &'a str, start: &str, end: &str) -> &'a str {
    let from = text.find(start).unwrap_or_else(|| panic!("no {start:?}")) + start.len();
    let len = text[from..]
        .find(end)
        .unwrap_or_else(|| panic!("no {end:?}"));
    &text[from..from + len]
}

fn agent_unit(text: &str) -> &str {
    between(
        text,
        "put \"$UNIT_DIR/js-agent.service\" <<EOF\n",
        "\nEOF\n",
    )
}

fn prestart_unit(text: &str) -> &str {
    between(
        text,
        "put \"$UNIT_DIR/js-agent-prestart.service\" <<EOF\n",
        "\nEOF\n",
    )
}

fn refused(prestart: &str) -> Fault {
    match with(prestart).render() {
        Err(BootstrapError::Field { field, fault }) => {
            assert_eq!(field, "prestart");
            fault
        }
        other => panic!("{prestart:?} gave {other:?}"),
    }
}

#[test]
fn an_empty_fragment_leaves_the_document_and_the_agent_unit_as_they_were() {
    let text = render("");
    // Byte for byte the document from before the fragment existed.
    assert_eq!(text, GOLDEN);
    assert!(!text.contains("prestart"));
    assert!(!text.contains("PRESTART"));
    assert!(!text.contains("@"));
    assert_eq!(
        agent_unit(&text),
        "[Unit]\n\
         Description=js-agent: offers this machine to the lease server\n\
         After=network-online.target docker.service\n\
         Wants=network-online.target\n\
         RequiresMountsFor=$SCRATCH_ROOT\n\
         ConditionPathExists=$TOKEN_FILE\n\
         ConditionPathExists=$CA_CERT\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart=$AGENT_BIN --server $SERVER_URL --pool $POOL --ca-cert $CA_CERT \
         --token-file $TOKEN_FILE --container-bin $CONTAINER_BIN --state-root $STATE_ROOT \
         --scratch-root $SCRATCH_ROOT\n\
         Restart=on-failure\n\
         RestartSec=10"
    );
}

#[test]
fn a_fragment_installs_a_oneshot_unit_that_the_agent_requires_and_follows() {
    let text = render(EXAMPLE);
    let unit = prestart_unit(&text);
    assert!(unit.contains("\nType=oneshot\n"), "{unit}");
    assert!(unit.contains("\nRemainAfterExit=yes\n"), "{unit}");
    assert!(
        unit.contains("\nExecStart=/bin/bash -e -o pipefail /usr/local/lib/js-agent/prestart.sh\n")
    );
    let agent = agent_unit(&text);
    assert!(
        agent.contains("\nRequires=js-agent-prestart.service\n"),
        "{agent}"
    );
    assert!(
        agent.contains("\nAfter=network-online.target docker.service js-agent-prestart.service\n"),
        "{agent}"
    );
    // Everything else in the agent unit is as it was.
    let plain = agent
        .replace("Requires=js-agent-prestart.service\n", "")
        .replace(" js-agent-prestart.service", "");
    assert_eq!(plain, agent_unit(GOLDEN));
    // The text of the fragment is in the document.
    assert_eq!(between(&text, OPEN, CLOSE), "wg-quick up wg-agent");
}

#[test]
fn the_fragment_runs_after_the_network_and_the_scratch_mount_and_before_the_agent_can_start() {
    let text = render(EXAMPLE);
    let unit = prestart_unit(&text);
    assert!(unit.contains("\nAfter=network-online.target\n"));
    assert!(unit.contains("\nWants=network-online.target\n"));
    assert!(unit.contains("\nRequiresMountsFor=$SCRATCH_ROOT\n"));
    assert!(unit.contains("\nWantedBy=multi-user.target"));
    let at = |needle: &str| text.find(needle).unwrap_or_else(|| panic!("no {needle:?}"));
    let start = at("systemctl enable --now js-agent-prestart.service");
    assert!(at("systemctl daemon-reload") > at("js-agent-prestart.service\" <<EOF"));
    assert!(at("mountpoint -q \"$SCRATCH_ROOT\"") < start);
    assert!(start < at("systemctl enable --now js-agent.path"));
    // A failed fragment fails the bootstrap.
    assert!(
        text.contains("js-agent-prestart.service ||\n    fail \"the pre-start fragment failed")
    );
}

#[test]
fn the_fragment_is_written_through_a_quoted_heredoc_and_arrives_unchanged() {
    let nasty = "# EOF\nEOF\nx=\"$HOME\" y='$(id)' z=`id`\necho \\$a \\\\ \"q\" 'q' %h @POOL@ @AGENT_URL@\n\
                 cat <<'EOF'\n$not_expanded\nEOF\n\ttabbed\n";
    let text = render(nasty);
    assert_eq!(between(&text, OPEN, CLOSE), nasty.trim_end_matches('\n'));
    // The delimiter is quoted (nothing in the text is expanded by the bootstrap) and
    // appears twice: where it opens and where it closes.
    assert_eq!(text.matches("JS_AGENT_PRESTART_END").count(), 2);
    // The bootstrap's own heredocs are not cut short by a line in the fragment.
    let unit = prestart_unit(&text);
    assert!(unit.starts_with("[Unit]\n") && unit.ends_with("WantedBy=multi-user.target"));
    // Inputs named in the text are not substituted into it.
    assert!(text.contains("%h @POOL@ @AGENT_URL@"));
    // The part of the document outside the fragment is the same as with a plain one.
    let plain = render("true");
    assert_eq!(text.replace(nasty.trim_end_matches('\n'), "true"), plain);
}

#[test]
fn trailing_newlines_do_not_change_the_document() {
    assert_eq!(render("true"), render("true\n"));
    assert_eq!(render("true"), render("true\n\n\n"));
    assert_ne!(render("true"), render("true # more"));
}

#[test]
fn the_delimiter_in_the_fragment_is_refused() {
    for text in [
        "JS_AGENT_PRESTART_END",
        "true\nJS_AGENT_PRESTART_END\nrm -rf /\n",
        "echo xJS_AGENT_PRESTART_ENDx",
        "# JS_AGENT_PRESTART_END",
    ] {
        assert_eq!(refused(text), Fault::Delimiter, "{text:?}");
    }
}

#[test]
fn a_fragment_over_the_size_cap_is_refused_and_one_at_the_cap_is_not() {
    assert_eq!(MAX_PRESTART_BYTES, 4096);
    let line = "echo ok # padding padding padding\n";
    let at_cap = line.repeat(MAX_PRESTART_BYTES / line.len() + 1);
    let at_cap = &at_cap[..MAX_PRESTART_BYTES];
    assert_eq!(at_cap.len(), MAX_PRESTART_BYTES);
    assert!(with(at_cap).render().is_ok());
    let over = format!("{at_cap}#");
    assert_eq!(refused(&over), Fault::TooLarge);
    assert_eq!(
        with(&over).render().unwrap_err().to_string(),
        "prestart: is larger than 4096 bytes"
    );
}

#[test]
fn a_nul_or_a_control_character_in_the_fragment_is_refused() {
    assert_eq!(refused("true\0"), Fault::Nul);
    assert_eq!(refused("\0"), Fault::Nul);
    for text in [
        "true\r\n",
        "echo \x07",
        "echo \x1b[0m",
        "echo \x7f",
        "a\u{85}b",
    ] {
        assert_eq!(refused(text), Fault::Control, "{text:?}");
    }
    // A tab and newlines are text.
    assert!(with("if true; then\n\techo ok\nfi\n").render().is_ok());
}

#[test]
fn a_blank_fragment_is_refused_and_only_the_empty_one_means_none() {
    for text in [" ", "\n", " \t\n\n"] {
        assert_eq!(refused(text), Fault::Empty, "{text:?}");
    }
    assert!(with("").render().is_ok());
}

#[test]
fn a_token_shaped_run_in_the_fragment_is_refused() {
    let key = "A".repeat(32);
    assert_eq!(refused(&format!("echo {key}")), Fault::TokenShaped);
    assert_eq!(
        refused("wg set wg0 private-key <(echo 0123456789abcdefghijklmnopqrstuvwxyzABCD=)"),
        Fault::TokenShaped
    );
    assert!(with(&format!("echo {}", "A".repeat(31))).render().is_ok());
    assert!(
        with(&format!("echo {}-{}", "A".repeat(31), "B".repeat(31)))
            .render()
            .is_ok()
    );
}

#[test]
fn every_refusal_of_the_fragment_names_the_field() {
    for text in [" ", "a\0", "a\x07", "JS_AGENT_PRESTART_END"] {
        let message = with(text).render().unwrap_err().to_string();
        assert!(message.starts_with("prestart: "), "{message}");
    }
}

#[test]
fn the_fragment_is_given_no_token_and_no_input() {
    let text = render(EXAMPLE);
    // From where the fragment's file is written to the end of its unit: no mention of the
    // token file, the token, the bootstrap's variables, or an export.
    let from = text
        .find("mkdir -p -m 0755 /usr/local/lib/js-agent")
        .unwrap();
    let block = &text[from..from + text[from..].find("\nEOF\n").unwrap()];
    // Mask the fragment's own text; the rest is the bootstrap's.
    let block = block.replace("wg-quick up wg-agent", "");
    for needle in [
        "TOKEN",
        "token",
        "export",
        "CA_CERT",
        "SERVER_URL",
        "Environment",
    ] {
        assert!(!block.contains(needle), "{needle}: {block}");
    }
    // The unit sets no environment and reads no environment file.
    assert!(!prestart_unit(&text).contains("Environment"));
    // The token is delivered later, by something else: nothing here writes the token file.
    assert!(!text.contains(">\"$TOKEN_FILE\""));
}

#[test]
fn nothing_rendered_names_a_network_product_or_a_host() {
    for text in [render(""), render(EXAMPLE), render("true")] {
        let lower = text.to_ascii_lowercase();
        for word in [
            "tailscale",
            "tailnet",
            "tailscaled",
            "ts.net",
            "headscale",
            "zerotier",
            "100.64.",
        ] {
            assert!(!lower.contains(word), "{word}");
        }
    }
}
