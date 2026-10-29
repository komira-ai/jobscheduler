//! The renderer: masked and real text differ only at the secret bytes, slots are filled
//! only in the real render, a missing slot is refused, and the hardening list of the
//! script holds.

use js_elastic::{BootPlan, BootStep, Secret, SlotName};
use js_lease_server::bootstrap::{
    BootSecrets, CLOUD_INIT_TRACES, Fill, JOIN_TOKEN_SLOT, MASK, RenderError, TOKEN_PATH, payload,
    render,
};
use js_lease_server::network::{AUTH_KEY_PATH, AUTH_KEY_SLOT, NetworkMode};

use crate::fake::Fake;
use crate::support::{TEN_MINUTES, clock, ctx, host, mode, plan};

const JOIN: &str = "join-token-MARK-5e1c";
const KEY: &str = "tskey-auth-MARK-9f3a";

fn tailscale_plan() -> BootPlan {
    let host = host();
    let fake = Fake::empty();
    let (_, shared) = clock();
    let mut plan = plan(&host);
    plan.network = mode(&fake, shared).describe(&ctx(&host, TEN_MINUTES));
    plan
}

fn secrets() -> BootSecrets {
    BootSecrets::new(
        Secret::new(JOIN.to_owned()),
        vec![(
            SlotName(AUTH_KEY_SLOT.to_owned()),
            Secret::new(KEY.to_owned()),
        )],
    )
    .unwrap()
}

/// Asserts that the real render is the masked render with each secret put where the mask
/// was: replacing the secrets by the mask in the real text gives the masked text, and
/// that each secret appears in the real text once and in the masked text never.
#[test]
fn masked_and_real_differ_only_at_the_secret_bytes() {
    let plan = tailscale_plan();
    let masked = render(&plan, Fill::Mask).unwrap();
    let secrets = secrets();
    let real = render(&plan, Fill::Real(&secrets)).unwrap();
    assert_ne!(masked, real);
    assert_eq!(real.matches(JOIN).count(), 1);
    assert_eq!(real.matches(KEY).count(), 1);
    assert!(!masked.contains(JOIN) && !masked.contains(KEY));
    assert_eq!(masked.matches(MASK).count(), 2);
    assert_eq!(real.replace(JOIN, MASK).replace(KEY, MASK), masked);
}

/// Asserts that a masked render needs no secret (it is what plan mode shows, where
/// nothing is minted), and that the real payload is the real render, delivered as it is.
#[test]
fn only_the_real_render_holds_secrets_and_it_becomes_the_payload() {
    let plan = tailscale_plan();
    assert!(render(&plan, Fill::Mask).is_ok());
    let secrets = secrets();
    let real = render(&plan, Fill::Real(&secrets)).unwrap();
    let body = payload(&plan, &secrets, usize::MAX).unwrap();
    assert_eq!(body.expose(), real);
    assert_eq!(body.len(), real.len());
    assert!(!format!("{body:?}").contains(JOIN));
}

/// Asserts that a slot a step names and nothing fills is refused naming the slot, that a
/// secret for a slot no step names is refused, and that two secrets for one slot (the
/// join token's included) are refused.
#[test]
fn a_missing_unused_or_duplicate_slot_is_refused() {
    let plan = tailscale_plan();
    let no_key = BootSecrets::new(Secret::new(JOIN.to_owned()), Vec::new()).unwrap();
    assert_eq!(
        render(&plan, Fill::Real(&no_key)).unwrap_err(),
        RenderError::MissingSlot(AUTH_KEY_SLOT.to_owned())
    );

    let plain = plan_without_network();
    let extra = BootSecrets::new(
        Secret::new(JOIN.to_owned()),
        vec![(
            SlotName("network.extra".to_owned()),
            Secret::new(KEY.to_owned()),
        )],
    )
    .unwrap();
    assert_eq!(
        render(&plain, Fill::Real(&extra)).unwrap_err(),
        RenderError::UnusedSlot("network.extra".to_owned())
    );

    let twice = |slot: &str| {
        BootSecrets::new(
            Secret::new(JOIN.to_owned()),
            vec![
                (SlotName(slot.to_owned()), Secret::new("a".to_owned())),
                (SlotName(slot.to_owned()), Secret::new("b".to_owned())),
            ],
        )
        .unwrap_err()
    };
    assert_eq!(
        twice("network.k"),
        RenderError::DuplicateSlot("network.k".to_owned())
    );
    let join_again = BootSecrets::new(
        Secret::new(JOIN.to_owned()),
        vec![(
            SlotName(JOIN_TOKEN_SLOT.to_owned()),
            Secret::new("b".to_owned()),
        )],
    )
    .unwrap_err();
    assert_eq!(
        join_again,
        RenderError::DuplicateSlot(JOIN_TOKEN_SLOT.to_owned())
    );
}

fn plan_without_network() -> BootPlan {
    plan(&host())
}

