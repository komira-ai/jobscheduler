//! `dep define-profile` and `dep assign` on the fake host and a fake API that records its calls.

mod common;

use common::{
    EXAMPLE, FakeApi, FakeNet, NoApi, UUID, answer, changes, config, kit, ok, ready_host, with_api,
};
use js_host_step::fake::FakeHost;
use js_host_step::{Host, SYSTEMCTL};
use js_mdm::api::Method;
use js_mdm::run;
use js_mdm_core::profile::AdeProfile;
use sha2::{Digest, Sha256};

const KEY: &str = "s3cr3t-dep-key";
/// base64 of `depserver:s3cr3t-dep-key`.
const AUTH: &str = "Basic ZGVwc2VydmVyOnMzY3IzdC1kZXAta2V5";
const STATE: &str = "/var/lib/js-mdm";
const UUID_FILE: &str = "/var/lib/js-mdm/dep-profile-uuid";
const SUM_FILE: &str = "/var/lib/js-mdm/dep-profile.sha256";
const ENROLL: &str = "/var/lib/js-mdm/www/enroll.mobileconfig";
const SERIAL: &str = "C02ABCDEF123";

/// A root host with the settings file, the depserver key, and a depsyncer that is neither
/// enabled nor running.
fn host() -> FakeHost {
    let host = FakeHost::new(0);
    host.add_user("js-mdm", 900);
    host.put_file("/cfg", 0o644, (0, 0), EXAMPLE.as_bytes());
    host.put_file(
        "/etc/js-mdm/nanodep.env",
        0o600,
        (0, 0),
        format!("NANODEP_API={KEY}\n").as_bytes(),
    );
    host.reply_prefix(&[SYSTEMCTL, "is-enabled"], 1, "disabled\n");
    host.reply_prefix(&[SYSTEMCTL, "is-active"], 3, "inactive\n");
    host.reply_prefix(&[SYSTEMCTL, "enable"], 0, "");
    host.reply_prefix(&[SYSTEMCTL, "start"], 0, "");
    host
}

/// The depsyncer is enabled and running.
fn depsyncer_up(host: &FakeHost) {
    host.reply_prefix(&[SYSTEMCTL, "is-enabled"], 0, "enabled\n");
    host.reply_prefix(&[SYSTEMCTL, "is-active"], 0, "active\n");
}

