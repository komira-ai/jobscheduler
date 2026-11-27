//! The read-only script `tools/held/submit-rollout/submit_rpc.sh`, run by `bash` with stand-ins
//! first on `PATH`: a `docker` that logs each call and answers the running containers of a
//! service, each container's labels and the `docker_gwbridge` addresses from files of the test;
//! a `curl` that answers a frontend's metrics from a file named after its address (and fails for
//! one with no file); and a `date` and a `sleep` that share a clock file, so a wait moves the
//! clock without taking time. So each test pins what a reading holds, the numbers of a window
//! (the p50 and p90 worked out by hand from the buckets), which windows and frontends are refused
//! or left out, and the input refused before anything runs. Nothing reaches a Swarm or a network.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use js_testkit::spawn::install_script;

/// The repository root.
fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

/// The js-server container, with the labels the script reads.
const JS: &str = "js1";

/// Two frontends, and their addresses on `docker_gwbridge` (documentation addresses).
const FE_A: &str = "aaaaaaaaaaaaaaaa";
const FE_B: &str = "bbbbbbbbbbbbbbbb";

/// The addresses, as the script's `docker network inspect` prints them: id, then address/prefix.
/// A third container (not a frontend) is listed too, as the real network lists every container.
const GWBRIDGE: &str =
    "aaaaaaaaaaaaaaaa 192.0.2.4/16\nbbbbbbbbbbbbbbbb 192.0.2.7/16\nffffffffffffffff 192.0.2.9/16\n";

/// One frontend's `rpc` series for `form` at the bucket bounds 0.01, 0.025, 0.05, 0.1 and +Inf
/// (cumulative counts), with its sum, plus the lines the script must leave out.
fn metrics(form: &str, counts: [u64; 5], sum: &str) -> String {
    let les = ["0.01", "0.025", "0.05", "0.1", "+Inf"];
    let mut text = String::from(
        "# HELP kbf_executor_step_seconds one step of an execution\n\
         # TYPE kbf_executor_step_seconds histogram\n\
         kbf_executor_step_seconds_bucket{step=\"tree_root\",form=\"provider\",le=\"0.01\"} 7\n",
    );
    for (le, count) in les.iter().zip(counts) {
        text.push_str(&format!(
            "kbf_executor_step_seconds_bucket{{step=\"rpc\",form=\"{form}\",le=\"{le}\"}} {count}\n"
        ));
    }
    text.push_str(&format!(
        "kbf_executor_step_seconds_sum{{step=\"rpc\",form=\"{form}\"}} {sum}\n\
         kbf_executor_step_seconds_count{{step=\"rpc\",form=\"{form}\"}} {}\n\
         kbf_executor_phase_seconds_count{{phase=\"submit\"}} 3\n",
        counts[4]
    ));
    text
}

/// A reading as `snapshot` writes it, at unix second `at`, with the js-server line `js` and,
/// for each frontend, its id and its metrics text (only the `rpc` lines are kept, as the script
/// keeps them).
fn reading(at: u64, js: &str, frontends: &[(&str, String)]) -> String {
    let mut text = format!("# at at-{at} {at}\n{js}\n");
    for (id, m) in frontends {
        text.push_str(&format!("fe {id}\n"));
        for line in m.lines().filter(|l| l.contains("{step=\"rpc\",")) {
            text.push_str(line);
            text.push('\n');
        }
    }
    text
}

/// The js-server line of a reading: container, version, start time.
const JS_LINE: &str = "js js1 0.1.420 2026-11-26T20:00:00Z";

/// The worked window: frontend A's `provider` submits go from 20 to 100 (80 in the window: 30
/// under 25 ms, 30 more under 50 ms, 20 more under 100 ms, 3.0 s in all) and frontend B's
/// `manifests` submits from 0 to 20 (all under 10 ms, 0.1 s in all), over 1,800 s.
fn worked() -> (String, String) {
    let first = reading(
        1000,
        JS_LINE,
        &[
            (FE_A, metrics("provider", [0, 10, 20, 20, 20], "0.5")),
            (FE_B, metrics("manifests", [0, 0, 0, 0, 0], "0")),
        ],
    );
    let second = reading(
        2800,
        JS_LINE,
        &[
            (FE_A, metrics("provider", [0, 40, 80, 100, 100], "3.5")),
            (FE_B, metrics("manifests", [20, 20, 20, 20, 20], "0.1")),
        ],
    );
    (first, second)
}

