//! The tests of the Debian package rule (`debs.rs`).

use std::path::Path;

use crate::debs::{APT_EXEMPT, DebsSite, lint_debs};
use crate::{read, workflows};

/// Every set of pinned packages the repository's workflows install: `<file>`, `<job>`, `<set>`.
const EXPECTED: [(&str, &str, &str); 3] = [
    ("artifacts.yml", "build", "musl-tools"),
    ("js-agent-deb.yml", "build", "musl-tools"),
    ("js-agent-quota-tests.yml", "build", "musl-tools"),
];

/// Asserts that every workflow of the repository keeps the rule, that it installs exactly the
/// sets of `EXPECTED`, each of which has its pins file, and that apt runs only in the exempt
/// workflow, which exists.
#[test]
fn every_workflow_keeps_the_debs_rule() {
    let mut sites = Vec::new();
    for (file, text) in workflows() {
        let found = lint_debs(&file, &text).unwrap_or_else(|e| panic!("{e:#?}"));
        sites.extend(
            found
                .into_iter()
                .map(|DebsSite { job, set }| (file.clone(), job, set)),
        );
    }
    let expected: Vec<(String, String, String)> = EXPECTED
        .iter()
        .map(|(f, j, s)| ((*f).to_owned(), (*j).to_owned(), (*s).to_owned()))
        .collect();
    assert_eq!(sites, expected);
    for (_, _, set) in EXPECTED {
        let pins = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../.github/debs")
            .join(format!("{set}.toml"));
        assert!(pins.is_file(), "{pins:?}");
    }
    for file in APT_EXEMPT {
        assert!(read(file).contains("apt-get"), "{file}");
    }
}

