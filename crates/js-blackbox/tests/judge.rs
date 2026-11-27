//! The judge of the black-box tests, on its own: the wait and the hold on a clock the test
//! moves, the readers on answers written out here, and the farm's shapes rendered. Nothing here
//! waits, starts a program or opens a socket.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use js_blackbox::{
    AGENT_FLAGS, BACK_WITHIN, BURST, Busiest, CALL_WINDOW, FARM_POOL, Held, HostSeen, JobSeen,
    MAC_POOL, MAX_PLACEMENTS, MOST_CALLS_IN_WINDOW, OUTAGE, PROBE_EVERY, PROVIDER_OUTAGE,
    READY_WITHIN, ResultSeen, SERVER_ARGS, SESSION_WITHIN, STALL, Seen, Version, WAITING, Waited,
    Watch, arguments, busiest, host, job, loaded_image, own_fault, per_second, previous_version,
    provider_calls, render, result,
};
use js_types::Monotonic;
use serde_json::json;

/// A clock the test moves.
#[derive(Debug, Clone, Default)]
struct Hand(Arc<Mutex<Duration>>);

impl Hand {
    fn advance(&self, by: Duration) {
        *self.0.lock().expect("the clock") += by;
    }
}

impl Monotonic for Hand {
    fn reading(&self) -> Duration {
        *self.0.lock().expect("the clock")
    }
}

const S: Duration = Duration::from_secs(1);