/// The numbers of the worked window. All forms: 100 submits, cumulative 20, 50, 80, 100. The
/// p50 (rank 50) is in the 10-25 ms bucket, at its top: 25.0 ms. The p90 (rank 90) is in the
/// 50-100 ms bucket, 10 of its 20 in: 75.0 ms. Mean 3.1 s / 100 = 31.0 ms. `manifests`: rank 10
/// and 18 of 20 in the 0-10 ms bucket, 5.0 and 9.0 ms. `provider`: rank 40 is 10 of the 30 in
/// 25-50 ms, 33.3 ms; rank 72 is 12 of the 20 in 50-100 ms, 80.0 ms; mean 37.5 ms.
const WORKED: [&str; 6] = [
    "window: at-1000 to at-2800 (1800 s)",
    "js-server: 0.1.420",
    "frontends: 2 kept, dropped: none",
    "rpc: count 100 (0.06/s), p50 25.0 ms, p90 75.0 ms, mean 31.0 ms",
    "rpc form=manifests: count 20 (0.01/s), p50 5.0 ms, p90 9.0 ms, mean 5.0 ms",
    "rpc form=provider: count 80 (0.04/s), p50 33.3 ms, p90 80.0 ms, mean 37.5 ms",
];

/// A scratch directory of its own for each test (outside the checkout), holding the stand-ins
/// in `bin/`, their answers, the clock and the script's `out/`.
struct Rig {
    dir: PathBuf,
    _scratch: js_testkit::dir::TestDir,
}

/// What one run of the script did.
#[derive(Debug)]
struct Ran {
    status: i32,
    stdout: String,
    stderr: String,
    /// Each call of the stand-in `docker`, its arguments joined by spaces.
    docker: Vec<String>,
    /// Each call of the stand-in `curl`, its arguments joined by spaces.
    curl: Vec<String>,
    /// Each call of the stand-in `sleep`, its argument.
    sleep: Vec<String>,
}

impl Rig {
    /// A rig with one js-server container and the two frontends, both answering; the clock
    /// stands at 1000.
    fn new(name: &str) -> Rig {
        let scratch = js_testkit::dir::TestDir::under(
            env!("CARGO_TARGET_TMPDIR"),
            &format!("submit-rpc-{name}"),
        );
        let dir = scratch.path().to_path_buf();
        fs::create_dir_all(dir.join("bin")).unwrap();
        fs::create_dir_all(dir.join("out")).unwrap();
        install_script(
            &dir.join("bin/docker"),
            "#!/bin/sh\n\
             printf '%s\\n' \"$*\" >> \"$FAKE_DIR/docker.log\"\n\
             case \"$1\" in\n\
             ps) for a in \"$@\"; do case \"$a\" in name=*) n=${a#name=} ;; esac; done; cat \"$FAKE_DIR/ps-$n.txt\" ;;\n\
             inspect) for a in \"$@\"; do last=$a; done; cat \"$FAKE_DIR/inspect-$last.txt\" ;;\n\
             network) cat \"$FAKE_DIR/gwbridge.txt\" ;;\n\
             *) exit 9 ;;\n\
             esac\n",
            "755",
        );
        install_script(
            &dir.join("bin/curl"),
            "#!/bin/sh\n\
             printf '%s\\n' \"$*\" >> \"$FAKE_DIR/curl.log\"\n\
             for a in \"$@\"; do url=$a; done\n\
             hp=${url#http://}; hp=${hp%/metrics}\n\
             [ -f \"$FAKE_DIR/metrics-$hp\" ] || exit 7\n\
             cat \"$FAKE_DIR/metrics-$hp\"\n",
            "755",
        );
        install_script(
            &dir.join("bin/date"),
            "#!/bin/sh\n\
             case \"$*\" in\n\
             *'+%s'*) cat \"$FAKE_DIR/clock\" ;;\n\
             *) printf 'at-%s\\n' \"$(cat \"$FAKE_DIR/clock\")\" ;;\n\
             esac\n",
            "755",
        );
        install_script(
            &dir.join("bin/sleep"),
            "#!/bin/sh\n\
             printf '%s\\n' \"$1\" >> \"$FAKE_DIR/sleep.log\"\n\
             echo $(( $(cat \"$FAKE_DIR/clock\") + $1 )) > \"$FAKE_DIR/clock\"\n",
            "755",
        );
        let rig = Rig {
            dir,
            _scratch: scratch,
        };
        rig.set("clock", "1000\n");
        rig.set("ps-js-server_server.txt", &format!("{JS}\n"));
        rig.set(
            &format!("inspect-{JS}.txt"),
            "0.1.420 2026-11-26T20:00:00Z\n",
        );
        rig.set("ps-kbf-frontend_server.txt", &format!("{FE_B}\n{FE_A}\n"));
        rig.set("gwbridge.txt", GWBRIDGE);
        rig.set(
            "metrics-192.0.2.4:8982",
            &metrics("provider", [0, 10, 20, 20, 20], "0.5"),
        );
        rig.set(
            "metrics-192.0.2.7:8982",
            &metrics("manifests", [0, 0, 0, 0, 0], "0"),
        );
        rig
    }

