//! The `[[option]]` table: the allow list of instance types.

use js_provider_lambdalabs::{OptionRow, OptionTable, OptionsError};

use crate::rig::warnings;

const GOOD: &str = r#"
unrelated = "left to another reader"

[provider]
anything = 1

[[option]]
instance_type = "gpu_1x_h100_sxm5"

[[option]]
instance_type = "gpu_8x_h100_sxm5"
"#;

fn row(extra: &str) -> String {
    format!("[[option]]\n{extra}\n")
}

fn refused(text: &str) -> OptionsError {
    OptionTable::parse(text).unwrap_err()
}

fn row_of(name: &str) -> OptionRow {
    OptionRow {
        instance_type: name.to_owned(),
    }
}

/// Asserts that the rows are read in file order, each the instance type alone, that a
/// type is found by name and another is not, and that keys outside `option` are left to
/// the file's other readers. Catches rows reordered or a table that refuses a file's
/// other keys.
#[test]
fn rows_are_read_in_file_order_and_other_keys_are_left_alone() {
    let (table, logged) = warnings(|| OptionTable::parse(GOOD).unwrap());
    assert_eq!(
        table.rows(),
        [row_of("gpu_1x_h100_sxm5"), row_of("gpu_8x_h100_sxm5")]
    );
    assert_eq!(
        table.get("gpu_8x_h100_sxm5"),
        Some(&row_of("gpu_8x_h100_sxm5"))
    );
    assert!(table.get("gpu_2x_other").is_none());
    assert_eq!(logged, "", "a row with no gpu field warns of nothing");
}

/// Asserts that a row written for an earlier release, with `gpu_class` and `gpus` (one or
/// both, of any value: a class the agent never spells, zero, a negative number, a string),
/// parses as the row without them, and that each such row logs one warning naming its
/// instance type while a row without them logs none. Catches an old file refused, a value
/// still checked, and a warning that does not say which row.
#[test]
fn an_old_row_parses_whatever_its_gpu_fields_hold_and_warns_once() {
    for extra in [
        "gpu_class = \"nvidia-a10\"\ngpus = 1",
        "gpu_class = \"NVIDIA A10\"",
        "gpu_class = \"\"\ngpus = 0",
        "gpus = -1",
        "gpus = \"1\"",
        "gpu_class = 3\ngpus = [1]",
    ] {
        let text = format!(
            "{}{}",
            row(&format!("instance_type = \"gpu_1x_a10\"\n{extra}")),
            row("instance_type = \"gpu_1x_a100\"")
        );
        let (table, logged) = warnings(|| OptionTable::parse(&text));
        assert_eq!(
            table.unwrap().rows(),
            [row_of("gpu_1x_a10"), row_of("gpu_1x_a100")],
            "{extra}"
        );
        let lines: Vec<&str> = logged.lines().collect();
        assert_eq!(lines.len(), 1, "{extra}: {logged}");
        assert!(
            lines[0].contains(
                "option row gpu_1x_a10: gpu_class and gpus are ignored; the GPU facts of an \
                 instance type come from js-provider-lambdalabs, so remove them from the row"
            ),
            "{logged}"
        );
        assert!(lines[0].contains("WARN"), "{logged}");
    }
}

/// Asserts that a table refused for a later row logs no warning for an earlier old-shape
/// row: the warnings are for a table that is used. Catches a warning logged for a file
/// the service then refuses.
#[test]
fn a_refused_table_warns_of_nothing() {
    let text = format!(
        "{}{}",
        row("instance_type = \"gpu_1x_a10\"\ngpus = 1"),
        row("instance_type = \"Bad\"")
    );
    let (result, logged) = warnings(|| OptionTable::parse(&text));
    assert!(matches!(result, Err(OptionsError::Row { row: 2, .. })));
    assert_eq!(logged, "");
}

#[test]
fn a_document_with_no_row_is_refused() {
    assert_eq!(refused(""), OptionsError::NoRows);
    assert_eq!(refused("[provider]\nx = 1\n"), OptionsError::NoRows);
}

/// Asserts that text not of the row's shape is a syntax error: broken TOML, a key no row
/// has (a misspelt `gpu_class` among them, and any other key beside the two old ones), a
/// row with no `instance_type`, an instance type that is not a string, and `option` that is
/// not an array of tables. Catches the allowance for the two old keys widened to any key.
#[test]
fn text_that_is_not_the_shape_is_a_syntax_error() {
    for text in [
        "[[option",
        // A misspelt key must not pass silently.
        &row("instance_type = \"gpu_1x_a\"\ngpu_clas = \"x\"\ngpus = 1"),
        &row("instance_type = \"gpu_1x_a\"\ncolour = \"red\""),
        &row("gpu_class = \"x\"\ngpus = 1"),
        &row("instance_type = 5"),
        "option = 3",
    ] {
        assert!(matches!(refused(text), OptionsError::Syntax(_)), "{text}");
    }
}

#[test]
fn a_syntax_error_quotes_a_bounded_amount_of_the_text() {
    let long = format!("{}[[option", " ".repeat(2000));
    let OptionsError::Syntax(why) = refused(&long) else {
        panic!("not a syntax error");
    };
    assert!(why.len() <= 400);
}

#[test]
fn an_instance_type_is_lower_case_letters_digits_and_underscores() {
    let long = "a".repeat(65);
    for bad in [
        "",
        "GPU_1x",
        "gpu-1x",
        "gpu 1x",
        "gpu.1x",
        "gpu_1x\u{e9}",
        &long,
    ] {
        let text = row(&format!("instance_type = \"{bad}\""));
        assert!(
            matches!(refused(&text), OptionsError::Row { row: 1, .. }),
            "{bad:?}"
        );
    }
    let edge = "a".repeat(64);
    assert!(OptionTable::parse(&row(&format!("instance_type = \"{edge}\""))).is_ok());
}

#[test]
fn an_instance_type_listed_twice_is_refused() {
    let one = "instance_type = \"gpu_1x_a\"";
    let text = format!("{}{}", row(one), row(one));
    assert_eq!(
        refused(&text),
        OptionsError::Duplicate("gpu_1x_a".to_owned())
    );
}

#[test]
fn every_error_reads_as_a_sentence_that_names_the_table() {
    for err in [
        OptionsError::NoRows,
        OptionsError::Syntax("x".to_owned()),
        OptionsError::Row {
            row: 2,
            why: "y".to_owned(),
        },
        OptionsError::Duplicate("z".to_owned()),
    ] {
        assert!(err.to_string().starts_with("option table"), "{err}");
    }
}
