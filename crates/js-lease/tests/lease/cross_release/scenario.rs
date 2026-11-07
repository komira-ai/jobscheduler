// The file a release writes for a few leases that each book a whole host: a whole-node
// lease with the caller's labels and a validation run, and a `gpu-host` lease. The previous
// release wrote `written_by_previous_release.toml` by running its own acquire of these
// steps, with a time to live and a renewal that this release does not have (see
// `cross_release.rs`), so the two files differ only in each lease's `expires_at_ms`.

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
    table.acquire(labelled).unwrap();
    table.acquire(gpu_host_request(3600)).unwrap();
    drop(table);
    fs::read_to_string(scratch.file()).unwrap()
}