    /// Replaces an answer of a stand-in, or writes a reading the test hands to `compute`.
    fn set(&self, file: &str, text: &str) {
        fs::write(self.dir.join(file), text).unwrap();
    }

    /// The path of a file of the rig, as a string.
    fn path(&self, file: &str) -> String {
        self.dir.join(file).display().to_string()
    }

    /// Runs the script with `args`.
    fn run(&self, args: &[&str]) -> Ran {
        let path = format!(
            "{}:{}",
            self.dir.join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let out = Command::new("bash")
            .arg(root().join("tools/held/submit-rollout/submit_rpc.sh"))
            .args(args)
            .current_dir(&self.dir)
            .env("PATH", path)
            .env("FAKE_DIR", &self.dir)
            .output()
            .unwrap();
        let log = |name: &str| -> Vec<String> {
            fs::read_to_string(self.dir.join(name))
                .unwrap_or_default()
                .lines()
                .map(str::to_owned)
                .collect()
        };
        Ran {
            status: out.status.code().unwrap(),
            stdout: String::from_utf8(out.stdout).unwrap(),
            stderr: String::from_utf8(out.stderr).unwrap(),
            docker: log("docker.log"),
            curl: log("curl.log"),
            sleep: log("sleep.log"),
        }
    }

    /// Runs `compute` over two readings written to the rig.
    fn compute(&self, first: &str, second: &str) -> Ran {
        self.set("first.txt", first);
        self.set("second.txt", second);
        let (a, b) = (self.path("first.txt"), self.path("second.txt"));
        self.run(&["compute", &a, &b])
    }
}

/// Asserts that a snapshot holds the time, the js-server container with its version and start
/// time, and, for each frontend in id order, its `rpc` lines only; that it reads each frontend
/// at its `docker_gwbridge` address on the metrics port; and that it reads nothing else. It
/// catches a reading that keeps other steps' lines, or reads a container that is not a frontend.
#[test]
fn a_snapshot_keeps_the_rpc_lines_of_each_frontend() {
    let rig = Rig::new("snapshot");
    let file = rig.path("out/r.txt");
    let ran = rig.run(&["snapshot", &file]);
    assert_eq!(ran.status, 0, "{ran:?}");
    assert_eq!(ran.stdout, "");
    let want = reading(
        1000,
        "js js1 0.1.420 2026-11-26T20:00:00Z",
        &[
            (FE_A, metrics("provider", [0, 10, 20, 20, 20], "0.5")),
            (FE_B, metrics("manifests", [0, 0, 0, 0, 0], "0")),
        ],
    );
    assert_eq!(fs::read_to_string(&file).unwrap(), want);
    assert_eq!(
        ran.docker,
        [
            "ps --no-trunc --filter name=js-server_server --format {{.ID}}",
            "inspect --format {{index .Config.Labels \"org.opencontainers.image.version\"}} {{.State.StartedAt}} js1",
            "network inspect docker_gwbridge --format {{range $id, $c := .Containers}}{{$id}} {{$c.IPv4Address}}{{println}}{{end}}",
            "ps --no-trunc --filter name=kbf-frontend_server --format {{.ID}}",
        ]
    );
    assert_eq!(
        ran.curl,
        [
            "-fsS --max-time 10 http://192.0.2.4:8982/metrics",
            "-fsS --max-time 10 http://192.0.2.7:8982/metrics",
        ]
    );
}

/// Asserts that `-` writes the reading to standard output, and that `--port`, `--js-service` and
/// `--frontend-service` reach the calls.
#[test]
fn a_snapshot_to_standard_output_with_other_names() {
    let rig = Rig::new("stdout");
    rig.set("ps-js.txt", &format!("{JS}\n"));
    rig.set("ps-fe.txt", &format!("{FE_A}\n"));
    rig.set(
        "metrics-192.0.2.4:9000",
        &metrics("provider", [1, 1, 1, 1, 1], "0.001"),
    );
    let ran = rig.run(&[
        "snapshot",
        "-",
        "--port",
        "9000",
        "--js-service",
        "js",
        "--frontend-service",
        "fe",
    ]);
    assert_eq!(ran.status, 0, "{ran:?}");
    assert_eq!(
        ran.stdout,
        reading(
            1000,
            JS_LINE,
            &[(FE_A, metrics("provider", [1, 1, 1, 1, 1], "0.001"))]
        )
    );
    assert_eq!(
        ran.docker[0],
        "ps --no-trunc --filter name=js --format {{.ID}}"
    );
    assert_eq!(
        ran.docker[3],
        "ps --no-trunc --filter name=fe --format {{.ID}}"
    );
    assert_eq!(
        ran.curl,
        ["-fsS --max-time 10 http://192.0.2.4:9000/metrics"]
    );
}

/// Asserts that a frontend with no `docker_gwbridge` address, and one whose metrics port does
/// not answer, are each noted in the reading and left out of it, and the reading still ends 0.
/// It catches a snapshot that stops at the first frontend it cannot read.
#[test]
fn a_frontend_it_cannot_read_is_noted_and_left_out() {
    let rig = Rig::new("unread");
    rig.set(
        "ps-kbf-frontend_server.txt",
        &format!("cccccccccccccccc\n{FE_B}\n{FE_A}\n"),
    );
    fs::remove_file(rig.dir.join("metrics-192.0.2.7:8982")).unwrap();
    let file = rig.path("out/r.txt");
    let ran = rig.run(&["snapshot", &file]);
    assert_eq!(ran.status, 0, "{ran:?}");
    let text = fs::read_to_string(&file).unwrap();
    assert!(text.contains(&format!("fe {FE_A}\n")), "{text}");
    assert!(!text.contains(&format!("fe {FE_B}")), "{text}");
    assert!(
        text.contains(&format!("# frontend {FE_B} did not answer\n")),
        "{text}"
    );
    assert!(
        text.contains("# no docker_gwbridge address for frontend cccccccccccccccc\n"),
        "{text}"
    );
}

/// Asserts the numbers of the worked window, exactly: the window, the server, the frontends,
/// and the count, rate, p50, p90 and mean of all forms and of each form. It catches a quantile
/// that is not linear inside its bucket, counts that are not differences of the two readings,
/// or forms summed into the wrong line.
#[test]
fn compute_gives_the_quantiles_of_the_window() {
    let rig = Rig::new("worked");
    let (first, second) = worked();
    let ran = rig.compute(&first, &second);
    assert_eq!(ran.status, 0, "{ran:?}");
    assert_eq!(ran.stdout.lines().collect::<Vec<_>>(), WORKED);
    assert_eq!(ran.stderr, "");
}

/// Asserts that a quantile past the last finite bucket is printed as `>` that bound, not as a
/// number. It catches a script that reads +Inf as a bound.
#[test]
fn a_quantile_past_the_last_bucket_is_a_lower_bound() {
    let rig = Rig::new("inf");
    let first = reading(1000, JS_LINE, &[(FE_A, metrics("provider", [0; 5], "0"))]);
    let second = reading(
        1600,
        JS_LINE,
        &[(FE_A, metrics("provider", [0, 0, 0, 2, 10], "4.2"))],
    );
    let ran = rig.compute(&first, &second);
    assert_eq!(ran.status, 0, "{ran:?}");
    assert_eq!(
        ran.stdout.lines().nth(3).unwrap(),
        "rpc: count 10 (0.02/s), p50 > 100.0 ms, p90 > 100.0 ms, mean 420.0 ms"
    );
}

/// Asserts that a frontend whose counts went down (it restarted) and one in a single reading are
/// left out and named, and the window is the kept frontend's alone. It catches a script that
/// adds a negative difference, or counts a frontend that one reading lacks as all new submits.
#[test]
fn a_restarted_or_missing_frontend_is_left_out() {
    let rig = Rig::new("dropped");
    let first = reading(
        1000,
        JS_LINE,
        &[
            (FE_A, metrics("provider", [0, 10, 20, 20, 20], "0.5")),
            (FE_B, metrics("manifests", [5, 5, 5, 5, 5], "0.02")),
        ],
    );
    let second = reading(
        2800,
        JS_LINE,
        &[
            (FE_A, metrics("provider", [0, 40, 80, 100, 100], "3.5")),
            (FE_B, metrics("manifests", [1, 1, 1, 1, 1], "0.005")),
            (
                "dddddddddddddddd",
                metrics("manifests", [9, 9, 9, 9, 9], "0.05"),
            ),
        ],
    );
    let ran = rig.compute(&first, &second);
    assert_eq!(ran.status, 0, "{ran:?}");
    let lines: Vec<&str> = ran.stdout.lines().collect();
    assert_eq!(
        lines[2],
        "frontends: 1 kept, dropped: bbbbbbbbbbbb(restarted) dddddddddddd(one-reading)"
    );
    assert_eq!(
        lines[3],
        "rpc: count 80 (0.04/s), p50 33.3 ms, p90 80.0 ms, mean 37.5 ms"
    );
}

/// Asserts that a window over which the js-server containers changed (another version, another
/// start time, another container, or none) is refused with exit 1 and both lists on stderr, and
/// no numbers. It catches a window that spans a deploy and so measures two servers.
#[test]
fn a_window_over_a_server_change_is_refused() {
    for after in [
        "js js1 0.1.421 2026-11-26T20:00:00Z",
        "js js1 0.1.420 2026-11-26T20:10:00Z",
        "js js2 0.1.420 2026-11-26T20:00:00Z",
        "# no js-server",
    ] {
        let rig = Rig::new("changed");
        let (first, second) = worked();
        let second = second.replace(JS_LINE, after);
        let ran = rig.compute(&first, &second);
        assert_eq!(ran.status, 1, "{after} {ran:?}");
        assert_eq!(ran.stdout, "", "{after}");
        assert!(
            ran.stderr.starts_with(
                "submit_rpc.sh: the js-server containers changed during the window (a deploy or a restart):\n"
            ),
            "{after} {ran:?}"
        );
    }
}

/// Asserts that a reading with no js-server container at either end is refused: the window
/// cannot say which server it measured.
#[test]
fn a_window_with_no_server_is_refused() {
    let rig = Rig::new("no-server");
    let (first, second) = worked();
    let ran = rig.compute(
        &first.replace(JS_LINE, "# none"),
        &second.replace(JS_LINE, "# none"),
    );
    assert_eq!(ran.status, 1, "{ran:?}");
    assert_eq!(ran.stdout, "");
    assert_eq!(
        ran.stderr,
        "submit_rpc.sh: no js-server container in either reading: the window cannot say which server it measured\n"
    );
}

/// Asserts that a frontend whose `rpc` lines carry their labels in another order (`step` after
/// `form`) is read and counted the same. It catches a filter that needs `step` to come first.
#[test]
fn labels_in_another_order_are_read() {
    let rig = Rig::new("label-order");
    let swap = |text: String| {
        text.replace(
            "{step=\"rpc\",form=\"provider\"",
            "{form=\"provider\",step=\"rpc\"",
        )
    };
    rig.set(
        "metrics-192.0.2.4:8982",
        &swap(metrics("provider", [0, 10, 20, 20, 20], "0.5")),
    );
    let file = rig.path("out/r.txt");
    let ran = rig.run(&["snapshot", &file]);
    assert_eq!(ran.status, 0, "{ran:?}");
    let text = fs::read_to_string(&file).unwrap();
    assert_eq!(
        text.lines()
            .filter(|l| l.contains("form=\"provider\",step=\"rpc\""))
            .count(),
        7,
        "{text}"
    );
    let first = text.clone();
    let second = first
        .replace("# at at-1000 1000", "# at at-2800 2800")
        .replace("le=\"+Inf\"} 20", "le=\"+Inf\"} 21")
        .replace(
            "_sum{form=\"provider\",step=\"rpc\"} 0.5",
            "_sum{form=\"provider\",step=\"rpc\"} 0.7",
        );
    let ran = rig.compute(&first, &second);
    assert_eq!(ran.status, 0, "{ran:?}");
    assert_eq!(
        ran.stdout.lines().nth(3).unwrap(),
        "rpc: count 1 (0.00/s), p50 > 100.0 ms, p90 > 100.0 ms, mean 200.0 ms"
    );
}

/// Asserts that a window with no submit prints the count 0 and is refused with exit 1, and that
/// a second reading not later than the first is refused. It catches quantiles printed from an
/// empty histogram.
#[test]
fn an_empty_or_backward_window_is_refused() {
    let rig = Rig::new("empty");
    let (first, _) = worked();
    let later = first.replace("# at at-1000 1000", "# at at-1600 1600");
    let ran = rig.compute(&first, &later);
    assert_eq!(ran.status, 1, "{ran:?}");
    assert_eq!(ran.stdout.lines().last().unwrap(), "rpc: count 0");
    assert_eq!(ran.stderr, "submit_rpc.sh: no SubmitJob in the window\n");

    let ran = rig.compute(&first, &first);
    assert_eq!(ran.status, 1, "{ran:?}");
    assert_eq!(
        ran.stderr,
        "submit_rpc.sh: the second reading is not later than the first\n"
    );
}

/// Asserts that `measure` takes a reading, waits `--minutes` (30 by default) as one sleep, takes
/// a second reading, and prints and keeps the numbers of the window between them. It catches a
/// measure that computes before the wait, or waits a different time.
#[test]
fn measure_reads_waits_and_computes() {
    let rig = Rig::new("measure");
    // The second reading sees the frontends' later counts: the stand-in `curl` answers from the
    // files the test swaps once the clock moved, so the files are named by the clock here.
    install_script(
        &rig.dir.join("bin/curl"),
        "#!/bin/sh\n\
         printf '%s\\n' \"$*\" >> \"$FAKE_DIR/curl.log\"\n\
         for a in \"$@\"; do url=$a; done\n\
         hp=${url#http://}; hp=${hp%/metrics}\n\
         cat \"$FAKE_DIR/metrics-$(cat \"$FAKE_DIR/clock\")-$hp\"\n",
        "755",
    );
    rig.set(
        "metrics-1000-192.0.2.4:8982",
        &metrics("provider", [0, 10, 20, 20, 20], "0.5"),
    );
    rig.set(
        "metrics-1000-192.0.2.7:8982",
        &metrics("manifests", [0, 0, 0, 0, 0], "0"),
    );
    rig.set(
        "metrics-2800-192.0.2.4:8982",
        &metrics("provider", [0, 40, 80, 100, 100], "3.5"),
    );
    rig.set(
        "metrics-2800-192.0.2.7:8982",
        &metrics("manifests", [20, 20, 20, 20, 20], "0.1"),
    );
    let out = rig.path("out");
    let ran = rig.run(&["measure", "--out-dir", &out]);
    assert_eq!(ran.status, 0, "{ran:?}");
    assert_eq!(ran.sleep, ["1800"]);
    let lines: Vec<&str> = ran.stdout.lines().collect();
    assert_eq!(
        lines[0],
        format!("first reading: {out}/first.txt; waiting 30 minutes")
    );
    assert_eq!(lines[1..], WORKED);
    let (first, second) = worked();
    assert_eq!(
        fs::read_to_string(format!("{out}/first.txt")).unwrap(),
        first
    );
    assert_eq!(
        fs::read_to_string(format!("{out}/second.txt")).unwrap(),
        second
    );
    assert_eq!(
        fs::read_to_string(format!("{out}/result.txt")).unwrap(),
        format!("{}\n", WORKED.join("\n"))
    );
}

/// Asserts that `--minutes 5` waits 300 seconds, and that a refused window ends `measure` with
/// exit 1 (here: no submit in it).
#[test]
fn measure_waits_the_minutes_given_and_fails_with_its_window() {
    let rig = Rig::new("measure-5");
    let out = rig.path("out");
    let ran = rig.run(&["measure", "--out-dir", &out, "--minutes", "5"]);
    assert_eq!(ran.sleep, ["300"]);
    assert_eq!(ran.status, 1, "{ran:?}");
    assert_eq!(ran.stderr, "submit_rpc.sh: no SubmitJob in the window\n");
}

/// Asserts that bad input is refused with exit 2 and the usage before `docker` or `curl` runs:
/// no verb, an unknown verb, a snapshot without its file or with a flag of `measure`, a port or a
/// window out of range, `compute` without two readings or with one that does not exist,
/// `measure` without an out directory or with one that is absent, a flag without its value and
/// an unknown flag.
#[test]
fn bad_input_is_refused_before_anything_runs() {
    let rig = Rig::new("usage");
    let out = rig.path("out");
    let nope = rig.path("nope");
    let cases: Vec<(Vec<&str>, String)> = vec![
        (vec!["fetch"], "unknown verb: fetch".to_owned()),
        (vec!["snapshot"], "snapshot needs a FILE".to_owned()),
        (
            vec!["snapshot", "-", "--minutes", "5"],
            "snapshot does not take --minutes".to_owned(),
        ),
        (
            vec!["snapshot", "-", "--out-dir", "x"],
            "snapshot does not take --out-dir".to_owned(),
        ),
        (
            vec!["snapshot", "-", "--port", "0"],
            "--port must be 1-65535: 0".to_owned(),
        ),
        (
            vec!["snapshot", "-", "--port", "65536"],
            "--port must be 1-65535: 65536".to_owned(),
        ),
        (
            vec!["snapshot", "-", "--port", "80a"],
            "--port must be 1-65535: 80a".to_owned(),
        ),
        (
            vec!["snapshot", "-", "--js-service", ""],
            "--js-service must not be empty".to_owned(),
        ),
        (
            vec!["snapshot", "-", "--frontend-service", ""],
            "--frontend-service must not be empty".to_owned(),
        ),
        (
            vec!["snapshot", "-", "--port"],
            "--port needs a value".to_owned(),
        ),
        (
            vec!["snapshot", "-", "--verbose"],
            "unknown argument: --verbose".to_owned(),
        ),
        (
            vec!["compute", "a"],
            "compute needs FIRST and SECOND".to_owned(),
        ),
        (
            vec!["compute", &nope, &nope],
            format!("no such reading: {nope}"),
        ),
        (vec!["measure"], "--out-dir is required".to_owned()),
        (
            vec!["measure", "--out-dir", &nope],
            format!("--out-dir is not a directory: {nope}"),
        ),
        (
            vec!["measure", "--out-dir", &out, "--minutes", "0"],
            "--minutes must be a whole number from 1 to 9999: 0".to_owned(),
        ),
        (
            vec!["measure", "--out-dir", &out, "--minutes", "10000"],
            "--minutes must be a whole number from 1 to 9999: 10000".to_owned(),
        ),
    ];
    for (args, why) in cases {
        let ran = rig.run(&args);
        assert_eq!(ran.status, 2, "{args:?} {ran:?}");
        assert!(
            ran.stderr
                .starts_with(&format!("submit_rpc.sh: {why}\nusage: submit_rpc.sh ")),
            "{args:?} {ran:?}"
        );
        assert!(
            ran.docker.is_empty() && ran.curl.is_empty(),
            "{args:?} {ran:?}"
        );
    }
    for args in [&[][..], &["--help"][..]] {
        let ran = rig.run(args);
        assert_eq!(ran.status, 2, "{ran:?}");
        assert!(ran.stderr.starts_with("usage: submit_rpc.sh "), "{ran:?}");
    }
}
