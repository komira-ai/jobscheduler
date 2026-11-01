//! `verify`: health checks and the comparison with the core's render, on the fake host.

mod common;

use common::{FakeNet, SVC, acme, config, kit, ready_host, with_env};
use js_host_step::fake::FakeHost;
use js_host_step::{Host, SYSTEMCTL};
use js_mdm::verify::{Compared, Verify, compare, run};
use js_mdm_core::render::{render, render_tls, strip_comments};
use sha2::{Digest, Sha256};

const UNITS: &str = "/etc/systemd/system";
const ETC: &str = "/etc/js-mdm";
const STATE: &str = "/var/lib/js-mdm";
const CERT: &str = "/var/lib/js-mdm/lego/certificates/mdm.example.com.crt";
const ACTIVE: [&str; 6] = [
    "js-mdm-scep.service",
    "js-mdm-nanohub.service",
    "js-mdm-depserver.service",
    "js-mdm-proxy.service",
    "js-mdm-cert.timer",
    "js-mdm-proxy-reload.path",
];

fn sha(text: &str) -> String {
    hex::encode(Sha256::digest(strip_comments(text).as_bytes()))
}

/// A host exactly as `install` and `tls` leave it, everything running.
fn installed() -> FakeHost {
    let host = ready_host(&kit());
    let rendered = render(&config()).unwrap();
    for unit in rendered
        .units
        .iter()
        .chain(render_tls(&config(), &acme()).unwrap().iter())
    {
        host.put_file(
            &format!("{UNITS}/{}", unit.name),
            0o644,
            (0, 0),
            unit.text.as_bytes(),
        );
    }
    host.put_file(
        &format!("{ETC}/Caddyfile"),
        0o640,
        (0, SVC),
        rendered.caddyfile.as_bytes(),
    );
    for unit in ACTIVE {
        host.reply(&[SYSTEMCTL, "is-active", unit], 0, "active\n");
    }
    host.reply(
        &["/usr/bin/ss", "-Hltn"],
        0,
        "LISTEN 0 4096 127.0.0.1:9004 0.0.0.0:*\nLISTEN 0 4096 127.0.0.1:9005 0.0.0.0:*\nLISTEN 0 4096 127.0.0.1:9001 0.0.0.0:*\nLISTEN 0 4096 10.0.0.10:443 0.0.0.0:*\nLISTEN 0 4096 [::]:22 [::]:*\n",
    );
    host.put_file(CERT, 0o644, (SVC, SVC), b"CERT");
    host.reply(
        &[
            "/usr/bin/openssl",
            "x509",
            "-noout",
            "-checkend",
            "1814400",
            "-in",
            CERT,
        ],
        0,
        "",
    );
    host.put_dir(STATE, 0o700, (SVC, SVC));
    host.put_dir(ETC, 0o750, (0, SVC));
    for f in ["nanohub.env", "nanodep.env", "scep.env", "dns.env"] {
        host.put_file(&format!("{ETC}/{f}"), 0o600, (0, 0), b"K=v\n");
    }
    host
}

fn go(host: &FakeHost, acme_on: bool, ignore: &[String]) -> (bool, String) {
    let (c, a) = (config(), acme());
    let v = Verify {
        config: &c,
        acme: acme_on.then_some(&a),
        ignore,
    };
    let net = FakeNet::new(Default::default());
    let mut out = Vec::new();
    let ok = with_env(host, &net, |env| run(env, &v, &mut out)).unwrap();
    (ok, String::from_utf8(out).unwrap())
}