fn cli(host: &FakeHost, api: &FakeApi, words: &[&str]) -> (u8, String, String) {
    let args: Vec<String> = std::iter::once("js-mdm")
        .chain(words.iter().copied())
        .map(str::to_owned)
        .collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let net = FakeNet::new(Default::default());
    let code = with_api(host, &net, api, |env| run(&args, env, &mut out, &mut err));
    (
        code,
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

fn define(host: &FakeHost, api: &FakeApi, extra: &[&str]) -> (u8, String, String) {
    let mut words = vec!["dep", "define-profile", "--config", "/cfg"];
    words.extend_from_slice(extra);
    cli(host, api, &words)
}

fn assign(host: &FakeHost, api: &FakeApi, extra: &[&str]) -> (u8, String, String) {
    let mut words = vec!["dep", "assign", "--config", "/cfg"];
    words.extend_from_slice(extra);
    cli(host, api, &words)
}

fn rendered() -> Vec<u8> {
    AdeProfile::from_config(&config()).to_json()
}

fn rendered_sum() -> String {
    format!("{}\n", hex::encode(Sha256::digest(rendered())))
}

/// A host on which a profile is already defined and recorded.
fn defined(uuid: &str, sum: &str) -> FakeHost {
    let host = host();
    host.put_file(UUID_FILE, 0o644, (0, 0), format!("{uuid}\n").as_bytes());
    host.put_file(SUM_FILE, 0o644, (0, 0), sum.as_bytes());
    host
}

/// Catches a dry run that calls Apple, writes a record or enables the depsyncer, and one that
/// hides what it would do: the plan names the two calls (with the size of the body and never
/// the body), the profile's JSON, both record files and the unit steps, and only read-only
/// commands run.
#[test]
fn define_profile_dry_run_makes_no_call_and_changes_nothing() {
    let (host, api) = (host(), FakeApi::apple());
    let before = host.paths();
    let (code, out, err) = define(&host, &api, &[]);
    assert_eq!((code, err.as_str()), (0, ""), "{out}");
    assert!(api.sent.borrow().is_empty());
    assert_eq!(host.paths(), before);
    assert!(changes(&host).is_empty(), "{:?}", changes(&host));
    let size = rendered().len();
    let post = format!(
        "DRY-RUN: POST http://127.0.0.1:9001/proxy/example/profile (NanoDEP API, {size} bytes)\n"
    );
    assert!(out.starts_with(&post), "{out}");
    for line in [
        "DRY-RUN: PUT http://127.0.0.1:9001/v1/assigner/example?profile_uuid=<the profile_uuid Apple answers> (NanoDEP API)",
        "DRY-RUN: write /var/lib/js-mdm/dep-profile-uuid (that uuid)",
        "    |   \"url\": \"https://mdm.example.com/enroll\",",
        "DRY-RUN: write /var/lib/js-mdm/dep-profile.sha256 mode 0644 owner root:root (65 bytes)",
        "DRY-RUN: systemctl enable -- js-mdm-depsyncer.service",
        "DRY-RUN: systemctl start -- js-mdm-depsyncer.service",
        "dry run: nothing changed; re-run with --apply to carry it out",
    ] {
        assert!(out.contains(line), "{line}\n{out}");
    }
}

/// Catches a body that drifts from the render, a wrong uuid in the assigner call, a call in
/// the wrong order, a record with the wrong bytes or mode, and a depsyncer left off: Apple is
/// sent exactly the render, the assigner gets the uuid it answered, both records hold what
/// they should (0644, root), depsyncer is enabled and started, and the key goes only in the
/// `Authorization` header of each call, to depserver on loopback.
#[test]
fn define_profile_apply_calls_in_order_and_records() {
    let (host, api) = (host(), FakeApi::apple());
    let (code, out, err) = define(&host, &api, &["--apply"]);
    assert_eq!((code, err.as_str()), (0, ""), "{out}");
    let sent = api.sent.borrow();
    assert_eq!(sent.len(), 2);
    let post = &sent[0];
    assert_eq!(
        (post.service.as_str(), post.addr.as_str()),
        ("NanoDEP", "127.0.0.1:9001")
    );
    assert_eq!(post.call.method, Method::Post);
    assert_eq!(post.call.path, "/proxy/example/profile");
    assert!(post.call.json);
    assert_eq!(post.call.body, rendered());
    let put = &sent[1];
    assert_eq!(put.call.method, Method::Put);
    assert_eq!(
        put.call.path,
        format!("/v1/assigner/example?profile_uuid={UUID}")
    );
    assert!(put.call.body.is_empty() && !put.call.json);
    assert_eq!(*api.authorizations.borrow(), vec![AUTH.to_owned(); 2]);
    assert!(
        out.contains(&format!("ADE profile: defined, uuid {UUID};")),
        "{out}"
    );

    let uuid = host.node(UUID_FILE).unwrap();
    assert_eq!(
        (uuid.bytes, uuid.mode, uuid.uid, uuid.gid),
        (format!("{UUID}\n").into_bytes(), 0o644, 0, 0)
    );
    let sum = host.node(SUM_FILE).unwrap();
    assert_eq!(
        (sum.bytes, sum.mode, sum.uid, sum.gid),
        (rendered_sum().into_bytes(), 0o644, 0, 0)
    );
    let steps = changes(&host);
    assert_eq!(
        steps,
        [
            "/usr/bin/systemctl enable -- js-mdm-depsyncer.service",
            "/usr/bin/systemctl start -- js-mdm-depsyncer.service",
        ]
    );
}

/// Catches a rerun that defines the profile again, and a record kbf-deploy wrote not being
/// adopted: with the recorded sha256 equal to the render's and a uuid recorded, no call is
/// made, the uuid is reported, and the run finds nothing to do (depsyncer already up). The
/// record here is written by hand in kbf-deploy's format (`<hex>\n`, `<uuid>\n`).
#[test]
fn a_recorded_profile_is_not_defined_again() {
    let host = defined("KBFUUID0123456789abcdef", &rendered_sum());
    depsyncer_up(&host);
    let api = FakeApi::apple();
    for extra in [&["--apply"][..], &[]] {
        let (code, out, err) = define(&host, &api, extra);
        assert_eq!((code, err.as_str()), (0, ""), "{out}");
        assert!(
            out.starts_with(
                "ADE profile: unchanged (uuid KBFUUID0123456789abcdef)\nnothing to do\n"
            ),
            "{out}"
        );
    }
    assert!(api.sent.borrow().is_empty());
    assert!(changes(&host).is_empty());

    // `--replace` does not define it again either: the render is the record.
    let (code, _, _) = define(&host, &api, &["--apply", "--replace"]);
    assert_eq!(code, 0);
    assert!(api.sent.borrow().is_empty());
}

/// Catches an unchanged profile that skips the depsyncer: a rerun makes no call, and still
/// brings the depsyncer up when it is not.
#[test]
fn an_unchanged_profile_still_enables_the_depsyncer() {
    let host = defined(UUID, &rendered_sum());
    let api = FakeApi::apple();
    let (code, out, _) = define(&host, &api, &["--apply"]);
    assert_eq!(code, 0, "{out}");
    assert!(api.sent.borrow().is_empty());
    assert_eq!(
        changes(&host),
        [
            "/usr/bin/systemctl enable -- js-mdm-depsyncer.service",
            "/usr/bin/systemctl start -- js-mdm-depsyncer.service",
        ]
    );
}

/// Catches a second profile defined by accident: a recorded profile whose sha256 is not the
/// render's (the settings changed, or another tool defined it with other `MDM_ADE_*` values),
/// or whose sha256 is missing, is refused with exit 2 as a dry run and with `--apply`, before
/// any call, and the record stays as it was. `--replace` defines the second profile, moves
/// the assigner to it and records it.
#[test]
fn a_recorded_profile_that_is_not_the_render_is_refused_unless_replace() {
    for sum in [Some("deadbeef\n"), None] {
        let host = defined("OLDUUID0123456789abcdef", sum.unwrap_or_default());
        if sum.is_none() {
            host.remove(SUM_FILE).unwrap();
        }
        let api = FakeApi::apple();
        for extra in [&["--apply"][..], &[]] {
            let (code, out, err) = define(&host, &api, extra);
            assert_eq!((code, out.as_str()), (2, ""), "{err}");
            assert!(
                err.starts_with("REFUSED: an ADE profile is already defined (uuid OLDUUID0123456789abcdef) from a render with sha256 "),
                "{err}"
            );
            assert!(err.contains(&rendered_sum().trim().to_owned()), "{err}");
            assert!(err.contains("--replace"), "{err}");
        }
        assert!(api.sent.borrow().is_empty());
        assert_eq!(
            host.node(UUID_FILE).unwrap().bytes,
            b"OLDUUID0123456789abcdef\n"
        );

        let (code, out, err) = define(&host, &api, &["--apply", "--replace"]);
        assert_eq!((code, err.as_str()), (0, ""), "{out}");
        assert_eq!(api.sent.borrow().len(), 2);
        assert_eq!(
            host.node(UUID_FILE).unwrap().bytes,
            format!("{UUID}\n").into_bytes()
        );
        assert_eq!(
            host.node(SUM_FILE).unwrap().bytes,
            rendered_sum().into_bytes()
        );
    }
}

/// Catches a changed `MDM_ADE_*` setting not changing the render (it would never be defined),
/// and a profile that no longer hashes as kbf-deploy's when the settings are kbf's: with
/// kbf's values (the name `<org> build farm`, `kbf-mdm`, `en`, `US`) the render is kbf's JSON
/// byte for byte, so its sha256 equals the one kbf-deploy records; the example's other
/// values give another.
#[test]
fn the_render_is_kbfs_only_with_kbfs_settings() {
    let kbf_text = EXAMPLE
        .lines()
        .map(|l| match l.split_once('=') {
            Some(("MDM_ADE_PROFILE_NAME", _)) => "MDM_ADE_PROFILE_NAME=\"Example build farm\"",
            Some(("MDM_ADE_ORG_MAGIC", _)) => "MDM_ADE_ORG_MAGIC=kbf-mdm",
            _ => l,
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    let c = js_mdm_core::config::MdmConfig::parse(&kbf_text).unwrap();
    let items: Vec<String> = c
        .ade
        .skip_items
        .iter()
        .map(|i| format!("    \"{i}\""))
        .collect();
    let kbf = format!(
        "{{\n  \"profile_name\": \"Example build farm\",\n  \"url\": \"https://mdm.example.com/enroll\",\n  \"is_supervised\": true,\n  \"is_mandatory\": true,\n  \"is_mdm_removable\": false,\n  \"await_device_configured\": false,\n  \"auto_advance_setup\": false,\n  \"org_magic\": \"kbf-mdm\",\n  \"language\": \"en\",\n  \"region\": \"US\",\n  \"anchor_certs\": [],\n  \"skip_setup_items\": [\n{}\n  ]\n}}\n",
        items.join(",\n")
    );
    assert_eq!(AdeProfile::from_config(&c).to_json(), kbf.as_bytes());
    assert_ne!(rendered(), kbf.as_bytes());
    assert_eq!(c.ade.skip_items.len(), 18);
}

/// Catches a record written for a define that failed, and an answer taken on trust: an answer
/// with no `profile_uuid` of 16 to 64 hex digits (not JSON, no key, too short, too long, not
/// hex of a good length) fails the run (exit 1) before the assigner is called, and no record
/// is written; the assigner is called after the define only.
#[test]
fn an_answer_without_a_usable_uuid_records_nothing() {
    let uuid_of = |uuid: String| format!("{{\"profile_uuid\":\"{uuid}\"}}");
    let bad_answers = [
        "not json".to_owned(),
        "{}".to_owned(),
        uuid_of("abc123".to_owned()),
        uuid_of("a".repeat(65)),
        uuid_of("g".repeat(32)),
    ];
    for bad in &bad_answers {
        let host = host();
        let body = bad.to_owned();
        let api = FakeApi::new(move |_| ok(&body));
        let before = host.paths();
        let (code, out, err) = define(&host, &api, &["--apply"]);
        assert_eq!(code, 1, "{bad}: {out}");
        assert!(
            err.starts_with(
                "FAILED: the profile definition: no profile_uuid of 16 to 64 hex digits in "
            ),
            "{err}"
        );
        assert_eq!(api.paths(), ["POST /proxy/example/profile"]);
        assert_eq!(host.paths(), before);
        assert!(changes(&host).is_empty());
    }
}

/// Catches an Apple refusal reported as done, and an error that quotes a whole answer: a
/// non-2xx answer to the define, a non-2xx answer to the assigner call (after a good define:
/// nothing is recorded) and a transport failure each exit 1 with the call named; the answer
/// is quoted to 300 characters at most.
#[test]
fn failed_calls_exit_1_and_record_nothing() {
    let long = "x".repeat(400);
    let host_a = host();
    let body = long.clone();
    let api = FakeApi::new(move |_| answer(500, &body));
    let (code, _, err) = define(&host_a, &api, &["--apply"]);
    assert_eq!(code, 1);
    let want = format!(
        "FAILED: POST http://127.0.0.1:9001/proxy/example/profile (NanoDEP API, {} bytes): answered 500: {}\n",
        rendered().len(),
        "x".repeat(300)
    );
    assert_eq!(err, want);

    let host_b = host();
    let api = FakeApi::new(|call| match call.method {
        Method::Post => ok(&format!("{{\"profile_uuid\":\"{UUID}\"}}")),
        Method::Put => answer(404, "no such dep name"),
    });
    let before = host_b.paths();
    let (code, _, err) = define(&host_b, &api, &["--apply"]);
    assert_eq!(code, 1);
    assert!(err.contains("answered 404: no such dep name"), "{err}");
    assert_eq!(host_b.paths(), before);

    let host_c = host();
    let api = FakeApi::new(|_| Err("connection refused".to_owned()));
    let (code, _, err) = define(&host_c, &api, &["--apply"]);
    assert_eq!(code, 1);
    assert!(err.ends_with("bytes): connection refused\n"), "{err}");
}

/// Catches a verb that runs as someone else or reads a key it should not: without root
/// (even as a dry run), with a key file that is not 0600 and root's, or with no
/// `NANODEP_API=` value, both verbs are refused (exit 2) with no call and nothing written; the
/// message names the file and key and never the value.
#[test]
fn both_verbs_are_refused_without_root_or_a_proper_key_file() {
    let verbs: [&[&str]; 2] = [&["--apply"], &["--serial", SERIAL, "--apply"]];
    // Not root.
    let host = FakeHost::new(1000);
    host.put_file("/cfg", 0o644, (0, 0), EXAMPLE.as_bytes());
    let api = FakeApi::apple();
    let (code, out, err) = define(&host, &api, &[]);
    assert_eq!((code, out.as_str()), (2, ""));
    assert_eq!(
        err,
        "REFUSED: dep define-profile reads root-only files: run it as root, even for a dry run (effective uid is 1000)\n"
    );
    let (code, _, err) = assign(&host, &api, &["--serial", SERIAL]);
    assert_eq!(code, 2);
    assert!(
        err.starts_with("REFUSED: dep assign reads root-only"),
        "{err}"
    );
    assert!(api.sent.borrow().is_empty());

    // A key file that is too open, owned by another user, empty-valued or without the key.
    for (mode, uid, text) in [
        (0o644, 0, format!("NANODEP_API={KEY}\n")),
        (0o600, 1000, format!("NANODEP_API={KEY}\n")),
        (0o600, 0, "NANODEP_API=\n".to_owned()),
        (0o600, 0, format!("OTHER={KEY}\n")),
    ] {
        let host = host_with_key_file(mode, uid, &text);
        for (i, extra) in verbs.iter().enumerate() {
            let (code, out, err) = match i {
                0 => define(&host, &api, extra),
                _ => assign(&host, &api, extra),
            };
            assert_eq!((code, out.as_str()), (2, ""), "{err}");
            assert!(
                err.starts_with("REFUSED: /etc/js-mdm/nanodep.env "),
                "{err}"
            );
            assert!(!err.contains(KEY), "{err}");
        }
    }
    assert!(api.sent.borrow().is_empty());
}

fn host_with_key_file(mode: u32, uid: u32, text: &str) -> FakeHost {
    let host = host();
    host.put_file("/etc/js-mdm/nanodep.env", mode, (uid, 0), text.as_bytes());
    host.put_file(UUID_FILE, 0o644, (0, 0), format!("{UUID}\n").as_bytes());
    host
}

/// Catches an API call made from a binary others could replace (it would carry the key to
/// whoever replaced it): with the running binary owned by another user or writable by its
/// group or others, `--apply` is refused (exit 2) before any call or write, for both verbs;
/// a dry run, which calls nothing, still works; a root-owned 0755 binary is the control.
#[test]
fn no_call_is_made_from_an_untrusted_binary() {
    for (uid, mode) in [(1000, 0o755), (0, 0o775), (0, 0o757)] {
        let (fresh, known) = (
            host(),
            host_with_key_file(0o600, 0, &format!("NANODEP_API={KEY}\n")),
        );
        let api = FakeApi::apple();
        let before = (fresh.paths(), known.paths());
        for h in [&fresh, &known] {
            h.set_binary(uid, mode);
        }
        let (code, _, err) = define(&fresh, &api, &["--apply"]);
        assert_eq!(code, 2, "{uid} {mode:o}");
        assert!(err.starts_with("REFUSED: the running binary is "), "{err}");
        let (code, _, err) = assign(&known, &api, &["--serial", SERIAL, "--apply"]);
        assert_eq!(code, 2, "{err}");
        assert!(err.starts_with("REFUSED: the running binary is "), "{err}");
        assert!(err.contains("owns alone"), "{err}");
        assert!(api.sent.borrow().is_empty());
        assert_eq!((fresh.paths(), known.paths()), before);
        assert!(changes(&fresh).is_empty() && changes(&known).is_empty());
        // A dry run calls nothing, so it has nothing to refuse.
        assert_eq!(define(&fresh, &api, &[]).0, 0);
        assert_eq!(assign(&known, &api, &["--serial", SERIAL]).0, 0);
        assert!(api.sent.borrow().is_empty());
    }
    let (fresh, known) = (
        host(),
        host_with_key_file(0o600, 0, &format!("NANODEP_API={KEY}\n")),
    );
    let api = FakeApi::apple();
    for h in [&fresh, &known] {
        h.set_binary(0, 0o755);
    }
    assert_eq!(define(&fresh, &api, &["--apply"]).0, 0);
    assert_eq!(assign(&known, &api, &["--serial", SERIAL, "--apply"]).0, 0);
    assert_eq!(api.sent.borrow().len(), 3);
}

/// Catches the key leaking: through a full define, an assign, and every failure path, the
/// depserver key (and its base64 in the header) is in no output, no error, no `Debug` of a
/// call or a service, no command line the host ran and no file written; it reaches the API
/// only as the `Authorization` header value.
#[test]
fn the_key_is_in_no_output_debug_argv_or_file() {
    let host = host();
    let api = FakeApi::new(|call| match call.path.as_str() {
        "/proxy/example/profile" => ok(&format!("{{\"profile_uuid\":\"{UUID}\"}}")),
        p if p.ends_with("/devices") => answer(500, "no"),
        _ => ok("{}"),
    });
    let mut seen = String::new();
    for (code, out, err) in [
        define(&host, &api, &[]),
        define(&host, &api, &["--apply"]),
        assign(&host, &api, &["--serial", SERIAL]),
        assign(&host, &api, &["--serial", SERIAL, "--apply"]),
    ] {
        seen.push_str(&format!("{code}\n{out}\n{err}\n"));
    }
    seen.push_str(&format!("{:?}\n", api.sent.borrow()));
    seen.push_str(&format!("{:?}\n", host.calls()));
    for sent in api.sent.borrow().iter() {
        seen.push_str(&format!("{:?}\n", sent.call));
    }
    for path in host.paths() {
        if path != "/etc/js-mdm/nanodep.env" {
            seen.push_str(&String::from_utf8_lossy(&host.node(&path).unwrap().bytes));
        }
    }
    let b64 = AUTH.trim_start_matches("Basic ");
    assert!(!seen.contains(KEY) && !seen.contains(b64), "{seen}");
    assert_eq!(api.sent.borrow().len(), 3);
    assert_eq!(
        api.authorizations.borrow().first().map(String::as_str),
        Some(AUTH)
    );
}

/// Catches `www/enroll.mobileconfig` being rewritten by the verbs (its payload UUIDs differ
/// between kbf-deploy's and js-mdm's render, and a Mac that took one would hold two
/// profiles): after `define-profile --apply`, `--replace` and `assign --apply` the file has
/// the bytes, mode and owner kbf-deploy gave it and the output never names it.
#[test]
fn the_dep_verbs_never_touch_the_enrollment_profile() {
    let host = host();
    host.put_dir(&format!("{STATE}/www"), 0o700, (900, 900));
    host.put_file(ENROLL, 0o600, (900, 900), b"KBF-WRITTEN PROFILE");
    let before = host.node(ENROLL);
    let api = FakeApi::apple();
    let mut shown = String::new();
    for (code, out, err) in [
        define(&host, &api, &["--apply"]),
        define(&host, &api, &["--apply", "--replace"]),
        assign(&host, &api, &["--serial", SERIAL, "--apply"]),
    ] {
        assert_eq!(code, 0, "{err}");
        shown.push_str(&out);
    }
    assert_eq!(host.node(ENROLL), before);
    assert!(!shown.contains("enroll.mobileconfig") && !shown.contains("mobileconfig"));
}

/// Catches `install` rewriting an existing `www/enroll.mobileconfig`: install (dry run and
/// `--apply`, twice) leaves the file, its mode and its owner as they were and plans no step
/// for it. (Today install never writes the file; this pins that, so a later change that
/// does must keep an existing one.)
#[test]
fn install_keeps_an_existing_enrollment_profile() {
    let kit = kit();
    let host = ready_host(&kit);
    host.put_file("/cfg", 0o644, (0, 0), EXAMPLE.as_bytes());
    host.put_dir(&format!("{STATE}/www"), 0o700, (900, 900));
    host.put_file(ENROLL, 0o600, (900, 900), b"KBF-WRITTEN PROFILE");
    let before = host.node(ENROLL);
    let net = FakeNet::new(Default::default());
    for apply in [false, true, true] {
        let mut words = vec!["install", "--config", "/cfg", "--bin-dir", "/srv/fetch"];
        if apply {
            words.push("--apply");
        }
        let args: Vec<String> = std::iter::once("js-mdm")
            .chain(words)
            .map(str::to_owned)
            .collect();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = common::with_api(&host, &net, &NoApi, |env| {
            js_mdm::run_with(&args, env, &kit.pins, &mut out, &mut err)
        });
        let shown = String::from_utf8(out).unwrap();
        assert_eq!(code, 0, "{}", String::from_utf8_lossy(&err));
        assert!(!shown.contains("mobileconfig"), "{shown}");
        assert_eq!(host.node(ENROLL), before);
    }
}

/// Catches an assign that goes out without a defined profile, or with a body that differs from
/// the serial list: with no `dep-profile-uuid` it is refused (exit 2, naming the verb to run
/// first) as a dry run and with `--apply`, with no call; with a profile the dry run prints the
/// call (body size, not the body) and calls nothing, and `--apply` posts exactly
/// `{"devices":[...],"profile_uuid":"..."}` in the serial order given, to depserver.
#[test]
fn assign_needs_a_defined_profile_and_posts_the_serials() {
    let (host, api) = (host(), FakeApi::apple());
    for extra in [&["--serial", SERIAL][..], &["--serial", SERIAL, "--apply"]] {
        let (code, out, err) = assign(&host, &api, extra);
        assert_eq!((code, out.as_str()), (2, ""));
        assert_eq!(
            err,
            "REFUSED: no ADE profile is recorded (/var/lib/js-mdm/dep-profile-uuid is not there): run js-mdm dep define-profile --apply first\n"
        );
    }
    assert!(api.sent.borrow().is_empty());

    let host = defined(UUID, &rendered_sum());
    let api = FakeApi::new(|_| {
        ok("{\"devices\":{\"C02ABCDEF123\":\"SUCCESS\",\"F4KABCDEFGH2\":\"SUCCESS\"}}")
    });
    let two = ["--serial", SERIAL, "--serial", "F4KABCDEFGH2"];
    let (code, out, err) = assign(&host, &api, &two);
    assert_eq!((code, err.as_str()), (0, ""));
    let body_len =
        format!("{{\"devices\":[\"{SERIAL}\",\"F4KABCDEFGH2\"],\"profile_uuid\":\"{UUID}\"}}")
            .len();
    assert_eq!(
        out,
        format!(
            "DRY-RUN: POST http://127.0.0.1:9001/proxy/example/profile/devices (NanoDEP API, {body_len} bytes)\ndry run: nothing changed; re-run with --apply to carry it out\n"
        )
    );
    assert!(api.sent.borrow().is_empty());

    let mut apply = two.to_vec();
    apply.push("--apply");
    let (code, out, err) = assign(&host, &api, &apply);
    assert_eq!((code, err.as_str()), (0, ""));
    assert!(
        out.ends_with("C02ABCDEF123: SUCCESS\nF4KABCDEFGH2: SUCCESS\n"),
        "{out}"
    );
    let sent = api.sent.borrow();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].call.method, Method::Post);
    assert!(sent[0].call.json);
    assert_eq!(
        String::from_utf8_lossy(&sent[0].call.body),
        format!("{{\"devices\":[\"{SERIAL}\",\"F4KABCDEFGH2\"],\"profile_uuid\":\"{UUID}\"}}")
    );
    assert_eq!(*api.authorizations.borrow(), [AUTH]);
}

/// Catches an assign that reports success for a device Apple did not accept: one answered
/// with another status, and one missing from the answer (or an answer that is not JSON), make
/// the exit 1 with the serial and the status on stdout; every device is listed.
#[test]
fn assign_exits_1_unless_every_device_succeeds() {
    let host = defined(UUID, &rendered_sum());
    for (body, lines) in [
        (
            "{\"devices\":{\"C02ABCDEF123\":\"NOT_ACCESSIBLE\",\"F4KABCDEFGH2\":\"SUCCESS\"}}",
            "C02ABCDEF123: NOT_ACCESSIBLE\nF4KABCDEFGH2: SUCCESS\n",
        ),
        (
            "{\"devices\":{\"C02ABCDEF123\":\"SUCCESS\"}}",
            "C02ABCDEF123: SUCCESS\nF4KABCDEFGH2: no answer\n",
        ),
        ("oops", "C02ABCDEF123: no answer\nF4KABCDEFGH2: no answer\n"),
    ] {
        let text = body.to_owned();
        let api = FakeApi::new(move |_| ok(&text));
        let argv = ["--serial", SERIAL, "--serial", "F4KABCDEFGH2", "--apply"];
        let (code, out, err) = assign(&host, &api, &argv);
        assert_eq!((code, err.as_str()), (1, ""), "{out}");
        assert!(out.ends_with(lines), "{out}");
    }
}

/// Catches a serial that is not one being sent to Apple: lower case, 7 or 15 characters,
/// punctuation, a space, an empty value and a missing `--serial` are all refused at the
/// command line (exit 2) with no call; 8 and 14 characters pass.
#[test]
fn serial_numbers_are_checked() {
    let host = defined(UUID, &rendered_sum());
    let api = FakeApi::new(|_| ok("{}"));
    for bad in [
        "c02abcdef123",
        "ABCDEFG",
        "ABCDEFGHIJKLMNO",
        "C02-ABCDEF1",
        "C02 ABCDEF1",
        "",
    ] {
        let (code, out, err) = assign(&host, &api, &["--serial", bad, "--apply"]);
        assert_eq!((code, out.as_str()), (2, ""), "{bad:?}");
        assert!(
            err.contains("is not a serial number (8 to 14 upper-case letters and digits)"),
            "{err}"
        );
    }
    let (code, _, err) = assign(&host, &api, &["--apply"]);
    assert_eq!(code, 2);
    assert!(err.contains("--serial"), "{err}");
    assert!(api.sent.borrow().is_empty());
    for good in ["ABCDEFGH", "ABCDEFGHIJKLMN", "0123456789"] {
        let (code, _, _) = assign(&host, &api, &["--serial", good, "--apply"]);
        assert_eq!(
            code, 1,
            "{good}: the answer is {{}}, so the device has no status"
        );
    }
    assert_eq!(api.sent.borrow().len(), 3);
    assert_eq!(
        js_mdm::dep::Serial::new("ABCDEFGH").unwrap().as_str(),
        "ABCDEFGH"
    );
}

/// Catches the key being sent to whoever holds depserver's port: a listener on `MDM_DEP_PORT`
/// owned by a user who is neither root nor the service user (alone or beside depserver's own)
/// refuses `--apply` for both verbs (exit 2) before any call or write; depserver's own
/// (the service user's, or root's) and no listener go on; a dry run calls nothing and so
/// refuses nothing.
#[test]
fn the_key_is_not_sent_to_a_squatter_on_the_dep_port() {
    for (listeners, refused) in [
        (vec![], false),
        (vec![900], false),
        (vec![0], false),
        (vec![1000], true),
        (vec![900, 1001], true),
    ] {
        let (fresh, known) = (
            host(),
            host_with_key_file(0o600, 0, &format!("NANODEP_API={KEY}\n")),
        );
        for h in [&fresh, &known] {
            h.add_listener(9004, 1234);
            for uid in &listeners {
                h.add_listener(9001, *uid);
            }
        }
        let api = FakeApi::apple();
        let (before_a, before_b) = (fresh.paths(), known.paths());
        let (code, out, err) = define(&fresh, &api, &["--apply"]);
        let (code2, _, err2) = assign(&known, &api, &["--serial", SERIAL, "--apply"]);
        match refused {
            true => {
                assert_eq!((code, code2), (2, 2), "{out}");
                for e in [&err, &err2] {
                    assert!(
                        e.starts_with("REFUSED: port 9001 on loopback is held by uid "),
                        "{e}"
                    );
                    assert!(!e.contains(KEY));
                }
                assert!(api.sent.borrow().is_empty());
                assert_eq!((fresh.paths(), known.paths()), (before_a, before_b));
                assert_eq!(define(&fresh, &api, &[]).0, 0);
                assert_eq!(assign(&known, &api, &["--serial", SERIAL]).0, 0);
                assert!(api.sent.borrow().is_empty());
            }
            false => {
                assert_eq!((code, code2), (0, 0), "{err} {err2}");
                assert_eq!(api.sent.borrow().len(), 3);
            }
        }
    }
}

/// Catches the key leaking through a server that reflects the request it got: an answer that
/// repeats the `Authorization` header and the key, as an error status (define and assign) and
/// as a 200 without a profile uuid, appears in no error text or output; the status is still
/// reported.
#[test]
fn a_server_that_reflects_the_request_leaks_no_key() {
    let reflect = |status| {
        move |_: &js_mdm::api::Call| {
            answer(
                status,
                &format!(
                    "seen\nAuthorization: {AUTH}\nand {KEY} and {}",
                    AUTH.trim_start_matches("Basic ")
                ),
            )
        }
    };
    for status in [403, 200] {
        let host = host();
        let api = FakeApi::new(reflect(status));
        let (code, out, err) = define(&host, &api, &["--apply"]);
        assert_eq!(code, 1);
        let all = format!("{out}{err}");
        assert!(
            !all.contains(KEY) && !all.contains("Basic") && !all.contains(&AUTH[6..]),
            "{all}"
        );
        assert!(all.contains("seen"), "{all}");
    }
    let host = defined(UUID, &rendered_sum());
    let api = FakeApi::new(reflect(500));
    let (code, out, err) = assign(&host, &api, &["--serial", SERIAL, "--apply"]);
    assert_eq!(code, 1);
    let all = format!("{out}{err}");
    assert!(!all.contains(KEY) && !all.contains("Basic"), "{all}");
}

/// Catches an assign sent with a recorded profile that is not a uuid (a damaged or hand-edited
/// file would go to Apple as the profile): refused (exit 2) naming the file, before any call,
/// dry run or not.
#[test]
fn assign_refuses_a_recorded_profile_that_is_not_a_uuid() {
    let long = "f".repeat(65);
    for bad in ["not-a-uuid", "", "abc123", long.as_str()] {
        let host = host();
        host.put_file(UUID_FILE, 0o644, (0, 0), format!("{bad}\n").as_bytes());
        let api = FakeApi::apple();
        for extra in [&["--serial", SERIAL][..], &["--serial", SERIAL, "--apply"]] {
            let (code, out, err) = assign(&host, &api, extra);
            assert_eq!((code, out.as_str()), (2, ""), "{bad:?}");
            assert!(
                err.starts_with("REFUSED: /var/lib/js-mdm/dep-profile-uuid does not hold a profile uuid of 16 to 64 hex digits"),
                "{err}"
            );
        }
        assert!(api.sent.borrow().is_empty());
    }
}

/// Catches Apple's per-device status reaching the terminal raw: a status with an escape
/// sequence and a line break is printed written out, and the device is still not a success.
#[test]
fn a_device_status_is_printed_without_control_characters() {
    let host = defined(UUID, &rendered_sum());
    let api = FakeApi::new(|_| ok("{\"devices\":{\"C02ABCDEF123\":\"OK\\u001b[2J\\nSUCCESS\"}}"));
    let (code, out, _) = assign(&host, &api, &["--serial", SERIAL, "--apply"]);
    assert_eq!(code, 1);
    assert!(
        out.ends_with("C02ABCDEF123: OK\\u{1b}[2J\\nSUCCESS\n"),
        "{out:?}"
    );
    assert!(!out.contains('\x1b'));
}

/// Catches a key read differently from the way systemd gives it to depserver: surrounding double
/// or single quotes, trailing blanks and a DOS line end are not part of the key (the call
/// carries the same Authorization either way); an empty quoted value is no key.
#[test]
fn the_key_is_read_as_systemd_reads_it() {
    for text in [
        format!("NANODEP_API=\"{KEY}\"\n"),
        format!("NANODEP_API='{KEY}'  \n"),
        format!("NANODEP_API={KEY}\t \r\n"),
        format!("NANODEP_API={KEY}\n"),
    ] {
        let host = host_with_key_file(0o600, 0, &text);
        let api = FakeApi::apple();
        let (code, _, err) = assign(&host, &api, &["--serial", SERIAL, "--apply"]);
        assert_eq!((code, err.as_str()), (0, ""), "{text:?}");
        assert_eq!(*api.authorizations.borrow(), [AUTH], "{text:?}");
    }
    for text in [
        "NANODEP_API=\"\"\n",
        "NANODEP_API='\n",
        "NANODEP_API=\"abc\n",
    ] {
        let host = host_with_key_file(0o600, 0, text);
        let api = FakeApi::apple();
        let (code, _, err) = assign(&host, &api, &["--serial", SERIAL, "--apply"]);
        if text == "NANODEP_API=\"\"\n" {
            assert_eq!(code, 2, "{err}");
            assert!(
                err.contains("has no NANODEP_API= line with a value"),
                "{err}"
            );
        } else {
            // A lone or unbalanced quote is kept as part of the value.
            assert_eq!(code, 0, "{err}");
        }
    }
}

/// Catches `--replace` documented as defining something when the render is already recorded:
/// the help of define-profile says it changes nothing then.
#[test]
fn the_replace_flag_is_documented_as_a_no_op_for_a_recorded_render() {
    let (code, out, _) = cli(
        &host(),
        &FakeApi::apple(),
        &["dep", "define-profile", "--help"],
    );
    assert_eq!(code, 0);
    let flat = out.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        flat.contains("With a recorded profile that is this render it changes nothing."),
        "{out}"
    );
}
