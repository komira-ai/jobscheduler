//! Every limit of the renderer, tested at the bound: the step count, the secret size, the
//! payload size, and the values a line cannot carry.

use js_elastic::{BootPlan, BootStep, Secret, SlotName};
use js_lease_server::bootstrap::{
    BootSecrets, Fill, MAX_SECRET_BYTES, MAX_STEPS, RenderError, payload, render,
};

use crate::support::{host, plan};

fn runs(count: usize) -> Vec<BootStep> {
    vec![BootStep::Run(vec!["true".to_owned()]); count]
}

fn join_only(token: &str) -> BootSecrets {
    BootSecrets::new(Secret::new(token.to_owned()), Vec::new()).unwrap()
}

/// Asserts that a plan of exactly the step limit renders, the provider's steps and the
/// network's counted together, and that one more is refused with the count, in the masked
/// render as in the real one.
#[test]
fn a_plan_may_hold_the_step_limit_and_not_one_more() {
    let mut at = plan(&host());
    at.host_prep = runs(MAX_STEPS - 1);
    at.network = runs(1);
    assert!(render(&at, Fill::Mask).is_ok());
    assert!(render(&at, Fill::Real(&join_only("t"))).is_ok());

    let mut over = at.clone();
    over.network = runs(2);
    let expected = RenderError::TooManySteps(MAX_STEPS + 1);
    assert_eq!(render(&over, Fill::Mask).unwrap_err(), expected);
    assert_eq!(
        render(&over, Fill::Real(&join_only("t"))).unwrap_err(),
        expected
    );
    assert_eq!(MAX_STEPS, 64);
}

/// Asserts that a secret of exactly the size limit is written and one byte more is
/// refused, naming the slot; and that an empty secret and one with a NUL are refused.
#[test]
fn a_secret_is_one_byte_to_the_size_limit_with_no_nul() {
    let plan = plan(&host());
    let at = join_only(&"a".repeat(MAX_SECRET_BYTES));
    let text = render(&plan, Fill::Real(&at)).unwrap();
    assert!(text.contains(&"a".repeat(MAX_SECRET_BYTES)));
    for bad in [
        "a".repeat(MAX_SECRET_BYTES + 1),
        String::new(),
        "a\0b".to_owned(),
    ] {
        assert_eq!(
            render(&plan, Fill::Real(&join_only(&bad))).unwrap_err(),
            RenderError::BadSecret("join_token".to_owned()),
            "{} bytes",
            bad.len()
        );
    }
    // Masked, a secret is not read at all.
    assert!(render(&plan, Fill::Mask).is_ok());
    assert_eq!(MAX_SECRET_BYTES, 4096);
}

/// Asserts that a payload exactly the provider's limit is delivered and one byte over is
/// refused with both sizes.
#[test]
fn a_payload_may_be_the_providers_limit_and_not_one_byte_more() {
    let plan = plan(&host());
    let secrets = join_only("t");
    let size = render(&plan, Fill::Real(&secrets)).unwrap().len();
    assert_eq!(payload(&plan, &secrets, size).unwrap().len(), size);
    assert_eq!(
        payload(&plan, &secrets, size - 1).unwrap_err(),
        RenderError::PayloadTooLarge {
            size,
            limit: size - 1
        }
    );
    assert_eq!(
        payload(&plan, &secrets, size - 1).unwrap_err().to_string(),
        format!(
            "the boot payload is {size} bytes and the provider carries at most {}",
            size - 1
        )
    );
}

/// Asserts that a run step with no program, and a scheduler URL or pool with a control
/// character, are refused, and the messages say which.
#[test]
fn a_run_without_a_program_and_a_value_with_a_line_break_are_refused() {
    let mut empty = plan(&host());
    empty.network = vec![BootStep::Run(Vec::new())];
    assert_eq!(
        render(&empty, Fill::Mask).unwrap_err(),
        RenderError::EmptyRun
    );

    let with = |edit: fn(&mut BootPlan)| {
        let mut p = plan(&host());
        edit(&mut p);
        render(&p, Fill::Mask).unwrap_err()
    };
    let url = with(|p| p.scheduler_url = "https://h\nExecStartPost=x".to_owned());
    assert_eq!(url, RenderError::BadValue("scheduler url"));
    let pool = with(|p| p.pool = "gpu\r".to_owned());
    assert_eq!(pool, RenderError::BadValue("pool"));
    assert_eq!(
        url.to_string(),
        "the scheduler url must not hold a control character"
    );
}

/// Asserts that a slot name is just a name: a step may name any slot the secrets fill, and
/// `SecretFile` steps from the provider's `host_prep` are filled and cleaned up like the
/// network's.
#[test]
fn a_host_prep_secret_file_is_filled_and_cleaned_up_too() {
    let mut p = plan(&host());
    p.host_prep.push(BootStep::SecretFile {
        path: "/etc/prep key".to_owned(),
        slot: SlotName("host_prep.key".to_owned()),
    });
    let secrets = BootSecrets::new(
        Secret::new("t".to_owned()),
        vec![(
            SlotName("host_prep.key".to_owned()),
            Secret::new("k".to_owned()),
        )],
    )
    .unwrap();
    let text = render(&p, Fill::Real(&secrets)).unwrap();
    assert!(text.contains("  rm -f -- '/etc/prep key'\n"), "{text}");
    assert!(text.contains("printf '%s' 'k' > '/etc/prep key'\nchmod 0600 -- '/etc/prep key'\n"));
}