/// Runs `body` with a watch on `clock` that probes every second, its pause moving the clock.
fn with_watch<R>(clock: &Hand, body: impl FnOnce(&mut Watch<'_>) -> R) -> R {
    let paused = clock.clone();
    let mut pause = move |by: Duration| paused.advance(by);
    let mut watch = Watch::new(clock, &mut pause, S);
    body(&mut watch)
}

#[test]
fn the_pass_conditions_are_60_60_30_and_60_seconds() {
    assert_eq!(READY_WITHIN, Duration::from_secs(60));
    assert_eq!(SESSION_WITHIN, Duration::from_secs(60));
    assert_eq!(BACK_WITHIN, Duration::from_secs(30));
    assert_eq!(OUTAGE, Duration::from_secs(60));
    assert_eq!(PROBE_EVERY, Duration::from_secs(1));
    assert_eq!(
        (BURST, MAX_PLACEMENTS, MOST_CALLS_IN_WINDOW, WAITING),
        (20, 2, 20, 6)
    );
    assert_eq!(
        (STALL, CALL_WINDOW, PROVIDER_OUTAGE),
        (
            Duration::from_secs(30),
            Duration::from_secs(30),
            Duration::from_secs(60)
        )
    );
}

/// A wait with a 1 s pace whose probe `n` (from 1) first moves the clock by `late(n)`, then
/// says yes when `yes(n)`, and no ("probe n") otherwise.
fn wait(limit: Duration, late: fn(u32) -> Duration, yes: fn(u32) -> bool) -> Waited {
    let clock = Hand::default();
    let moved = clock.clone();
    let mut n = 0;
    with_watch(&clock, |w| {
        w.until(limit, &mut || {
            n += 1;
            moved.advance(late(n));
            if yes(n) {
                Seen::Yes
            } else {
                Seen::No(format!("probe {n}"))
            }
        })
    })
}

#[test]
fn a_wait_is_met_by_the_first_probe_that_sees_it_within_the_limit() {
    let waited = wait(Duration::from_secs(60), |_| Duration::ZERO, |n| n == 3);
    assert_eq!(
        waited,
        Waited::Met {
            after: Duration::from_secs(2)
        }
    );
    assert_eq!(
        waited.within("the session", SESSION_WITHIN),
        Ok(Duration::from_secs(2))
    );
}

#[test]
fn a_wait_that_never_sees_it_misses_at_the_limit_with_what_it_saw_last() {
    // Catches a wait that gives up early, or never: the last probe is the one that ends at the
    // limit.
    let waited = wait(Duration::from_secs(30), |_| Duration::ZERO, |_| false);
    assert_eq!(
        waited,
        Waited::Missed {
            after: Duration::from_secs(30),
            last: "probe 31".to_owned()
        }
    );
    assert_eq!(
        waited.within("back in a session", BACK_WITHIN),
        Err(
            "back in a session: not within 30 s (gave up after 30000 ms); last seen: probe 31"
                .to_owned()
        )
    );
}

#[test]
fn a_probe_that_sees_it_only_after_the_limit_is_a_miss() {
    // Catches a wait that counts a late success: a session first seen at 61 s fails the 60 s
    // condition even though the probe said yes.
    let waited = wait(
        Duration::from_secs(60),
        |_| Duration::from_secs(61),
        |_| true,
    );
    assert_eq!(
        waited,
        Waited::Missed {
            after: Duration::from_secs(61),
            last: "seen only after the limit".to_owned()
        }
    );
    // Exactly at the limit is within it.
    let waited = wait(
        Duration::from_secs(60),
        |_| Duration::from_secs(60),
        |_| true,
    );
    assert_eq!(
        waited,
        Waited::Met {
            after: Duration::from_secs(60)
        }
    );
}

/// A hold (`until_done` false) or a hold until done (true) of `span` with a 1 s pace, whose
/// probe `n` (from 1) answers `answer(n)`.
fn hold(
    span: Duration,
    until_done: bool,
    answer: fn(u32) -> Result<bool, String>,
) -> (Held, Duration) {
    let clock = Hand::default();
    let mut n = 0;
    let held = with_watch(&clock, |w| {
        if until_done {
            w.hold_until(span, &mut || {
                n += 1;
                answer(n)
            })
        } else {
            w.hold(span, &mut || {
                n += 1;
                answer(n).map(|_| ())
            })
        }
    });
    (held, clock.reading())
}

#[test]
fn a_hold_probes_the_whole_span_and_breaks_at_the_first_failure() {
    let (held, _) = hold(OUTAGE, false, |_| Ok(false));
    // The first probe at 0 s, the last at 60 s.
    assert_eq!(held, Held::Kept { probes: 61 });
    assert_eq!(held.kept("the session"), Ok(61));

    let (held, _) = hold(OUTAGE, false, |n| {
        if n == 11 {
            Err(format!("{} at probe {n}", js_blackbox::LOST))
        } else {
            Ok(false)
        }
    });
    assert_eq!(
        held,
        Held::Broken {
            after: Duration::from_secs(10),
            seen: "HOST_STATE_LOST at probe 11".to_owned()
        }
    );
    assert_eq!(
        held.kept("the host is never shown lost"),
        Err(
            "the host is never shown lost: broken 10000 ms in: HOST_STATE_LOST at probe 11"
                .to_owned()
        )
    );
}

#[test]
fn a_hold_until_done_ends_at_the_probe_that_is_done_and_breaks_when_none_is() {
    // Catches a hold that runs on after the job it watches ended, or that passes a watch that
    // never saw the end.
    let (held, at) = hold(Duration::from_secs(600), true, |n| Ok(n == 5));
    assert_eq!(held, Held::Kept { probes: 5 });
    assert_eq!(at, Duration::from_secs(4));

    let (held, _) = hold(Duration::from_secs(3), true, |_| Ok(false));
    assert_eq!(
        held,
        Held::Broken {
            after: Duration::from_secs(3),
            seen: "not done within 3 s".to_owned()
        }
    );

    let (held, _) = hold(Duration::from_secs(3), true, |_| {
        Err("listed lost".to_owned())
    });
    assert_eq!(
        held,
        Held::Broken {
            after: Duration::ZERO,
            seen: "listed lost".to_owned()
        }
    );
}

#[test]
fn a_host_is_read_from_the_list_by_its_id() {
    let list = json!({"hosts": [
        {"id": "other", "state": "HOST_STATE_LOST"},
        {"id": "blackbox-node", "state": "HOST_STATE_LIVE", "agentVersion": "0.1.430"},
        {"id": "quiet"}
    ]});
    assert_eq!(
        host(&list, "blackbox-node"),
        Some(HostSeen {
            state: "HOST_STATE_LIVE".to_owned(),
            agent_version: "0.1.430".to_owned()
        })
    );
    // A host whose state and version are at their defaults has them left out.
    assert_eq!(
        host(&list, "quiet"),
        Some(HostSeen {
            state: "HOST_STATE_UNSPECIFIED".to_owned(),
            agent_version: String::new()
        })
    );
    assert_eq!(host(&list, "absent"), None);
    assert_eq!(host(&json!({}), "blackbox-node"), None);
    assert_eq!(host(&json!({"hosts": {}}), "blackbox-node"), None);
}

#[test]
fn a_job_and_its_result_are_read_from_the_gateways_answers() {
    let running = json!({"job": {
        "id": "j", "state": "JOB_STATE_RUNNING", "hostId": "blackbox-node", "attempt": 1
    }});
    let seen = job(&running).expect("a job");
    assert_eq!(
        seen,
        JobSeen {
            state: "JOB_STATE_RUNNING".to_owned(),
            host_id: "blackbox-node".to_owned(),
            attempt: 1,
            host_silent: false,
            start_failure: String::new(),
            start_failure_detail: String::new(),
        }
    );
    assert!(!seen.ended());
    let refused = json!({"job": {
        "state": "JOB_STATE_FAILED_TO_START",
        "startFailure": "START_FAILURE_CODE_AGENT_REFUSED",
        "startFailureDetail": "provider-refused"
    }});
    let seen = job(&refused).expect("a job");
    assert_eq!(
        (
            seen.start_failure.as_str(),
            seen.start_failure_detail.as_str()
        ),
        ("START_FAILURE_CODE_AGENT_REFUSED", "provider-refused")
    );
    let silent =
        json!({"job": {"state": "JOB_STATE_RUNNING", "hostSilentSinceMs": "1800000000000"}});
    let seen = job(&silent).expect("a job");
    assert!(seen.host_silent);
    assert_eq!(seen.host_id, "");
    assert_eq!(seen.attempt, 0);
    for state in [
        "JOB_STATE_SUCCEEDED",
        "JOB_STATE_FAILED",
        "JOB_STATE_CANCELLED",
        "JOB_STATE_LOST",
        "JOB_STATE_FAILED_TO_START",
    ] {
        assert!(
            job(&json!({"job": {"state": state}})).expect(state).ended(),
            "{state}"
        );
    }
    assert_eq!(job(&json!({})), Err("no job in {}".to_owned()));
    assert_eq!(
        job(&json!({"job": 3})),
        Err("no job in {\"job\":3}".to_owned())
    );
    assert_eq!(
        job(&json!({"job": {"id": "j"}})),
        Err("a job with no state: {\"id\":\"j\"}".to_owned())
    );

    let zero = json!({"result": {"state": "JOB_STATE_SUCCEEDED", "exitCode": 0, "attempt": 1}});
    assert_eq!(
        result(&zero),
        Ok(ResultSeen {
            state: "JOB_STATE_SUCCEEDED".to_owned(),
            exit_code: Some(0),
            attempt: 1,
        })
    );
    let no_exit = json!({"result": {"state": "JOB_STATE_LOST"}});
    assert_eq!(
        result(&no_exit),
        Ok(ResultSeen {
            state: "JOB_STATE_LOST".to_owned(),
            exit_code: None,
            attempt: 0,
        })
    );
    assert_eq!(result(&json!([])), Err("no result in []".to_owned()));
    assert_eq!(
        result(&json!({"result": {}})),
        Err("a result with no state: {}".to_owned())
    );
}

#[test]
fn the_image_docker_loaded_is_read_from_what_it_printed() {
    let id = format!("sha256:{}", "ab".repeat(32));
    assert_eq!(
        loaded_image(&format!("Loaded image ID: {id}\n")),
        Some(id.clone())
    );
    assert_eq!(
        loaded_image(&format!("something first\nLoaded image: {id}  \n")),
        Some(id.clone())
    );
    // A name, a short or an upper-case digest, and nothing loaded are not an image id.
    assert_eq!(loaded_image("Loaded image: js-lease-server:latest\n"), None);
    assert_eq!(loaded_image("Loaded image ID: sha256:abc\n"), None);
    assert_eq!(
        loaded_image(&format!("Loaded image ID: sha256:{}\n", "AB".repeat(32))),
        None
    );
    assert_eq!(loaded_image(""), None);
}

#[test]
fn versions_parse_order_and_print() {
    assert_eq!(Version::parse("0.1.430"), Some(Version(0, 1, 430)));
    assert_eq!(Version(0, 1, 430).to_string(), "0.1.430");
    assert!(Version(0, 1, 99) < Version(0, 1, 100));
    assert!(Version(0, 2, 0) > Version(0, 1, 999));
    for bad in [
        "",
        "0.1",
        "0.1.2.3",
        "0.1.x",
        "0..1",
        "0.1.-1",
        "0.1.99999999999999999999",
    ] {
        assert_eq!(Version::parse(bad), None, "{bad:?}");
    }
}

#[test]
fn the_previous_version_is_the_newest_below_the_release() {
    let madison = "  js-agent | 0.1.431 | https://packages.example/apt testing/main amd64 Packages\n\
                   \x20 js-agent | 0.1.430 | https://packages.example/apt testing/main amd64 Packages\n\
                   \x20 js-agent | 0.1.428 | https://packages.example/apt testing/main amd64 Packages\n\
                   \x20 js-agent | 0.1.429 | https://packages.example/apt testing/main amd64 Packages\n\
                   a line of no version\n";
    assert_eq!(
        previous_version(madison, Version(0, 1, 430)),
        Some(Version(0, 1, 429))
    );
    assert_eq!(previous_version(madison, Version(0, 1, 428)), None);
    assert_eq!(previous_version("", Version(0, 1, 430)), None);
}

fn values(pairs: &[(&'static str, &str)]) -> BTreeMap<&'static str, String> {
    pairs.iter().map(|(k, v)| (*k, (*v).to_owned())).collect()
}

#[test]
fn a_shape_renders_only_with_a_value_for_each_placeholder_and_a_placeholder_for_each_value() {
    let shape = "--server=https://{name}:{port}\n--pool={pool}\n";
    assert_eq!(
        render(
            shape,
            &values(&[("name", "js.test"), ("port", "7301"), ("pool", "p")])
        ),
        Ok("--server=https://js.test:7301\n--pool=p\n".to_owned())
    );
    // A placeholder twice takes its value twice.
    assert_eq!(
        render("{a}-{a}", &values(&[("a", "x")])),
        Ok("x-x".to_owned())
    );
    assert_eq!(
        render(shape, &values(&[("name", "js.test"), ("port", "7301")])),
        Err("the placeholder {pool} has no value".to_owned())
    );
    assert_eq!(
        render("plain", &values(&[("pool", "p")])),
        Err("the value pool fills no placeholder".to_owned())
    );
    for (bad, at) in [("a {open", 2), ("a } alone", 2), ("{}", 0), ("{a{b}", 0)] {
        assert_eq!(
            render(bad, &values(&[("a", "x"), ("b", "y")])),
            Err(format!("a brace that opens no placeholder at byte {at}")),
            "{bad:?}"
        );
    }
}

#[test]
fn an_argument_shape_is_its_lines_without_comments() {
    assert_eq!(
        arguments("# a comment\n--tick\n\n  1s  \n# another\n"),
        vec!["--tick".to_owned(), "1s".to_owned()]
    );
}

#[test]
fn the_farm_shapes_render_with_the_values_the_black_box_test_gives() {
    // The test fills exactly these; a shape that gains or loses a placeholder fails here before
    // it fails on a runner.
    let flags = render(
        AGENT_FLAGS,
        &values(&[
            ("server_name", "js.blackbox.test"),
            ("agent_port", "7301"),
            ("pool", "blackbox"),
            ("registry_port", "5000"),
            ("egress_endpoint", "192.0.2.5:9300"),
        ]),
    )
    .expect("the agent's flags");
    let lines = arguments(&flags);
    assert_eq!(lines[0], "--server=https://js.blackbox.test:7301");
    assert!(lines.contains(&"--host-cert=/var/lib/hostkeeper/identity/host.crt".to_owned()));
    assert!(lines.contains(&"--egress-allow=192.0.2.5:9300".to_owned()));
    // The flags the packaged unit already sets are not repeated.
    for set in [
        "--token-file",
        "--health-listen",
        "--state-root",
        "--scratch-root",
        "--job-user",
    ] {
        assert!(!flags.contains(set), "{set}");
    }

    let args = render(
        SERVER_ARGS,
        &values(&[
            ("agent_port", "7301"),
            ("job_port", "7302"),
            ("ready_port", "7303"),
            ("metrics_port", "7304"),
        ]),
    )
    .expect("the server's arguments");
    let args = arguments(&args);
    assert_eq!(
        args[..2],
        ["--listen".to_owned(), "127.0.0.1:7301".to_owned()]
    );
    assert!(args.windows(2).any(|w| w == ["--boot", "resume"]));
    assert!(
        args.windows(2)
            .any(|w| w == ["--store", "/run/secrets/store.url"])
    );

    for pool in [FARM_POOL, MAC_POOL] {
        let text = render(pool, &values(&[("pool", "blackbox")])).expect("a pool file");
        assert!(text.contains("name = \"blackbox\""));
        assert!(text.contains("heartbeat_interval = \"1s\""));
    }
}

/// Asserts the fake provider's list of calls is read line by line, and each line that is not a
/// call is refused by its full text.
#[test]
fn the_provider_calls_are_read_from_the_fakes_list() {
    let calls = provider_calls("0 Prepare ok\n1500 Mount unavailable\n").expect("two calls");
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[1].at, Duration::from_millis(1500));
    assert_eq!(
        (calls[1].method.as_str(), calls[1].answer.as_str()),
        ("Mount", "unavailable")
    );
    assert_eq!(provider_calls(""), Ok(Vec::new()));
    for line in ["soon Prepare ok", "0 Prepare", "0 Prepare ok now"] {
        assert_eq!(
            provider_calls(line),
            Err(format!("not a call of the fake provider: {line:?}"))
        );
    }
}

/// Asserts the busiest window: the calls of the one method, a call exactly one window after
/// another in a window of its own, and the first window of the most kept when a later one ties.
#[test]
fn the_busiest_window_counts_one_method() {
    let listed = "0 Prepare ok\n10000 Mount ok\n29999 Prepare ok\n30000 Prepare ok\n\
                  31000 Prepare ok\n59000 Prepare ok\n";
    let calls = provider_calls(listed).expect("calls");
    assert_eq!(
        busiest(&calls, "Prepare", CALL_WINDOW),
        Busiest {
            count: 4,
            from: Duration::from_millis(29_999)
        }
    );
    assert_eq!(
        busiest(&calls, "Mount", CALL_WINDOW),
        Busiest {
            count: 1,
            from: Duration::from_millis(10_000)
        }
    );
    assert_eq!(
        busiest(&calls, "Release", CALL_WINDOW),
        Busiest {
            count: 0,
            from: Duration::ZERO
        }
    );
    assert_eq!(per_second(&calls, "Prepare"), "0:1 29:1 30:1 31:1 59:1");
    // A Mount held 25 s is listed after one that came later and was answered at once.
    let held =
        provider_calls("5000 Mount ok\n0 Mount unavailable\n31000 Mount ok\n").expect("calls");
    assert_eq!(
        busiest(&held, "Mount", CALL_WINDOW),
        Busiest {
            count: 2,
            from: Duration::ZERO
        }
    );
    assert_eq!(per_second(&held, "Mount"), "0:1 5:1 31:1");
    let same = provider_calls("100 Mount ok\n900 Mount ok\n1000 Mount ok\n").expect("calls");
    assert_eq!(per_second(&same, "Mount"), "0:2 1:1");
    assert_eq!(per_second(&same, "Prepare"), "");
}

/// Asserts which ends are a job's own fault: all but a success and a start that failed for the
/// host's unavailable input.
#[test]
fn a_job_ends_as_its_own_fault_unless_it_succeeded_or_its_input_was_unavailable() {
    let ended = |state: &str, failure: &str| JobSeen {
        state: state.to_owned(),
        host_id: String::new(),
        attempt: 1,
        host_silent: false,
        start_failure: failure.to_owned(),
        start_failure_detail: String::new(),
    };
    assert!(!own_fault(&ended("JOB_STATE_SUCCEEDED", "")));
    assert!(!own_fault(&ended(
        "JOB_STATE_FAILED_TO_START",
        "START_FAILURE_CODE_INPUT_UNAVAILABLE"
    )));
    assert!(own_fault(&ended(
        "JOB_STATE_FAILED_TO_START",
        "START_FAILURE_CODE_AGENT_REFUSED"
    )));
    assert!(own_fault(&ended("JOB_STATE_FAILED", "")));
}