/// Catches a verify that reports a healthy, exactly installed host as anything but all-clear:
/// every check is `ok`, every file `same`, the summary says so and the answer is true.
#[test]
fn a_host_as_installed_passes_every_check_and_every_file_is_the_same() {
    let host = installed();
    let (ok, shown) = go(&host, true, &[]);
    assert!(ok, "{shown}");
    assert!(shown.ends_with("verify: all checks passed\n"));
    assert!(!shown.contains("FAIL"));
    assert_eq!(
        shown.lines().filter(|l| l.starts_with("ok    ")).count(),
        6 + 4 + 1 + 1 + 2 + 4
    );
    assert_eq!(
        shown
            .lines()
            .filter(|l| l.starts_with("same       "))
            .count(),
        6 + 1 + 4
    );
    assert!(shown.contains("same       /etc/systemd/system/js-mdm.slice\n"));
    assert!(shown.contains("same       /etc/js-mdm/Caddyfile\n"));
    assert!(!shown.contains("NOTE"));
}

/// Catches a comparison that counts comments: a unit whose comment lines were reworded or
/// added is `same`; one whose directive changed is `differs` with both sha256 values; a unit
/// that is gone is `missing`. The exit answer is false while any file is not the same.
#[test]
fn files_are_same_differs_or_missing_and_comments_do_not_count() {
    let host = installed();
    let rendered = render(&config()).unwrap();
    let scep = rendered
        .units
        .iter()
        .find(|u| u.name == "js-mdm-scep.service")
        .unwrap();
    let reworded = format!(
        "# a different header\n# more words\n{}   # indented note\n",
        scep.text.replace('#', "# x")
    );
    host.put_file(
        &format!("{UNITS}/js-mdm-scep.service"),
        0o644,
        (0, 0),
        reworded.as_bytes(),
    );
    let (ok, shown) = go(&host, true, &[]);
    assert!(ok, "{shown}");
    assert!(shown.contains("same       /etc/systemd/system/js-mdm-scep.service\n"));

    let nano = rendered
        .units
        .iter()
        .find(|u| u.name == "js-mdm-nanohub.service")
        .unwrap();
    let edited = nano.text.replace("Restart=on-failure", "Restart=always");
    assert_ne!(
        edited, nano.text,
        "the template has no Restart=on-failure to edit"
    );
    host.put_file(
        &format!("{UNITS}/js-mdm-nanohub.service"),
        0o644,
        (0, 0),
        edited.as_bytes(),
    );
    host.remove(&format!("{UNITS}/js-mdm-cert.timer")).unwrap();
    let (ok, shown) = go(&host, true, &[]);
    assert!(!ok);
    let want = format!(
        "differs    /etc/systemd/system/js-mdm-nanohub.service: installed sha256 {}, rendered sha256 {}\n",
        sha(&edited),
        sha(&nano.text)
    );
    assert!(shown.contains(&want), "{shown}");
    assert!(shown.contains("missing    /etc/systemd/system/js-mdm-cert.timer\n"));
    assert!(shown.ends_with("verify: 2 check(s) failed\n"), "{shown}");
}

/// Catches `--ignore` that is not applied to the installed text (a host installed by another
/// tool carries a phrase the render lacks), and one that hides a real difference.
#[test]
fn ignore_removes_text_from_the_installed_file_before_comparing() {
    let host = installed();
    let rendered = render(&config()).unwrap();
    let slice = &rendered.units[0];
    let line = slice
        .text
        .lines()
        .find(|l| l.starts_with("Description="))
        .unwrap();
    let old = slice.text.replace(line, &format!("{line} on the dev box"));
    host.put_file(
        &format!("{UNITS}/{}", slice.name),
        0o644,
        (0, 0),
        old.as_bytes(),
    );
    let (ok, shown) = go(&host, true, &[]);
    assert!(!ok);
    assert!(shown.contains("differs    /etc/systemd/system/js-mdm.slice: installed sha256 "));
    let (ok, shown) = go(&host, true, &[" on the dev box".to_owned()]);
    assert!(ok, "{shown}");
    let (ok, _) = go(&host, true, &["Slice".to_owned()]);
    assert!(!ok);
}

