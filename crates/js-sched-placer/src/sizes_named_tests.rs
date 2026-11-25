//! The class a client names in the job's own field (`JobSpec::size_class`): it is the class
//! as sent, ahead of the `size-class` label, in the class's name, its history key and the
//! placement log, and a label of the same text is the same class.

use std::collections::BTreeMap;

use js_jobs::{JobId, JobSpec, NewJob};
use js_types::{Labels, SizeClassName};

use super::fallback::{Lines, process_sink};
use super::{
    GIB, JobSizes, SIZE_CLASS_LABEL, Tables, allocation, booked, class_name, finish_at, needs,
    of_class, pass, placer, queued, spec_of,
};

/// A build target's full name as Buck2 writes it with the kind of its step, padded to `len`
/// bytes with `pad`.
fn target(len: usize, pad: char) -> String {
    let name = format!("komira//{}json:json mojo_compile build ", "src/".repeat(40));
    let padded = format!("{name}{}", pad.to_string().repeat(len - name.len()));
    assert_eq!(padded.len(), len);
    padded
}

/// The spec of a job that books nothing and names `class` in its field.
fn named(class: &str) -> JobSpec {
    let mut spec = spec_of(needs(0, 0), 600);
    spec.size_class = Some(SizeClassName::parse(class).unwrap());
    spec
}

/// Asserts that a 200-byte class is the class's name byte for byte, that the field wins over
/// a label naming another class, that a label alone still names the class, and that two
/// classes that differ only in their last byte are two keys. Catches a class lowered, cut or
/// hashed into its name, the label read ahead of the field, and a key that keeps only part
/// of a long class (as the 63-byte label cut did).
#[test]
fn the_field_is_the_class_as_sent_ahead_of_the_label() {
    let long = target(200, 'a');
    let job = queued("field", named(&long));
    assert_eq!(class_name(&job), long);
    let mut both = of_class("label-class", needs(0, 0));
    both.size_class = Some(SizeClassName::parse(&long).unwrap());
    let both = queued("both", both);
    assert_eq!(class_name(&both), long);
    assert_eq!(JobSizes::key_of(&both), JobSizes::key_of(&job));
    let label = queued("label", of_class("label-class", needs(0, 0)));
    assert_eq!(class_name(&label), "label-class");
    assert_ne!(JobSizes::key_of(&label), JobSizes::key_of(&job));
    let other = queued("other", named(&target(200, 'b')));
    assert_ne!(JobSizes::key_of(&other), JobSizes::key_of(&job));
}

/// Asserts that a field and a label that hold the same text are one class: the runs a job
/// that named it in its label taught size a job that names it in the field, so a client that
/// moves a short class from the label to the field keeps its history. Catches the field
/// hashed into a key of its own, which would send every such class back to the cold size.
#[test]
fn a_field_and_a_label_of_the_same_text_share_one_history() {
    let tables = Tables::new();
    let placer = placer();
    let submit = |id: &str, spec: JobSpec| {
        let new = NewJob {
            id: Some(JobId::parse(id).unwrap()),
            spec,
            priority: 0,
            client: None,
        };
        tables.jobs.insert_queued(new).unwrap();
    };
    submit("by-label", of_class("compile", needs(0, 0)));
    assert_eq!(pass(&placer, &tables).len(), 1);
    finish_at(&tables, "by-label", GIB);
    submit("by-field", named("compile"));
    assert_eq!(pass(&placer, &tables).len(), 1);
    let learned = (6 * GIB / 5 + 128 * (1 << 20)).div_ceil(256 << 20) * (256 << 20);
    assert_eq!(allocation(&tables, "by-field"), booked(3000, learned));
}

/// Asserts that the placement of a job whose 200-byte class the history sized logs that class
/// byte for byte after `class=`: the exact line, slashes, colon and spaces and all. Catches a
/// log that names a cut, hashed or quoted class, so that nobody can read which target a
/// booking belongs to.
#[test]
fn the_placement_log_names_a_200_byte_class_byte_for_byte() {
    process_sink();
    let lines = Lines::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(lines.clone())
        .with_ansi(false)
        .without_time()
        .with_target(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let tables = Tables::new();
    let placer = placer();
    let class = target(200, ' ');
    let class = format!("{}x", &class[..199]);
    let new = NewJob {
        id: Some(JobId::parse("long").unwrap()),
        spec: named(&class),
        priority: 0,
        client: None,
    };
    tables.jobs.insert_queued(new).unwrap();
    assert_eq!(pass(&placer, &tables).len(), 1);
    let logged: Vec<String> = lines
        .take()
        .lines()
        .filter(|line| line.contains("a job the history sized was placed"))
        .map(str::to_owned)
        .collect();
    let expected = format!(
        " INFO a job the history sized was placed job=long host=a class={class} \
         sized_from=\"cold\""
    );
    assert_eq!(logged, [expected]);
}

/// Asserts that a job's labels other than `size-class` do not name its class when the field
/// is empty and the label absent: the class is then the image and command's. Catches the
/// field's absence read as an empty class every such job would share.
#[test]
fn no_field_and_no_label_leaves_the_class_to_the_image_and_command() {
    let mut spec = spec_of(needs(0, 0), 600);
    let labels = BTreeMap::from([("team".to_owned(), "infra".to_owned())]);
    spec.labels = Labels::from_caller(&labels).unwrap();
    let job = queued("plain", spec);
    assert!(class_name(&job).starts_with("run-"), "{}", class_name(&job));
    assert!(job.spec.labels.get(SIZE_CLASS_LABEL).is_none());
}
