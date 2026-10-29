//! The `[[option]]` table.

use js_elastic::GpuClass;
use js_provider_lambdalabs::{OptionRow, OptionTable, OptionsError};

const GOOD: &str = r#"
unrelated = "left to another reader"

[provider]
anything = 1

[[option]]
instance_type = "gpu_1x_sample"
gpu_class = "nvidia-a10"
gpus = 1

[[option]]
instance_type = "gpu_8x_example"
gpu_class = "nvidia-a100-sxm4-80gb"
gpus = 8
"#;

fn row(extra: &str) -> String {
    format!("[[option]]\n{extra}\n")
}

fn refused(text: &str) -> OptionsError {
    OptionTable::parse(text).unwrap_err()
}

#[test]
fn rows_are_read_in_file_order_and_other_keys_are_left_alone() {
    let table = OptionTable::parse(GOOD).unwrap();
    assert_eq!(
        table.rows(),
        [
            OptionRow {
                instance_type: "gpu_1x_sample".to_owned(),
                gpu_class: GpuClass::parse("nvidia-a10").unwrap(),
                gpus: 1,
            },
            OptionRow {
                instance_type: "gpu_8x_example".to_owned(),
                gpu_class: GpuClass::parse("nvidia-a100-sxm4-80gb").unwrap(),
                gpus: 8,
            },
        ]
    );
    assert_eq!(table.get("gpu_8x_example").unwrap().gpus, 8);
    assert!(table.get("gpu_2x_other").is_none());
    assert_eq!(
        table.rows()[1].gpu_class.to_string(),
        "nvidia-a100-sxm4-80gb"
    );
    assert_eq!(table.rows()[1].gpu_class.as_str(), "nvidia-a100-sxm4-80gb");
}

#[test]
fn a_document_with_no_row_is_refused() {
    assert_eq!(refused(""), OptionsError::NoRows);
    assert_eq!(refused("[provider]\nx = 1\n"), OptionsError::NoRows);
}

#[test]
fn text_that_is_not_the_shape_is_a_syntax_error() {
    for text in [
        "[[option",
        // A misspelt key must not pass silently.
        &row("instance_type = \"gpu_1x_a\"\ngpu_clas = \"x\"\ngpus = 1"),
        &row("instance_type = \"gpu_1x_a\"\ngpu_class = \"x\""),
        &row("instance_type = \"gpu_1x_a\"\ngpu_class = \"x\"\ngpus = \"1\""),
        &row("instance_type = \"gpu_1x_a\"\ngpu_class = \"x\"\ngpus = -1"),
        &row("instance_type = 5\ngpu_class = \"x\"\ngpus = 1"),
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
fn a_row_with_zero_gpus_is_refused_and_named() {
    let text = format!(
        "{GOOD}\n{}",
        row("instance_type = \"gpu_0x\"\ngpu_class = \"x\"\ngpus = 0")
    );
    assert!(matches!(refused(&text), OptionsError::Row { row: 3, .. }));
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
        let text = row(&format!(
            "instance_type = \"{bad}\"\ngpu_class = \"x\"\ngpus = 1"
        ));
        assert!(
            matches!(refused(&text), OptionsError::Row { row: 1, .. }),
            "{bad:?}"
        );
    }
    let edge = "a".repeat(64);
    assert!(
        OptionTable::parse(&row(&format!(
            "instance_type = \"{edge}\"\ngpu_class = \"x\"\ngpus = 1"
        )))
        .is_ok()
    );
}

#[test]
fn a_row_whose_gpu_class_is_not_in_the_agents_spelling_is_refused_and_named() {
    let text = row("instance_type = \"gpu_1x_a\"\ngpu_class = \"NVIDIA A10\"\ngpus = 1");
    let OptionsError::Row { why, .. } = refused(&text) else {
        panic!("not a row error");
    };
    assert!(why.starts_with("gpu_class "), "{why}");
}

#[test]
fn an_instance_type_listed_twice_is_refused() {
    let one = "instance_type = \"gpu_1x_a\"\ngpu_class = \"x\"\ngpus = 1";
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