/// Catches a verify that needs root for what anyone may read: as another user the world-readable
/// units compare, a file this user may not read is `unreadable`, and a note says why the checks
/// of root-only files fail.
#[test]
fn another_user_compares_readable_files_and_reports_the_rest_unreadable() {
    let host = {
        let h = installed();
        // The same machine seen by uid 1000.
        let other = FakeHost::new(1000);
        for p in h.paths() {
            let n = h.node(&p).unwrap();
            match n.kind {
                js_host_step::Kind::Dir => other.put_dir(&p, n.mode, (n.uid, n.gid)),
                _ => other.put_file(&p, n.mode, (n.uid, n.gid), &n.bytes),
            }
        }
        other.add_user("js-mdm", SVC);
        other.reply(&["/usr/bin/ss", "-Hltn"], 0, "");
        other.reply(
            &[
                "/usr/bin/openssl",
                "x509",
                "-noout",
                "-checkend",
                "1814400",
                "-in",
                CERT,
            ],
            0,
            "",
        );
        for unit in ACTIVE {
            other.reply(&[SYSTEMCTL, "is-active", unit], 0, "active\n");
        }
        other.deny_read(&format!("{ETC}/Caddyfile"));
        other
    };
    let (ok, shown) = go(&host, true, &[]);
    assert!(!ok);
    assert!(
        shown.starts_with("NOTE: not root: the checks of root-only files fail; run it as root\n")
    );
    assert!(shown.contains("same       /etc/systemd/system/js-mdm-scep.service\n"));
    assert!(
        shown.contains("unreadable /etc/js-mdm/Caddyfile: permission denied"),
        "{shown}"
    );
}

/// Catches each health check that cannot fail: an inactive unit, a listener on the wrong
/// address, a missing listener, something on port 80, an expiring or absent certificate, a
/// state directory with the wrong mode, a secret too open and a missing user.
#[test]
fn each_health_check_fails_when_its_condition_does() {
    let host = installed();
    host.reply(
        &[SYSTEMCTL, "is-active", "js-mdm-proxy.service"],
        3,
        "inactive\n",
    );
    host.reply(
        &["/usr/bin/ss", "-Hltn"],
        0,
        "LISTEN 0 1 0.0.0.0:9004 0.0.0.0:*\nLISTEN 0 1 127.0.0.1:9001 *:*\nLISTEN 0 1 0.0.0.0:443 *:*\nLISTEN 0 1 10.0.0.10:443 *:*\nLISTEN 0 1 0.0.0.0:80 *:*\nLISTEN 0 1 [::]:80 *:*\n",
    );
    host.reply(
        &[
            "/usr/bin/openssl",
            "x509",
            "-noout",
            "-checkend",
            "1814400",
            "-in",
            CERT,
        ],
        1,
        "",
    );
    host.put_dir(STATE, 0o755, (SVC, SVC));
    host.put_file(&format!("{ETC}/dns.env"), 0o644, (0, 0), b"K=v\n");
    let (ok, shown) = go(&host, true, &[]);
    assert!(!ok);
    for line in [
        "FAIL  js-mdm-proxy.service is active: is inactive\n",
        "FAIL  NanoHUB listens on 127.0.0.1:9004 only: listening on 0.0.0.0:9004\n",
        "FAIL  SCEP listens on 127.0.0.1:9005 only: nothing listens there\n",
        "ok    depserver listens on 127.0.0.1:9001 only\n",
        "FAIL  the proxy listens on 10.0.0.10:443 only: listening on 0.0.0.0:443, 10.0.0.10:443\n",
        "FAIL  nothing listens on port 80: listening on 0.0.0.0:80, [::]:80\n",
        "FAIL  the certificate is valid for 21 more days: expires sooner, or is not a certificate\n",
        "FAIL  /var/lib/js-mdm is a 0700 directory of js-mdm: is Dir 0755 uid 900, not Dir 0700 uid 900\n",
        "FAIL  /etc/js-mdm/dns.env is a 0600 file of root: is File 0644 uid 0, not File 0600 uid 0\n",
    ] {
        assert!(shown.contains(line), "missing {line:?} in\n{shown}");
    }
    // The certificate gone, and the service user gone.
    host.remove(CERT).unwrap();
    host.reply(&["/usr/bin/getent", "passwd", "js-mdm"], 2, "");
    let (_, shown) = go(&host, true, &[]);
    assert!(shown.contains(&format!(
        "FAIL  the certificate is valid for 21 more days: {CERT} is not there\n"
    )));
    assert!(shown.contains("is a 0700 directory of js-mdm: no user js-mdm\n"));
    host.remove(&format!("{ETC}/nanohub.env")).unwrap();
    let (_, shown) = go(&host, true, &[]);
    assert!(
        shown
            .contains("nanohub.env is a 0600 file of root: /etc/js-mdm/nanohub.env is not there\n")
    );
    // ss failing is reported on every listener check, not guessed.
    host.reply(&["/usr/bin/ss", "-Hltn"], 1, "");
    let (_, shown) = go(&host, true, &[]);
    assert_eq!(
        shown.matches("ss exited with status 1").count(),
        5,
        "{shown}"
    );
}