/// A job that installs one pinned set in the shape the rule asks for, under no condition.
const GOOD: &str = r#"name: t
on: push
jobs:
  build:
    runs-on: ubuntu-24.04
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
      - uses: actions/cache/restore@55cc8345863c7cc4c66a329aec7e433d2d1c52a9 # v6.1.0
        id: set-debs
        with:
          path: ${{ runner.temp }}/debs/set-1
          key: debs-set-1-${{ runner.arch }}-${{ hashFiles('.github/debs/set-1.toml') }}
      - name: Fetch the set
        run: cargo xtask debs --pins .github/debs/set-1.toml --dir "$RUNNER_TEMP/debs/set-1"
      - uses: actions/cache/save@55cc8345863c7cc4c66a329aec7e433d2d1c52a9 # v6.1.0
        if: steps.set-debs.outputs.cache-hit != 'true'
        with:
          path: ${{ runner.temp }}/debs/set-1
          key: ${{ steps.set-debs.outputs.cache-primary-key }}
      - name: Install the set
        run: sudo dpkg --install "$RUNNER_TEMP"/debs/set-1/*.deb
      - run: musl-gcc --version
"#;

/// `GOOD` with `from` replaced by `to`, which must occur in it once.
fn with(from: &str, to: &str) -> String {
    assert_eq!(GOOD.matches(from).count(), 1, "{from}");
    GOOD.replacen(from, to, 1)
}

/// The errors of `text` as the workflow `file` (none when it passes).
fn errors_of(file: &str, text: &str) -> Vec<String> {
    lint_debs(file, text).err().unwrap_or_default()
}

/// The line of the step whose `-` line is `dash` in `text`.
fn line_of(text: &str, dash: &str) -> usize {
    1 + text.lines().position(|l| l == dash).expect(dash)
}

/// Asserts that `text` fails with exactly one error, on the line of the step `dash`, that
/// contains `says`.
#[track_caller]
fn refused_at(text: &str, dash: &str, says: &str) {
    let errors = errors_of("t.yml", text);
    assert_eq!(errors.len(), 1, "{says}: {errors:#?}");
    let line = line_of(text, dash);
    assert!(
        errors[0].starts_with(&format!("t.yml:{line}: build: ")) && errors[0].contains(says),
        "{says}: {errors:#?}"
    );
}

const DEBS: &str = "      - name: Fetch the set";

/// Asserts that the shape passes, unconditioned and with the four steps under one condition
/// (the save's joined to its miss), with the condition written in `${{ }}` or not.
#[test]
fn the_four_steps_pass() {
    assert_eq!(
        lint_debs("t.yml", GOOD),
        Ok(vec![DebsSite {
            job: "build".to_owned(),
            set: "set-1".to_owned(),
        }])
    );
    let cond = "${{ endsWith(matrix.target, '-linux-musl') }}";
    let guarded = GOOD
        .replacen(
            "        id: set-debs\n",
            &format!("        if: {cond}\n        id: set-debs\n"),
            1,
        )
        .replacen(
            "        run: cargo xtask debs",
            "        if: endsWith(matrix.target, '-linux-musl')\n        run: cargo xtask debs",
            1,
        )
        .replacen(
            "        if: steps.set-debs.outputs.cache-hit != 'true'",
            "        if: ${{ endsWith(matrix.target, '-linux-musl') && steps.set-debs.outputs.cache-hit != 'true' }}",
            1,
        )
        .replacen(
            "        run: sudo dpkg",
            &format!("        if: {cond}\n        run: sudo dpkg"),
            1,
        );
    assert_eq!(errors_of("t.yml", &guarded), Vec::<String>::new());
}

/// Asserts that apt, in any spelling a step runs it, is refused outside the exempt workflow,
/// which may run it; that a Canonical host is refused everywhere; and that a dpkg install
/// other than the fourth debs step is refused.
#[test]
fn apt_a_canonical_host_and_a_loose_dpkg_install_are_refused() {
    let last = "      - run: musl-gcc --version";
    for (run, says) in [
        ("sudo apt-get update", "runs apt-get"),
        ("apt-get install -y musl-tools", "runs apt-get"),
        ("sudo apt install crun", "runs apt"),
        ("sudo aptitude install crun", "runs aptitude"),
        (
            "curl -fsSO https://archive.ubuntu.com/ubuntu/pool/a.deb",
            "names a Canonical host",
        ),
        (
            "curl -fsSO https://launchpad.net/a.deb",
            "names a Canonical host",
        ),
        ("sudo dpkg -i a.deb", "a dpkg install that is not"),
        ("dpkg --install a.deb", "a dpkg install that is not"),
    ] {
        let step = format!("      - run: {run}");
        refused_at(&with(last, &step), &step, says);
    }
    // In a block, on a later line, it is refused the same.
    let block = with(
        last,
        "      - run: |\n          set -e\n          sudo apt-get install -y crun",
    );
    refused_at(&block, "      - run: |", "runs apt-get");
    // The exempt workflow may run apt, and not name a Canonical host.
    let apt = with(last, "      - run: sudo apt-get install -y ./*.deb");
    assert_eq!(errors_of(APT_EXEMPT[0], &apt), Vec::<String>::new());
    let host = with(
        last,
        "      - run: curl https://cloud-images.ubuntu.com/a.img",
    );
    assert_eq!(errors_of(APT_EXEMPT[0], &host).len(), 1);
    // What only mentions apt or dpkg is not refused.
    for run in [
        "tool/xtask channels apt read --suite testing",
        "dpkg-query --show crun",
        "dpkg-deb --info a.deb",
    ] {
        let text = with(last, &format!("      - run: {run}"));
        assert_eq!(errors_of("t.yml", &text), Vec::<String>::new(), "{run}");
    }
}

/// Asserts that a debs step of another shape is refused: chained, with other flags or words,
/// a pins file outside `.github/debs/` or of another name, or a `--dir` of another set.
#[test]
fn a_debs_step_of_another_shape_is_refused() {
    let run = "cargo xtask debs --pins .github/debs/set-1.toml --dir \"$RUNNER_TEMP/debs/set-1\"";
    for (to, says) in [
        (format!("{run} && true"), "not one command"),
        (format!("{run} > log"), "not one command"),
        (format!("{run} --x"), "is not `cargo xtask debs"),
        (run.replace("--pins", "--pin"), "is not `cargo xtask debs"),
        (
            run.replace(".github/debs/set-1.toml", "debs/set-1.toml"),
            "the --pins \"debs/set-1.toml\" is not .github/debs/<set>.toml",
        ),
        (
            run.replace(".github/debs/set-1.toml", ".github/debs/Set.toml"),
            "is not .github/debs/<set>.toml",
        ),
        (
            run.replace(".github/debs/set-1.toml", ".github/debs/.toml"),
            "is not .github/debs/<set>.toml",
        ),
        (
            run.replace("$RUNNER_TEMP/debs/set-1", "$RUNNER_TEMP/debs/other"),
            "the --dir \"$RUNNER_TEMP/debs/other\" is not \"$RUNNER_TEMP/debs/set-1\"",
        ),
    ] {
        refused_at(&with(run, &to), DEBS, says);
    }
    let block = with(
        &format!("        run: {run}"),
        &format!("        run: |\n          {run}\n          echo done"),
    );
    refused_at(&block, DEBS, "not one command");
}

/// Asserts that a restore of another shape is refused: absent, unpinned, the whole cache
/// action, under another condition than the debs step, without an id, with restore-keys, at
/// another path, or under a key without the pins file's hash or of another set.
#[test]
fn a_restore_of_another_shape_is_refused() {
    let restore = "      - uses: actions/cache/restore@55cc8345863c7cc4c66a329aec7e433d2d1c52a9 # v6.1.0\n        id: set-debs\n        with:\n          path: ${{ runner.temp }}/debs/set-1\n          key: debs-set-1-${{ runner.arch }}-${{ hashFiles('.github/debs/set-1.toml') }}\n";
    refused_at(&with(restore, ""), DEBS, "is not actions/cache/restore");
    let first = with(
        &format!(
            "      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1\n{restore}"
        ),
        "",
    );
    refused_at(&first, DEBS, "no actions/cache/restore step right before");
    let key = "debs-set-1-${{ runner.arch }}-${{ hashFiles('.github/debs/set-1.toml') }}";
    for (from, to, says) in [
        (
            "actions/cache/restore@55cc8345863c7cc4c66a329aec7e433d2d1c52a9",
            "actions/cache/restore@v6",
            "is not actions/cache/restore",
        ),
        (
            "actions/cache/restore@55cc8345863c7cc4c66a329aec7e433d2d1c52a9",
            "actions/cache@55cc8345863c7cc4c66a329aec7e433d2d1c52a9",
            "is not actions/cache/restore",
        ),
        (
            "        id: set-debs\n",
            "        if: github.event_name == 'push'\n        id: set-debs\n",
            "the restore's `if:` is Some(\"github.event_name == 'push'\"), not the debs step's None",
        ),
        ("        id: set-debs\n", "", "the restore has no `id:`"),
        (
            &format!("          key: {key}\n"),
            &format!("          key: {key}\n          restore-keys: debs-set-1-\n"),
            "not exactly path and key",
        ),
        (
            "          path: ${{ runner.temp }}/debs/set-1\n          key: debs",
            "          path: ${{ runner.temp }}/debs/other\n          key: debs",
            "the restore's path is",
        ),
        (key, "debs-set-1-${{ runner.arch }}", "the restore's key is"),
        (
            key,
            "debs-set-1-${{ runner.arch }}-${{ hashFiles('.github/debs/other.toml') }}",
            "the restore's key is",
        ),
        (
            key,
            "debs-set-1-${{ hashFiles('.github/debs/set-1.toml') }}",
            "the restore's key is",
        ),
    ] {
        refused_at(&with(from, to), DEBS, says);
    }
}

/// Asserts that a save of another shape is refused: absent, unpinned, unconditional, on
/// another step's hit, not joined to the debs step's condition, at another path or under
/// another key; and that the install after it must be the glob of the set's directory under
/// the same condition.
#[test]
fn a_save_or_an_install_of_another_shape_is_refused() {
    let save = "      - uses: actions/cache/save@55cc8345863c7cc4c66a329aec7e433d2d1c52a9 # v6.1.0\n        if: steps.set-debs.outputs.cache-hit != 'true'\n        with:\n          path: ${{ runner.temp }}/debs/set-1\n          key: ${{ steps.set-debs.outputs.cache-primary-key }}\n";
    let install = "      - name: Install the set\n        run: sudo dpkg --install \"$RUNNER_TEMP\"/debs/set-1/*.deb\n";
    let gone = with(save, "");
    let errors = errors_of("t.yml", &gone);
    assert_eq!(errors.len(), 2, "{errors:#?}");
    assert!(
        errors[0].contains("is not actions/cache/save"),
        "{errors:#?}"
    );
    assert!(
        errors[1].contains("a dpkg install that is not"),
        "{errors:#?}"
    );
    let last = GOOD.replacen(
        &format!("{save}{install}      - run: musl-gcc --version\n"),
        "",
        1,
    );
    refused_at(&last, DEBS, "no actions/cache/save step right after");
    let hit = "        if: steps.set-debs.outputs.cache-hit != 'true'\n";
    for (from, to, says) in [
        (
            "actions/cache/save@55cc8345863c7cc4c66a329aec7e433d2d1c52a9",
            "actions/cache/save@v6",
            "is not actions/cache/save",
        ),
        (hit, "", "the save's `if:` is None"),
        (hit, "        if: always()\n", "the save's `if:`"),
        (
            hit,
            "        if: steps.other.outputs.cache-hit != 'true'\n",
            "the save's `if:`",
        ),
        (
            "          path: ${{ runner.temp }}/debs/set-1\n          key: ${{ steps",
            "          path: ${{ runner.temp }}/debs/other\n          key: ${{ steps",
            "the save's path is",
        ),
        (
            "key: ${{ steps.set-debs.outputs.cache-primary-key }}",
            "key: ${{ steps.other.outputs.cache-primary-key }}",
            "the save's key is",
        ),
    ] {
        refused_at(&with(from, to), DEBS, says);
    }
    // The install: absent (the save last), another command, or under another condition.
    let no_install = GOOD.replacen(
        &format!("{install}      - run: musl-gcc --version\n"),
        "",
        1,
    );
    refused_at(
        &no_install,
        DEBS,
        "no dpkg install step right after the save",
    );
    let other = with(install, "      - run: musl-gcc --version\n");
    refused_at(&other, DEBS, "is not `sudo dpkg --install");
    let elsewhere = with("/debs/set-1/*.deb", "/debs/other/*.deb");
    refused_at(&elsewhere, DEBS, "is not `sudo dpkg --install");
    let guarded = with(
        "      - name: Install the set\n",
        "      - name: Install the set\n        if: always()\n",
    );
    refused_at(&guarded, DEBS, "under the debs step's `if:`");
}

/// Asserts that the repository's own workflows are refused with one line changed: apt back in
/// js-agent-deb.yml, artifacts.yml's restore no longer under its legs' condition, and the
/// quota tests' save run on every outcome.
#[test]
fn the_repository_workflows_with_one_line_changed_are_refused() {
    let one = |file: &str, from: &str, to: &str| {
        let text = read(file);
        assert_eq!(text.matches(from).count(), 1, "{file}: {from}");
        let changed = text.replacen(from, to, 1);
        assert!(lint_debs(file, &changed).is_err(), "{file}: {from} -> {to}");
    };
    one(
        "js-agent-deb.yml",
        "run: sudo dpkg --install \"$RUNNER_TEMP\"/debs/musl-tools/*.deb",
        "run: sudo apt-get install --yes --no-install-recommends musl-tools",
    );
    one(
        "artifacts.yml",
        "        if: ${{ endsWith(matrix.target, '-linux-musl') }}\n        uses: actions/cache/restore",
        "        uses: actions/cache/restore",
    );
    one(
        "js-agent-quota-tests.yml",
        "if: steps.musl-debs.outputs.cache-hit != 'true'",
        "if: always()",
    );
    one(
        "ci.yml",
        "      - run: dpkg-query --show crun uidmap\n",
        "      - run: sudo apt-get install -y --no-install-recommends crun uidmap\n",
    );
}