/// Asserts the hardening list of the script: the cleanup trap is the first command and
/// the cleanup removes every secret file a step writes and every cloud-init trace; no
/// `set -x`; no `echo`; each secret file is made 0600 right after it is written; each
/// download is followed by a check of its hash and goes over https only; the join token
/// file stays for the agent.
#[test]
fn the_script_is_hardened() {
    let plan = tailscale_plan();
    let secrets = secrets();
    let text = render(&plan, Fill::Real(&secrets)).unwrap();
    let lines: Vec<&str> = text.lines().collect();

    assert_eq!(lines[0], "#!/bin/sh");
    assert_eq!(
        lines[1], "trap cleanup EXIT",
        "the trap is the first command"
    );
    let cleanup: String = lines[2..6].join("\n");
    assert!(
        cleanup.contains(&format!("rm -f -- {AUTH_KEY_PATH}")),
        "{cleanup}"
    );
    for trace in CLOUD_INIT_TRACES {
        assert!(cleanup.contains(trace), "{trace} is not removed");
    }
    assert!(
        !cleanup.contains(TOKEN_PATH),
        "the agent reads the join token file"
    );

    assert!(!text.contains("set -x"));
    assert!(!lines.iter().any(|l| l.trim_start().starts_with("echo")));

    for (at, line) in lines.iter().enumerate() {
        if let Some(rest) = line.strip_prefix("printf '%s' ") {
            let path = rest.rsplit_once("> ").unwrap().1;
            assert_eq!(lines[at + 1], format!("chmod 0600 -- {path}"));
        }
        if line.starts_with("curl ") {
            assert!(
                line.contains("--fail") && line.contains("--proto =https"),
                "{line}"
            );
            assert!(
                lines[at + 1].ends_with("| sha256sum --check --status -"),
                "{line}"
            );
        }
    }
    assert_eq!(
        text.matches("curl ").count(),
        2,
        "the archive and the agent"
    );
    let agent_check = lines.iter().position(|l| l.contains("chmod 0755")).unwrap();
    let start = lines
        .iter()
        .position(|l| l.starts_with("systemctl enable"))
        .unwrap();
    assert!(
        agent_check < start,
        "the agent is checked before it can start"
    );
}

/// Asserts that no step can add shell syntax: an argument with a quote, a `;`, `$(...)`,
/// a backquote, a glob and a newline is one word, quoted; and a secret with a quote and
/// a `$` is written as one quoted string.
#[test]
fn nothing_in_a_step_or_a_secret_adds_shell_syntax() {
    let mut plan = plan_without_network();
    plan.host_prep = vec![BootStep::Run(vec![
        "echo".to_owned(),
        "a'b; rm -rf /".to_owned(),
        "$(id)".to_owned(),
        "`id`".to_owned(),
        "*".to_owned(),
        "x\ny".to_owned(),
        String::new(),
    ])];
    let masked = render(&plan, Fill::Mask).unwrap();
    assert!(
        masked.contains("echo 'a'\\''b; rm -rf /' '$(id)' '`id`' '*' 'x\ny' ''\n"),
        "{masked}"
    );

    let tricky = BootSecrets::new(Secret::new("it's $HOME; `x`".to_owned()), Vec::new()).unwrap();
    let real = render(&plan, Fill::Real(&tricky)).unwrap();
    assert!(real.contains("printf '%s' 'it'\\''s $HOME; `x`' > /etc/jobscheduler/token\n"));
}

/// Asserts that the unit's command line is escaped for systemd (a `%` and a `$` are
/// doubled, a `"` and a `\` are escaped), so a scheduler URL cannot add an argument or
/// be expanded, and that the instance line is `<pool>:<name>`.
#[test]
fn the_unit_and_the_instance_line_carry_the_plan_values() {
    let mut plan = plan_without_network();
    plan.scheduler_url = r#"https://h.example:7301/%h"$x\y"#.to_owned();
    let text = render(&plan, Fill::Mask).unwrap();
    assert!(
        text.contains(r#"--server "https://h.example:7301/%%h\"$$x\\y" --token-file"#),
        "{text}"
    );
    assert!(text.contains(&format!(
        "printf '%s\\n' 'gpu:{}' > /etc/jobscheduler/instance",
        plan.host
    )));
}

/// Asserts that no refusal quotes a secret: every error of a render with unusable and
/// misplaced secrets prints, in `Display` and `Debug`, without the secret's text; the
/// secrets set prints `***`.
#[test]
fn render_errors_and_secrets_never_print_a_secret() {
    let plan = tailscale_plan();
    let mut shown = String::new();
    let bad = BootSecrets::new(
        Secret::new(JOIN.to_owned()),
        vec![(
            SlotName(AUTH_KEY_SLOT.to_owned()),
            Secret::new(format!("{KEY}\0")),
        )],
    )
    .unwrap();
    shown.push_str(&format!("{bad:?}\n"));
    let err = render(&plan, Fill::Real(&bad)).unwrap_err();
    shown.push_str(&format!("{err} {err:?}\n"));
    let missing = BootSecrets::new(Secret::new(JOIN.to_owned()), Vec::new()).unwrap();
    let err = render(&plan, Fill::Real(&missing)).unwrap_err();
    shown.push_str(&format!("{err} {err:?} {:?}\n", Fill::Real(&missing)));
    let err = payload(&plan, &secrets(), 10).unwrap_err();
    shown.push_str(&format!("{err} {err:?}\n"));
    assert!(!shown.contains(JOIN) && !shown.contains(KEY), "{shown}");
    assert!(shown.contains("***"));
}

/// Asserts that a fetch URL or a path that starts with `-` cannot become an option: the
/// URL follows `--url`, the destination follows `--output`, and the secret file's path
/// follows `--` for `chmod`, as does each path of the cleanup's `rm`.
#[test]
fn a_url_or_path_that_starts_with_a_dash_is_not_an_option() {
    let mut plan = plan_without_network();
    plan.host_prep = vec![
        BootStep::Fetch {
            url: "--config=/x".to_owned(),
            sha256: [1; 32],
            dest: "-o".to_owned(),
        },
        BootStep::SecretFile {
            path: "-k".to_owned(),
            slot: SlotName("k".to_owned()),
        },
    ];
    let text = render(&plan, Fill::Mask).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert!(
        lines.contains(
            &"curl --fail --silent --show-error --location --proto =https --output -o --url --config=/x"
        ),
        "{text}"
    );
    assert!(lines.contains(&"chmod 0600 -- -k"), "{text}");
    assert!(lines.contains(&"  rm -f -- -k"), "{text}");
}