/// Catches a verify that panics or passes on a host where no program answers: every check
/// that needs one fails with the reason, and no file is `same`.
#[test]
fn a_bare_host_fails_every_check_and_reports_every_file_missing() {
    let host = FakeHost::new(0);
    let (ok, shown) = go(&host, true, &[]);
    assert!(!ok);
    assert!(shown.contains("FAIL  js-mdm-scep.service is active: fake host: unexpected command"));
    assert!(shown.contains("FAIL  the certificate is valid for 21 more days: /var/lib/js-mdm/lego/certificates/mdm.example.com.crt is not there"));
    assert_eq!(shown.matches("missing    ").count(), 11);
    assert!(!shown.contains("\nsame"));
    // A command that could not be started for the certificate check is a failed check too.
    let (c, a) = (config(), acme());
    let v = Verify {
        config: &c,
        acme: Some(&a),
        ignore: &[],
    };
    let bare = FakeHost::new(0);
    bare.put_file(CERT, 0o644, (0, 0), b"c");
    let rows = js_mdm::verify::checks(&bare, &v);
    let cert = rows
        .iter()
        .find(|r| r.name.starts_with("the certificate"))
        .unwrap();
    assert!(
        cert.result
            .as_ref()
            .unwrap_err()
            .contains("unexpected command")
    );
}

/// Catches the comparison left out when the settings have no ACME keys: the certificate job's
/// units are skipped, said so, and not counted as missing.
#[test]
fn without_acme_settings_the_certificate_units_are_skipped() {
    let host = installed();
    let (ok, shown) = go(&host, false, &[]);
    assert!(ok, "{shown}");
    assert!(
        shown.contains("skipped  the certificate job's units: the settings have no ACME keys\n")
    );
    assert_eq!(
        shown
            .lines()
            .filter(|l| l.starts_with("same       "))
            .count(),
        6 + 1
    );
}

/// Catches `compare` that reads an installed file as something it is not: invalid UTF-8 is
/// compared as text with replacement characters (so it differs), and an empty file differs.
#[test]
fn compare_handles_odd_bytes_and_the_check_display_covers_each_form() {
    let host = FakeHost::new(0);
    host.put_file("/f", 0o644, (0, 0), &[0xff, 0xfe, b'\n']);
    assert!(matches!(
        compare(&host, "/f", "x\n", &[]),
        Compared::Differs { .. }
    ));
    assert_eq!(
        compare(&host, "/f", "\u{fffd}\u{fffd}\n", &[]),
        Compared::Same
    );
    assert_eq!(compare(&host, "/none", "x", &[]), Compared::Missing);
    let c = js_mdm::verify::Check {
        name: "n".to_owned(),
        result: Ok("seen".to_owned()),
    };
    assert_eq!(c.to_string(), "ok    n: seen");
}
