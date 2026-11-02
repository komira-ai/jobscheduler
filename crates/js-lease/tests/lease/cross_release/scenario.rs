// The file a release writes for a few leases that each book a whole host: a whole-node
// lease with the caller's labels and a validation run, and a `gpu-host` lease renewed ten
// seconds in. This file is the same in this release and in the one before it, which wrote
// `written_by_previous_release.toml` by running it (see `cross_release.rs`), so the two
// releases are compared over the same steps.

/// Runs the leases over the fake provider into the file of `scratch`, and returns the text
/// the table left there.
fn whole_hosts_written(scratch: &Scratch) -> String {
    let clock = clock();
    let mut table = open(fake(), &clock, scratch);
    let labelled = Request {
        labels: BTreeMap::from([("team".to_owned(), "infra".to_owned())]),
        validation_run: Some("v-1".to_owned()),
        ..request(3600)
    };
    table.acquire(labelled, None).unwrap();
    let renewed = table.acquire(gpu_host_request(3600), Some(secs(30))).unwrap();
    clock.advance(secs(10));
    table.renew(&renewed.id, None).unwrap();
    drop(table);
    fs::read_to_string(scratch.file()).unwrap()
}
