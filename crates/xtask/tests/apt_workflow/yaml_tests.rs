//! Tests of the YAML reader ([`super::yaml`]): a plain scalar holding `: ` is refused with
//! the line GitHub refuses it on, ` #` is read as the start of a comment, as GitHub reads it,
//! and what the lints do not evaluate (a tag, a second document, an alias bomb) is refused.

use super::support::workflows;
use super::yaml::{self, MAX_NODES, Value};

/// A `run:` written as a plain scalar whose text holds `): `, which ends the scalar, so GitHub
/// refuses the file.
const ISSUE_RUN: &str = r#"run: echo "::notice::off (the variable is unset): no farm was told""#;

/// The command of [`ISSUE_RUN`], as the step would run it.
const ISSUE_COMMAND: &str = r#"echo "::notice::off (the variable is unset): no farm was told""#;

/// A workflow whose one step is `step` (one line, or several joined by `\n`, at the step's
/// indentation), and the 1-based line the step starts on.
fn with_step(step: &str) -> (String, usize) {
    let head = "name: t\non: push\njobs:\n  a:\n    steps:\n";
    let line = head.lines().count() + 1;
    let step = step.replace('\n', "\n        ");
    (format!("{head}      - {step}\n"), line)
}

/// The `run` of the one step of the workflow `doc` ([`with_step`]): its text, and whether it was
/// plain.
fn first_run(doc: &yaml::Node) -> (String, bool) {
    fn get<'a>(node: &'a yaml::Node, key: &str) -> &'a yaml::Node {
        match &node.value {
            Value::Map(entries) => entries
                .iter()
                .find(|(k, _)| k.as_str() == Some(key))
                .map(|(_, v)| v)
                .unwrap_or_else(|| panic!("no `{key}`")),
            _ => panic!("`{key}` is looked up in a node that is not a mapping"),
        }
    }
    let Value::Map(jobs) = &get(doc, "jobs").value else {
        panic!("`jobs` is not a mapping")
    };
    let Value::Seq(steps) = &get(&jobs[0].1, "steps").value else {
        panic!("`steps` is not a sequence")
    };
    match &get(&steps[0], "run").value {
        Value::Scalar { text, plain } => (text.clone(), *plain),
        _ => panic!("`run` is not a scalar"),
    }
}

/// The error of `yaml::load` on `text`, without the node.
fn refused(text: &str) -> String {
    yaml::load(text)
        .map(|_| ())
        .expect_err("the text is refused")
}

#[test]
fn every_workflow_of_the_repository_is_read_as_one_document() {
    // Catches: a workflow GitHub would refuse as YAML (the issue's `): `, a tag, a second
    // document), which every lint after this one would then skip instead of reading.
    for (file, text) in workflows() {
        assert!(yaml::load(&text).is_ok(), "{file}: {:?}", yaml::load(&text));
    }
}

#[test]
fn the_issue_run_as_a_plain_scalar_is_refused_on_its_own_line() {
    // Catches: a reader that accepts a run whose plain scalar holds `): `, which GitHub refuses
    // ("workflow file issue", no jobs), and one that refuses it without saying where.
    let (spoiled, line) = with_step(ISSUE_RUN);
    let column = "      - ".len() + ISSUE_RUN.find("): ").expect("the issue's `): `") + 2;
    assert_eq!(
        refused(&spoiled),
        format!(
            "line {line}: not valid YAML: mapping values are not allowed in this context \
             (column {column})"
        )
    );
}

#[test]
fn a_colon_space_on_a_later_line_of_a_plain_scalar_names_that_line() {
    // Catches: an error that names the line the scalar starts on, or the first line of the
    // file, when the `: ` is on a continuation line.
    let (spoiled, line) = with_step("run: echo one\n  two: three");
    assert_eq!(
        refused(&spoiled),
        format!(
            "line {}: not valid YAML: mapping values are not allowed in this context \
             (column 14)",
            line + 1
        )
    );
}

#[test]
fn a_colon_tab_in_a_plain_scalar_is_refused_on_its_line() {
    // Catches: a reader that accepts `:` followed by a tab, which ends a plain scalar as a
    // space does.
    let (spoiled, line) = with_step("run: echo a:\tb");
    let column = "      - run: echo a:\t".len() + 1;
    assert_eq!(
        refused(&spoiled),
        format!(
            "line {line}: not valid YAML: ':' must be followed by a valid YAML whitespace \
             (column {column})"
        )
    );
}

#[test]
fn an_error_on_the_first_line_of_the_file_names_line_one() {
    // Catches: a 0-based line, which would name line 0 for the first line of the file.
    assert_eq!(
        refused("on: a: b\n"),
        "line 1: not valid YAML: mapping values are not allowed in this context (column 6)"
    );
}

#[test]
fn an_unclosed_quote_names_the_line_it_opens_on() {
    // Catches: an error found only at the end of the text that names line 0, or a line
    // other than the one the quote opens on.
    assert_eq!(
        refused("name: t\non: 'push\n"),
        "line 2: not valid YAML: while scanning a quoted scalar, found unexpected end of \
         stream (column 5)"
    );
}

#[test]
fn the_issue_run_as_a_block_or_a_quoted_scalar_is_read_whole() {
    // Catches: a reader that refuses the forms GitHub accepts for the same command, or that
    // reads them as anything but the command.
    let quoted = format!("run: '{}'", ISSUE_COMMAND.replace('\'', "''"));
    let double = format!("run: \"{}\"", ISSUE_COMMAND.replace('"', "\\\""));
    for step in [format!("run: |\n  {ISSUE_COMMAND}"), quoted, double] {
        let (spoiled, _) = with_step(&step);
        let doc = yaml::load(&spoiled).expect("valid YAML");
        let (text, plain) = first_run(&doc);
        assert_eq!(text.trim_end(), ISSUE_COMMAND, "{step}");
        assert!(!plain, "{step}");
    }
}

#[test]
fn a_space_hash_in_a_plain_scalar_starts_a_comment_and_a_hash_alone_does_not() {
    // Catches: a reader that refuses ` #` (GitHub reads it as a comment and runs the rest),
    // keeps the comment in the command, or drops a `#` that no space comes before. The
    // issue's `::notice::` (colons with no space after) is kept.
    for (step, command) in [
        ("run: echo a #b", "echo a"),
        ("run: echo a\t#b", "echo a"),
        ("run: echo a#b", "echo a#b"),
        ("run: echo \"::notice::off\"", "echo \"::notice::off\""),
        ("run: echo a\n  #b", "echo a"),
    ] {
        let (spoiled, _) = with_step(step);
        let doc = yaml::load(&spoiled).expect("valid YAML");
        assert_eq!(first_run(&doc), (command.to_owned(), true), "{step:?}");
    }
}

#[test]
fn what_the_lints_do_not_evaluate_is_refused_not_read_some_other_way() {
    // Catches: a tag, a second document, a file with no document, an alias to a node that is not
    // finished and a mapping key with no value read as if they were plain data, which a lint
    // would then pass.
    assert_eq!(
        refused("a: !!str b\n"),
        "line 1: a YAML tag (`tag:yaml.org,2002:!str`) is not evaluated"
    );
    assert_eq!(
        refused("a: 1\n---\nb: 2\n"),
        "line 3: a second YAML document is not evaluated"
    );
    assert_eq!(refused(""), "line 0: the file holds no YAML document");
    assert!(refused("a: &x [*x]\n").contains("an alias names a node that is not finished"));
}

#[test]
fn an_alias_bomb_is_refused_before_it_is_expanded() {
    // Catches: a document that expands to more nodes than the lint may build, which would
    // exhaust memory in the test instead of failing it.
    let mut text = String::from("a0: &a0 [x, x, x, x, x, x, x, x, x, x]\n");
    for n in 1..8 {
        let previous = format!("*a{}", n - 1);
        let items = vec![previous; 10].join(", ");
        text.push_str(&format!("a{n}: &a{n} [{items}]\n"));
    }
    assert!(refused(&text).contains(&format!(
        "the document expands to more than {MAX_NODES} nodes"
    )));
}

#[test]
fn a_scalar_is_null_only_when_plain_and_spelled_so() {
    // Catches: `null`, `~` or an empty value read as the text "null" (a missing key read as
    // present), and a quoted `"null"` read as a null.
    let doc = yaml::load("a: null\nb: ~\nc:\nd: 'null'\ne: x\n").expect("valid YAML");
    let Value::Map(entries) = &doc.value else {
        panic!("a mapping")
    };
    let read: Vec<Option<&str>> = entries.iter().map(|(_, v)| v.as_str()).collect();
    assert_eq!(read, [None, None, None, Some("null"), Some("x")]);
    assert!(yaml::load("{}\n").expect("valid YAML").is_empty_map());
    assert!(!doc.is_empty_map());
}
